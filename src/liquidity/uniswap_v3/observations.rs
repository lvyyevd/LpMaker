//! 同一链/池共享只读观察。WSS 负责区块与实时行情；余额、手续费仍读取确认块。
//! 缓存只在进程内使用，断线、重组及本地交易会失效，不能替代交易前核对。
use crate::{
    config::LiquidityConfig,
    domain::{LpPosition, PoolSnapshot},
    stream::Event,
};
use alloy::primitives::{Address, B256, U256, keccak256};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

const HEAD_AGE: Duration = Duration::from_secs(30);
const SNAPSHOT_AGE: Duration = Duration::from_secs(2);
const READ_AGE: Duration = Duration::from_secs(30);
type Words = HashMap<String, (Instant, Vec<U256>)>;

#[derive(Clone)]
pub struct Header {
    pub number: u64,
    pub hash: String,
    pub parent: String,
    pub time_ms: u64,
    seen: Instant,
}
impl Header {
    fn parse(v: &Value) -> Result<Self> {
        let hash = |key: &str| -> Result<String> {
            Ok(format!(
                "{:#x}",
                v[key]
                    .as_str()
                    .context("missing WS block hash")?
                    .parse::<B256>()?
            ))
        };
        let time_ms = super::rpc::hex_u64(&v["timestamp"])?
            .checked_mul(1000)
            .context("WS timestamp overflow")?;
        ensure!(
            time_ms > 0 && time_ms <= crate::now_ms().saturating_add(5000),
            "invalid WS block timestamp"
        );
        Ok(Self {
            number: super::rpc::hex_u64(&v["number"])?,
            hash: hash("hash")?,
            parent: hash("parentHash")?,
            time_ms,
            seen: Instant::now(),
        })
    }
}
#[derive(Clone)]
pub struct PositionObservation {
    pub snapshot: PoolSnapshot,
    pub rows: Vec<(LpPosition, String)>,
    pub observed_ms: u64,
    owner: Address,
    ids: Vec<(String, String)>,
    seen: Instant,
}
#[derive(Default)]
struct State {
    epoch: u64,
    connected: bool,
    head_ack: bool,
    headers: BTreeMap<u64, Header>,
    anchor: Option<Instant>,
    snapshot: Option<(Instant, PoolSnapshot)>,
    spacing: Option<i32>,
    words: Words,
    positions: Option<PositionObservation>,
    owners: BTreeSet<Address>,
    inventory_checked: HashMap<Address, (Instant, BTreeSet<String>)>,
    ids: BTreeSet<U256>,
    latest_swap: Option<Value>,
}
impl State {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.snapshot = None;
        self.words.clear();
        self.positions = None;
        self.inventory_checked.clear();
    }
    fn reset_feed(&mut self) {
        self.invalidate();
        self.headers.clear();
        self.anchor = None;
        self.latest_swap = None;
    }
}
pub struct Shared {
    state: Mutex<State>,
    pub snapshot_lock: AsyncMutex<()>,
    pub read_lock: AsyncMutex<()>,
    pub spacing_lock: AsyncMutex<()>,
}
impl Shared {
    pub fn for_pool(c: &LiquidityConfig) -> Arc<Self> {
        static ALL: OnceLock<Mutex<HashMap<B256, Weak<Shared>>>> = OnceLock::new();
        // 完整传输/市场身份隔离，不能跨链、跨池或从另一个数据源复用观察。
        let key = keccak256(
            json!([
                c.chain_id,
                c.pool.to_lowercase(),
                c.position_manager.to_lowercase(),
                c.base_token.to_lowercase(),
                c.quote_token.to_lowercase(),
                c.base_decimals,
                c.quote_decimals,
                c.fee,
                c.confirmations,
                c.rpc_url,
                c.ws_url
            ])
            .to_string(),
        );
        let mut all = ALL
            .get_or_init(Default::default)
            .lock()
            .expect("observation registry poisoned");
        all.retain(|_, v| v.strong_count() > 0);
        if let Some(shared) = all.get(&key).and_then(Weak::upgrade) {
            return shared;
        }
        let shared = Arc::new(Self {
            state: Mutex::new(State::default()),
            snapshot_lock: AsyncMutex::new(()),
            read_lock: AsyncMutex::new(()),
            spacing_lock: AsyncMutex::new(()),
        });
        all.insert(key, Arc::downgrade(&shared));
        shared
    }
    pub fn epoch(&self) -> u64 {
        self.state.lock().expect("observations poisoned").epoch
    }
    pub fn ensure_epoch(&self, epoch: u64) -> Result<()> {
        if self.epoch() != epoch {
            return Err(crate::runtime::ReadUnavailable(
                "WSS connection/chain/inventory changed during observation; refresh before acting"
                    .into(),
            )
            .into());
        }
        Ok(())
    }
    pub fn invalidate(&self) {
        self.state
            .lock()
            .expect("observations poisoned")
            .invalidate();
    }
    pub fn disconnect(&self) {
        let mut state = self.state.lock().expect("observations poisoned");
        state.reset_feed();
        state.connected = false;
        state.head_ack = false;
    }
    pub fn reject_feed(&self) {
        self.state
            .lock()
            .expect("observations poisoned")
            .reset_feed();
    }
    pub fn spacing(&self) -> Option<i32> {
        self.state.lock().expect("observations poisoned").spacing
    }
    pub fn set_spacing(&self, spacing: i32) {
        self.state.lock().expect("observations poisoned").spacing = Some(spacing);
    }
    pub fn snapshot(&self) -> Option<PoolSnapshot> {
        let state = self.state.lock().expect("observations poisoned");
        state
            .snapshot
            .as_ref()
            .filter(|(t, _)| t.elapsed() < SNAPSHOT_AGE)
            .map(|(_, v)| v.clone())
    }
    pub fn save_snapshot(&self, epoch: u64, snapshot: &PoolSnapshot) {
        let mut state = self.state.lock().expect("observations poisoned");
        if state.epoch == epoch {
            if state
                .snapshot
                .as_ref()
                .is_some_and(|(_, old)| old.block > snapshot.block)
            {
                return;
            }
            state.snapshot = Some((Instant::now(), snapshot.clone()));
        }
    }
    pub fn confirmed_header(&self, confirmations: u64) -> Option<(Header, bool)> {
        let state = self.state.lock().expect("observations poisoned");
        if !state.connected || !state.head_ack {
            return None;
        }
        let tip = state.headers.last_key_value()?.1;
        if tip.seen.elapsed() > HEAD_AGE {
            return None;
        }
        let header = state
            .headers
            .get(&tip.number.checked_sub(confirmations)?)?
            .clone();
        let anchor_needed = state
            .anchor
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
        Some((header, anchor_needed))
    }
    pub fn anchored(&self, epoch: u64) {
        let mut state = self.state.lock().expect("observations poisoned");
        if state.epoch == epoch {
            state.anchor = Some(Instant::now());
        }
    }
    pub fn canonical_hash(&self, number: u64) -> Option<String> {
        let state = self.state.lock().expect("observations poisoned");
        if !state.connected
            || !state.head_ack
            || state
                .anchor
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
            || state.headers.last_key_value()?.1.seen.elapsed() > HEAD_AGE
        {
            return None;
        }
        Some(state.headers.get(&number)?.hash.clone())
    }
    pub fn words(&self, key: &str) -> Option<Vec<U256>> {
        self.state
            .lock()
            .expect("observations poisoned")
            .words
            .get(key)
            .filter(|(t, _)| t.elapsed() < READ_AGE)
            .map(|(_, v)| v.clone())
    }
    pub fn save_words(&self, epoch: u64, key: String, words: Vec<U256>) {
        let mut state = self.state.lock().expect("observations poisoned");
        if state.epoch != epoch {
            return;
        }
        state.words.retain(|_, (t, _)| t.elapsed() < READ_AGE);
        if state.words.len() >= 256 {
            state.words.clear();
        }
        state.words.insert(key, (Instant::now(), words));
    }
    pub fn watch_owner(&self, owner: Address) {
        self.state
            .lock()
            .expect("observations poisoned")
            .owners
            .insert(owner);
    }
    pub fn inventory_recent(&self, owner: Address, ids: &[(String, String)]) -> bool {
        self.state
            .lock()
            .expect("observations poisoned")
            .inventory_checked
            .get(&owner)
            .is_some_and(|(t, checked)| {
                t.elapsed() < Duration::from_secs(60)
                    && *checked == ids.iter().map(|(_, id)| id.clone()).collect()
            })
    }
    pub fn inventory_checked(&self, epoch: u64, owner: Address, ids: &[String]) {
        let mut state = self.state.lock().expect("observations poisoned");
        if state.epoch == epoch {
            state
                .inventory_checked
                .insert(owner, (Instant::now(), ids.iter().cloned().collect()));
        }
    }
    pub fn save_positions(
        &self,
        epoch: u64,
        owner: Address,
        ids: &[(String, String)],
        snapshot: &PoolSnapshot,
        rows: &[(LpPosition, String)],
    ) {
        let mut state = self.state.lock().expect("observations poisoned");
        if state.epoch != epoch {
            return;
        }
        if state
            .positions
            .as_ref()
            .is_some_and(|old| old.owner == owner && old.snapshot.block > snapshot.block)
        {
            return;
        }
        state.owners.insert(owner);
        state.ids = ids
            .iter()
            .filter_map(|(_, id)| U256::from_str_radix(id, 10).ok())
            .collect();
        let mut ids = ids.to_vec();
        ids.sort();
        state.positions = Some(PositionObservation {
            snapshot: snapshot.clone(),
            rows: rows.to_vec(),
            observed_ms: crate::now_ms(),
            owner,
            ids,
            seen: Instant::now(),
        });
    }
    pub fn positions(
        &self,
        owner: Address,
        ids: &[(String, String)],
        max_age: Duration,
    ) -> Option<PositionObservation> {
        let mut ids = ids.to_vec();
        ids.sort();
        self.state
            .lock()
            .expect("observations poisoned")
            .positions
            .as_ref()
            .filter(|v| v.owner == owner && v.ids == ids && v.seen.elapsed() < max_age)
            .cloned()
    }
    pub fn latest_swap(&self) -> Option<Value> {
        self.state
            .lock()
            .expect("observations poisoned")
            .latest_swap
            .clone()
    }

    /// 来自现有订阅连接；不打开第二条 socket。未确认 Swap 仅作实时展示。
    pub fn on_event(&self, event: &Event, c: &LiquidityConfig) -> Result<()> {
        let mut state = self.state.lock().expect("observations poisoned");
        match event.channel.as_str() {
            "connected" | "disconnected" => {
                state.reset_feed();
                state.connected = event.channel == "connected";
                state.head_ack = false;
            }
            "subscriptionResponse" if event.data["type"] == "newHeads" => state.head_ack = true,
            "newHeads" => {
                let head = match Header::parse(&event.data) {
                    Ok(h) => h,
                    Err(e) => {
                        state.reset_feed();
                        return Err(e);
                    }
                };
                if let Some((_, previous)) = state.headers.last_key_value() {
                    if head.number == previous.number && head.hash == previous.hash {
                        return Ok(());
                    }
                    if head.number != previous.number.saturating_add(1)
                        || head.parent != previous.hash
                    {
                        state.reset_feed(); // 缺块/重组后必须重新与 RPC 锚定，不能沿用旧状态。
                    }
                }
                state.headers.insert(head.number, head);
                while state.headers.len() > 1024 {
                    state.headers.pop_first();
                }
            }
            "logs" => {
                if event.data["removed"] == true {
                    state.reset_feed();
                    return Ok(());
                }
                let address = event.data["address"].as_str().unwrap_or("");
                if address.eq_ignore_ascii_case(&c.pool) {
                    let decoded = super::events::decode(&event.data)?;
                    if decoded["kind"] == "Swap" {
                        let sqrt = decoded["fields"]["sqrtPriceX96"]
                            .as_str()
                            .context("swap sqrt price")?
                            .parse::<f64>()?
                            / 2f64.powi(96);
                        let base0 =
                            c.base_token.parse::<Address>()? < c.quote_token.parse::<Address>()?;
                        let price = if base0 {
                            sqrt * sqrt
                        } else {
                            1.0 / (sqrt * sqrt)
                        } * 10f64
                            .powi(c.base_decimals as i32 - c.quote_decimals as i32);
                        ensure!(price.is_finite() && price > 0.0, "invalid WSS swap price");
                        let number = super::rpc::hex_u64(&decoded["blockNumber"])?;
                        let index = super::rpc::hex_u64(&decoded["logIndex"])?;
                        if state.latest_swap.as_ref().is_some_and(|old| {
                            (
                                old["block_number"].as_u64().unwrap_or(0),
                                old["log_index"].as_u64().unwrap_or(0),
                            ) >= (number, index)
                        }) {
                            return Ok(());
                        }
                        state.latest_swap = Some(
                            json!({"received_ms":event.received_ms,"fields":decoded["fields"],
                            "price":price,"block_number":number,"log_index":index,
                            "block":decoded["blockNumber"],"transaction_hash":decoded["transactionHash"],"removed":false}),
                        );
                    } else if decoded["kind"] == "Mint" || decoded["kind"] == "Burn" {
                        state.snapshot = None; // 其他人的流动性变化不改我方 NFT/手续费缓存的确认块。
                    }
                } else if address.eq_ignore_ascii_case(&c.position_manager) {
                    let topics = event.data["topics"].as_array().context("NFT log topics")?;
                    let word = |i: usize| {
                        topics
                            .get(i)
                            .and_then(Value::as_str)
                            .and_then(|v| U256::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                    };
                    let transfer = topics.first().and_then(Value::as_str).is_some_and(|t| {
                        t.eq_ignore_ascii_case(&format!(
                            "{:#x}",
                            keccak256("Transfer(address,address,uint256)")
                        ))
                    });
                    let relevant = if transfer {
                        word(3).is_some_and(|id| state.ids.contains(&id))
                            || [1, 2].iter().any(|i| {
                                word(*i).is_some_and(|w| {
                                    state.owners.contains(&super::rpc::word_address(w))
                                })
                            })
                    } else {
                        word(1).is_some_and(|id| state.ids.contains(&id))
                    };
                    if relevant {
                        state.invalidate();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// 即使监控任务被 abort，也立即使其他模块停止使用这条 WSS 的旧观察。
pub struct FeedGuard(pub Arc<Shared>);
impl Drop for FeedGuard {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

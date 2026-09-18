//! Regression tests; all signed requests use a public fixture and loopback mock only.
//! All network requests stay on loopback; signing uses a public fixed test vector.
use crate::{
    config::Config,
    domain::{LiquidityExecutor, LpPosition, PoolSnapshot},
    engine::{self, Live},
    hyperliquid::{Client, journal::Orders},
    store::Store,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const USER: &str = "0x0000000000000000000000000000000000000001";
fn config() -> Config {
    let mut c = Config::load("config/paper-200.toml").unwrap();
    c.hyperliquid.account = Some(USER.into());
    c.hyperliquid.vault = None;
    c.hyperliquid.private_key_env = "LPMAKER_REVIEW_PUBLIC_TEST_KEY".into();
    c.hyperliquid.maker_wait_seconds = 0;
    c
}
struct NoLiquidity;
#[async_trait]
impl LiquidityExecutor for NoLiquidity {
    async fn snapshot(&self) -> Result<PoolSnapshot> {
        unreachable!()
    }
    async fn current_positions(&self) -> Result<Vec<LpPosition>> {
        unreachable!()
    }
    async fn wallet_balances(&self) -> Result<(f64, f64)> {
        unreachable!()
    }
    async fn mint_layer(&self, _: &str, _: f64, _: f64) -> Result<()> {
        unreachable!()
    }
    async fn increase_position(&self, _: &LpPosition, _: f64) -> Result<()> {
        unreachable!()
    }
    async fn remove_position(&self, _: &LpPosition) -> Result<()> {
        unreachable!()
    }
    async fn swap_inventory(&self, _: bool, _: f64) -> Result<()> {
        unreachable!()
    }
}
#[derive(Default)]
struct ExchangeState {
    submitted_id: String,
    canceled: bool,
    actions: Vec<String>,
    bad_agent: bool,
    expired_agent: bool,
    drop_exchange_reply: bool,
    fail_cancel_meta: usize,
    target_role: Option<String>,
    target_owner: Option<String>,
}
struct Mock {
    url: String,
    state: Arc<Mutex<ExchangeState>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ExchangeState::default()));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut data = vec![];
                let mut buf = [0; 4096];
                let end = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().unwrap())
                    })
                    .unwrap();
                while data.len() < end + length {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                }
                let q: Value = serde_json::from_slice(&data[end..end + length]).unwrap();
                if q["type"] == "meta" {
                    let mut state = shared.lock().unwrap();
                    if !state.submitted_id.is_empty() && state.fail_cancel_meta > 0 {
                        state.fail_cancel_meta -= 1;
                        continue;
                    }
                }
                let result = {
                    let mut s = shared.lock().unwrap();
                    if let Some(action) = q.get("action") {
                        let kind = action["type"].as_str().unwrap();
                        s.actions.push(kind.into());
                        match kind {
                            "order" => {
                                s.submitted_id = action["orders"][0]["c"].as_str().unwrap().into();
                                json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":42}}]}}})
                            }
                            "cancelByCloid" => {
                                s.canceled = true;
                                json!({"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}})
                            }
                            _ => panic!("unexpected mock exchange action: {kind}"),
                        }
                    } else {
                        match q["type"].as_str().unwrap() {
                            "meta" => {
                                json!({"universe":[{"name":"ETH","szDecimals":4,"maxLeverage":25}]})
                            }
                            "l2Book" => {
                                json!({"time":crate::now_ms(),"levels":[[{"px":"2500"}],[{"px":"2500"}]]})
                            }
                            "clearinghouseState" => {
                                let positions = if s.submitted_id.is_empty() {
                                    json!([])
                                } else {
                                    json!([{"position":{"coin":"ETH","szi":"-0.006"}}])
                                };
                                json!({"assetPositions":positions,"withdrawable":"60","marginSummary":{"accountValue":"60"}})
                            }
                            "openOrders" => {
                                if !s.submitted_id.is_empty() && !s.canceled {
                                    json!([{"oid":42,"coin":"ETH"}])
                                } else {
                                    json!([])
                                }
                            }
                            "vaultDetails" => {
                                json!({"vaultAddress":"0x0000000000000000000000000000000000000003","leader":s.target_owner})
                            }
                            "userRole" => {
                                if q["user"]
                                    .as_str()
                                    .is_some_and(|u| u.eq_ignore_ascii_case(USER))
                                {
                                    json!({"role":"user"})
                                } else if q["user"] == "0x0000000000000000000000000000000000000003"
                                {
                                    json!({"role":s.target_role,"data":{"master":s.target_owner}})
                                } else {
                                    json!({"role":"agent","data":{"user":if s.bad_agent {"0x0000000000000000000000000000000000000002"} else {USER}}})
                                }
                            }
                            "extraAgents" => {
                                json!([{"name":"test","address":"0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
                                "validUntil":if s.expired_agent {1} else {crate::now_ms()+3_600_000}}])
                            }
                            "activeAssetData" => {
                                json!({"user":USER,"coin":"ETH","leverage":{"type":"isolated","value":3}})
                            }
                            "orderStatus" => {
                                if q["oid"].as_str() == Some(s.submitted_id.as_str()) {
                                    json!({"status":"order","order":{"status":if s.canceled {"canceled"} else {"open"},"order":{"oid":42,"cloid":s.submitted_id,"coin":"ETH","sz":"0.002","origSz":"0.008"}}})
                                } else {
                                    json!({"status":"unknownOid"})
                                }
                            }
                            kind => panic!("unexpected mock info type: {kind}"),
                        }
                    }
                };
                if q.get("action").is_some() && shared.lock().unwrap().drop_exchange_reply {
                    continue;
                }
                let body = result.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self { url, state, task }
    }
}

fn signed_client(c: &Config, store: Arc<Store>, url: &str) -> Client {
    let mut client = Client::new(c.hyperliquid.clone(), store).unwrap();
    client.cfg.http_url = url.into();
    client.signer = Some(format!("{:064x}", 1).parse().unwrap());
    client
}
fn live(c: Config, store: Arc<Store>, url: &str) -> Live {
    Live {
        liquidity: Arc::new(NoLiquidity),
        hl: signed_client(&c, store.clone(), url),
        store,
        cfg: c,
    }
}
#[tokio::test]
async fn partial_fill_dust_defers_without_phantom_order_and_restart_succeeds() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let live = live(config(), store.clone(), &mock.url);
    live.hedge(0.008, true).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    assert!(store.read::<Value>("hedge_order.json").unwrap().is_none());
    let residual = store.read::<Value>("hedge_residual.json").unwrap().unwrap();
    assert!((residual["residual_usd"].as_f64().unwrap() - 5.0).abs() < 1e-9);
    assert_eq!(
        mock.state.lock().unwrap().actions,
        ["order", "cancelByCloid"]
    );
    live.sync_orders(true).await.unwrap();
    let ledger = store.read::<Orders>("orders.json").unwrap().unwrap();
    assert_eq!(ledger.len(), 1);
    assert!(ledger.values().all(|o| o.terminal));
    // Repeating the same target uses actual inventory; it does not accumulate/fabricate a new order.
    live.hedge(0.008, true).await.unwrap();
    assert_eq!(mock.state.lock().unwrap().actions.len(), 2);
}
#[tokio::test]
async fn tiny_initial_hedge_creates_no_order_intent() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let live = live(config(), store.clone(), &mock.url);
    live.hedge(0.002, true).await.unwrap();
    assert!(mock.state.lock().unwrap().actions.is_empty());
    assert!(store.pending().unwrap().is_none());
    assert!(store.read::<Value>("hedge_order.json").unwrap().is_none());
}
#[tokio::test]
async fn wrong_or_expired_agent_is_rejected_before_any_exchange_or_intent() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let client = signed_client(&config(), store.clone(), &mock.url);
    for (bad, expired) in [(true, false), (false, true)] {
        {
            let mut s = mock.state.lock().unwrap();
            s.bad_agent = bad;
            s.expired_agent = expired;
        }
        let err = client.leverage("ETH", 3, false).await.unwrap_err();
        assert!(!crate::runtime::retryable(&err));
        assert!(store.pending().unwrap().is_none());
        assert!(store.read::<Value>("hl_identity.json").unwrap().is_none());
    }
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn changing_authorized_account_cannot_rebind_existing_state() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let client = signed_client(&config(), store.clone(), &mock.url);
    let signer = client.signer.as_ref().unwrap().address();
    client.verify_identity(signer).await.unwrap();
    let mut identity = store.read::<Value>("hl_identity.json").unwrap().unwrap();
    identity["user"] = json!("0x0000000000000000000000000000000000000002");
    store.write("hl_identity.json", &identity).unwrap();
    assert!(client.verify_identity(signer).await.is_err());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn lost_order_reply_keeps_pending_and_never_retries_exchange() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().drop_exchange_reply = true;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let live = live(config(), store.clone(), &mock.url);
    let err = live.hedge(0.008, true).await.unwrap_err();
    assert!(!crate::runtime::retryable(&err));
    assert_eq!(
        store.pending().unwrap().unwrap()["dispatch_state"],
        "submitting"
    );
    assert_eq!(mock.state.lock().unwrap().actions, ["order"]);
}
#[tokio::test]
async fn lost_leverage_ack_reconciles_without_position_or_write() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut c = config();
    c.hyperliquid.http_url = mock.url.clone();
    store.begin(json!({"venue":"hyperliquid","user":USER,"request":{"action":{"type":"updateLeverage","asset":0,"isCross":false,"leverage":3}}})).unwrap();
    engine::reconcile(&c, store.clone()).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn pre_dispatch_crash_can_be_resolved_without_query_or_resubmit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut c = config();
    c.hyperliquid.http_url = "http://127.0.0.1:1".into();
    let id = "0x11111111111111111111111111111111";
    store.begin(json!({"venue":"hyperliquid","user":USER,"dispatch_state":"prepared","hedge_intent":{"cloid":id},
        "request":{"nonce":1,"action":{"type":"order","orders":[{"c":id}]}}})).unwrap();
    engine::reconcile(&c, store.clone()).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    let ledger = store.read::<Orders>("orders.json").unwrap().unwrap();
    assert!(ledger[id].terminal && ledger[id].managed_hedge);
    assert_eq!(ledger[id].status, "not_submitted");
}
#[tokio::test]
async fn order_quote_expiring_during_preflight_cannot_be_submitted() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let client = signed_client(&config(), store.clone(), &mock.url);
    let err = client
        .managed_order(
            "ETH",
            false,
            "2500",
            "0.01",
            "Alo",
            "0x11111111111111111111111111111111",
            0.01,
            crate::now_ms() - 61_000,
            60,
        )
        .await
        .unwrap_err();
    assert!(crate::runtime::retryable(&err));
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}

#[tokio::test]
async fn cancel_preflight_read_failure_recovers_the_existing_order_without_duplicate() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().fail_cancel_meta = 3;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let live = live(config(), store.clone(), &mock.url);
    let err = live.hedge(0.008, true).await.unwrap_err();
    assert!(crate::runtime::retryable(&err));
    assert!(store.pending().unwrap().is_none());
    assert_eq!(mock.state.lock().unwrap().actions, ["order"]);
    crate::runtime::retry_reads(&store, std::time::Duration::from_millis(1), false, || {
        live.sync_orders(true)
    })
    .await
    .unwrap();
    assert_eq!(
        mock.state.lock().unwrap().actions,
        ["order", "cancelByCloid"]
    );
    assert!(store.read::<Value>("hedge_order.json").unwrap().is_none());
}
#[tokio::test]
async fn direct_master_subaccount_and_vault_require_matching_authority() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut client = signed_client(&config(), store, &mock.url);
    super::auth::verify(&client, USER.parse().unwrap())
        .await
        .unwrap();
    client.cfg.vault = Some("0x0000000000000000000000000000000000000003".into());
    for role in ["subAccount", "vault"] {
        {
            let mut state = mock.state.lock().unwrap();
            state.target_role = Some(role.into());
            state.target_owner = Some(USER.into());
        }
        let signer = client.signer.as_ref().unwrap().address();
        super::auth::verify(&client, signer).await.unwrap();
        mock.state.lock().unwrap().target_owner =
            Some("0x0000000000000000000000000000000000000002".into());
        assert!(super::auth::verify(&client, signer).await.is_err());
    }
    assert!(mock.state.lock().unwrap().actions.is_empty());
}

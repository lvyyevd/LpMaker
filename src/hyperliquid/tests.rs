//! Regression tests; all signed requests use a public fixture and loopback mock only.
//! All network requests stay on loopback; signing uses a public fixed test vector.
use crate::{
    config::Config,
    domain::{Decision, HedgeVenue, LiquidityExecutor, LpIntent, LpPosition, PoolSnapshot},
    engine::{self, Live},
    hyperliquid::{Client, journal::Orders},
    store::Store,
    strategy::{EntryHistory, Phase, Strategy},
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
        Ok(vec![])
    }
    async fn wallet_balances(&self) -> Result<(f64, f64)> {
        Ok((0.0, 120.0))
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
    collateral: Option<f64>,
    account_mode: Option<String>,
    next_mode: Option<String>,
    spot_state: Option<Value>,
    active_state: Option<Value>,
    cloid_unknown: bool,
    all_status_unknown: bool,
    hide_open_orders: bool,
    filled: bool,
    status_script: std::collections::VecDeque<Value>,
    after_cancel_status: std::collections::VecDeque<Value>,
    order_queries: Vec<Value>,
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
                                s.status_script = std::mem::take(&mut s.after_cancel_status);
                                json!({"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}})
                            }
                            _ => panic!("unexpected mock exchange action: {kind}"),
                        }
                    } else {
                        match q["type"].as_str().unwrap() {
                            "userAbstraction" => {
                                let mode = s.account_mode.clone().unwrap_or_else(|| "default".into());
                                if let Some(next) = s.next_mode.take() {s.account_mode=Some(next);}
                                json!(mode)
                            }
                            "spotClearinghouseState" => s.spot_state.clone().expect("unexpected spot request"),
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
                                let collateral = s.collateral.unwrap_or(60.0).to_string();
                                json!({"assetPositions":positions,"withdrawable":collateral,"marginSummary":{"accountValue":collateral}})
                            }
                            "openOrders" => {
                                if !s.submitted_id.is_empty() && !s.canceled && !s.filled && !s.hide_open_orders {
                                    json!([{"oid":42,"coin":"ETH","cloid":s.submitted_id,"sz":"0.002","origSz":"0.008","side":"A","limitPx":"2500"}])
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
                                s.active_state.clone().unwrap_or_else(|| json!({"user":USER,"coin":"ETH","leverage":{"type":"isolated","value":3}}))
                            }
                            "orderStatus" => {
                                s.order_queries.push(q["oid"].clone());
                                if let Some(response) = s.status_script.pop_front() {
                                    response
                                } else if s.all_status_unknown || (s.cloid_unknown && q["oid"].is_string()) {
                                    json!({"status":"unknownOid"})
                                } else if q["oid"].as_str() == Some(s.submitted_id.as_str()) || (q["oid"] == 42 && !s.submitted_id.is_empty()) {
                                    json!({"status":"order","order":{"status":if s.filled {"filled"} else if s.canceled {"canceled"} else {"open"},"order":{"oid":42,"cloid":s.submitted_id,"coin":"ETH","sz":if s.filled {"0"} else {"0.002"},"origSz":"0.008"}}})
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
fn unified_fixture(mock: &Mock, sell_available: &str) {
    let mut s = mock.state.lock().unwrap();
    s.account_mode = Some("unifiedAccount".into());
    s.collateral = Some(0.0); // Same native-perp zero balance returned by the user's account.
    s.spot_state = Some(
        json!({"balances":[{"coin":"USDC","token":0,"total":"79.6","hold":"0"}],"tokenToAvailableAfterMaintenance":[[0,"79.6"]]}),
    );
    s.active_state = Some(
        json!({"user":USER,"coin":"ETH","leverage":{"type":"isolated","value":3,"rawUsd":"0"},"availableToTrade":["79.6",sell_available],"maxTradeSzs":["0.0959","0.0959"],"markPx":"2488.98"}),
    );
}
#[tokio::test]
async fn unified_balance_reaches_portfolio_entry_guard_and_actual_hedge_order() {
    let mock = Mock::start().await;
    unified_fixture(&mock, "79.6");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    let p = l.portfolio().await.unwrap();
    assert_eq!(p.hedge_equity, 79.6);
    let snapshot = store
        .read::<Value>("account_snapshot.json")
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot["account"]["lpMakerCollateral"]["equity_usdc"],
        79.6
    );
    assert_eq!(snapshot["account"]["withdrawable"], "0");
    let d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 1.0 },
        target_short_base: 0.0,
        emergency: false,
    };
    l.preflight_entry(&d, &p).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
    // Public fixture signature goes to loopback ONLY. Maker and cancel must reach execution.
    l.hedge(0.008, false).await.unwrap();
    assert_eq!(
        mock.state.lock().unwrap().actions,
        vec!["order", "cancelByCloid"]
    );
    assert!(store.pending().unwrap().is_none());
}
#[tokio::test]
async fn unified_short_side_collateral_still_blocks_entry_and_hedge_when_insufficient() {
    let mock = Mock::start().await;
    unified_fixture(&mock, "0.5"); // Long availability cannot be used as short collateral.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    let d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 1.0 },
        target_short_base: 0.0,
        emergency: false,
    };
    assert!(
        l.apply(&d)
            .await
            .unwrap_err()
            .to_string()
            .contains("entry_waiting_hedge_collateral")
    );
    assert!(
        l.hedge(0.008, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("insufficient free hedge collateral")
    );
    assert!(store.read::<Value>("workflow.json").unwrap().is_none());
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn account_mode_switch_and_missing_capacity_block_without_signing() {
    let mock = Mock::start().await;
    unified_fixture(&mock, "79.6");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    mock.state.lock().unwrap().next_mode = Some("default".into());
    assert!(
        l.hl.account()
            .await
            .unwrap_err()
            .to_string()
            .contains("mode changed")
    );
    unified_fixture(&mock, "79.6");
    mock.state
        .lock()
        .unwrap()
        .active_state
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("availableToTrade");
    assert!(
        l.hl.account()
            .await
            .unwrap_err()
            .to_string()
            .contains("availableToTrade")
    );
    mock.state.lock().unwrap().account_mode = Some("portfolioMargin".into());
    assert!(
        l.hl.account()
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn standard_account_does_not_spend_spot_only_balance() {
    let mock = Mock::start().await;
    unified_fixture(&mock, "79.6");
    mock.state.lock().unwrap().account_mode = Some("disabled".into());
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    assert_eq!(l.portfolio().await.unwrap().hedge_equity, 0.0);
    assert!(l.hedge(0.008, false).await.is_err());
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn zero_hedge_collateral_blocks_entry_before_any_swap_or_signed_action() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().collateral = Some(0.0);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let c = config();
    let l = live(c, store.clone(), &mock.url);
    let mut d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 1.0 },
        target_short_base: 0.0,
        emergency: false,
    };
    // Every LiquidityExecutor mutation panics, so this catches a buy before the guard.
    assert!(
        l.apply(&d)
            .await
            .unwrap_err()
            .to_string()
            .contains("entry_waiting_hedge_collateral")
    );
    assert!(store.read::<Value>("workflow.json").unwrap().is_none());
    assert!(store.pending().unwrap().is_none());
    assert!(mock.state.lock().unwrap().actions.is_empty());
    let p = l.portfolio().await.unwrap();
    let previous = Strategy {
        phase: Phase::Paused,
        pause_since: 1234,
        ..Default::default()
    };
    let mut evaluated = previous.clone();
    evaluated.phase = Phase::Active;
    evaluated.fraction = 1.0;
    evaluated.peak_equity = 200.0;
    engine::defer_entry(&mut evaluated, &previous, &mut d, &p);
    assert_eq!(evaluated.phase, Phase::Paused);
    assert_eq!(evaluated.entry_history, EntryHistory::Initial);
    assert_eq!(evaluated.pause_since, 1234);
    assert_eq!(evaluated.fraction, 0.0);
    assert_eq!(evaluated.peak_equity, 200.0);
    assert_eq!(d.lp, LpIntent::Hold);
    // Normal polling can retry after funding; no timer/state reset is necessary.
    mock.state.lock().unwrap().collateral = Some(60.0);
    d.lp = LpIntent::Deploy { fraction: 1.0 };
    l.preflight_entry(&d, &p).await.unwrap();
    assert!(mock.state.lock().unwrap().actions.is_empty());
}
#[tokio::test]
async fn entry_collateral_guard_keeps_buffer_and_allows_risk_exit() {
    let mock = Mock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store, &mock.url);
    let p = l.portfolio().await.unwrap();
    let mut d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 1.0 },
        target_short_base: 0.0,
        emergency: false,
    };
    mock.state.lock().unwrap().collateral = Some(40.0); // Exactly 120 / 3 leaves no buffer.
    assert!(l.preflight_entry(&d, &p).await.is_err());
    mock.state.lock().unwrap().collateral = Some(45.0);
    l.preflight_entry(&d, &p).await.unwrap();
    mock.state.lock().unwrap().collateral = Some(0.0);
    d.lp = LpIntent::ExitToQuote;
    l.preflight_entry(&d, &p).await.unwrap();
    let previous = Strategy::default();
    let mut evaluated = previous.clone();
    let mut exposed = p.clone();
    exposed.wallet_base = 0.01;
    engine::defer_entry(&mut evaluated, &previous, &mut d, &exposed);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert!(d.emergency);
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

#[tokio::test]
async fn acknowledged_oid_is_used_when_cloid_index_is_missing() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().cloid_unknown = true;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    l.hedge(0.008, false).await.unwrap();
    let s = mock.state.lock().unwrap();
    assert_eq!(s.actions, ["order", "cancelByCloid"]);
    assert_eq!(s.order_queries, [json!(42), json!(42)]);
    assert!(
        store
            .read::<Orders>("orders.json")
            .unwrap()
            .unwrap()
            .values()
            .all(|o| o.terminal)
    );
}

#[tokio::test]
async fn current_open_order_recovers_missing_status_indexes_without_new_order() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().status_script = [
        json!({"status":"unknownOid"}),
        json!({"status":"unknownOid"}),
    ]
    .into();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    live(config(), store.clone(), &mock.url)
        .hedge(0.008, false)
        .await
        .unwrap();
    assert_eq!(
        mock.state.lock().unwrap().actions,
        ["order", "cancelByCloid"]
    );
    let audit = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert!(audit.contains("openOrders"));
    assert!(store.pending().unwrap().is_none());
}

#[tokio::test]
async fn transient_absence_and_delayed_cancel_status_are_only_read_retried() {
    let mock = Mock::start().await;
    {
        let mut s = mock.state.lock().unwrap();
        s.hide_open_orders = true;
        s.status_script = [
            json!({"status":"unknownOid"}),
            json!({"status":"unknownOid"}),
        ]
        .into();
        s.after_cancel_status = s.status_script.clone();
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    live(config(), store.clone(), &mock.url)
        .hedge(0.008, false)
        .await
        .unwrap();
    let s = mock.state.lock().unwrap();
    assert_eq!(s.actions, ["order", "cancelByCloid"]);
    assert_eq!(s.order_queries.len(), 6);
    assert!(
        store
            .read::<Orders>("orders.json")
            .unwrap()
            .unwrap()
            .values()
            .all(|o| o.status == "canceled")
    );
}

#[tokio::test]
async fn unresolved_acknowledged_order_retains_evidence_then_restart_reads_later_fill() {
    let mock = Mock::start().await;
    {
        let mut s = mock.state.lock().unwrap();
        s.all_status_unknown = true;
        s.hide_open_orders = true;
    }
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let l = live(config(), store.clone(), &mock.url);
        let error = l.hedge(0.008, false).await.unwrap_err();
        assert!(crate::runtime::retryable(&error));
        let orders = store.read::<Orders>("orders.json").unwrap().unwrap();
        let o = orders.values().next().unwrap();
        assert_eq!(o.oid, Some(42));
        assert_eq!(o.status, "open");
        assert!(!o.terminal);
        id = o.cloid.clone();
        assert!(store.read::<Value>("hedge_order.json").unwrap().is_some());
        assert!(store.pending().unwrap().is_none()); // ACK was durably confirmed.
        assert_eq!(mock.state.lock().unwrap().actions, ["order"]);
        // Reproduce the old release's unknown record while retaining its accepted OID.
        store
            .update::<Orders>("orders.json", |ledger| {
                ledger.get_mut(&id).unwrap().status = "unknown".into();
                Ok(())
            })
            .unwrap();
    }
    {
        let mut s = mock.state.lock().unwrap();
        s.all_status_unknown = false;
        s.cloid_unknown = true;
        s.filled = true;
    }
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let l = live(config(), store.clone(), &mock.url);
    l.sync_orders(true).await.unwrap();
    assert_eq!(l.portfolio().await.unwrap().short_base, 0.006);
    assert_eq!(
        store.read::<Orders>("orders.json").unwrap().unwrap()[&id].status,
        "filled"
    );
    assert!(store.read::<Value>("hedge_order.json").unwrap().is_none());
    assert_eq!(mock.state.lock().unwrap().actions, ["order"]);
}

#[tokio::test]
async fn lost_ack_finds_cloid_in_open_orders_and_persists_oid_before_recovery_cancel() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().drop_exchange_reply = true;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut c = config();
    c.hyperliquid.http_url = mock.url.clone();
    let l = live(c.clone(), store.clone(), &mock.url);
    assert!(l.hedge(0.008, false).await.is_err());
    {
        let mut s = mock.state.lock().unwrap();
        s.drop_exchange_reply = false;
        s.cloid_unknown = true;
    }
    engine::reconcile(&c, store.clone()).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    let orders = store.read::<Orders>("orders.json").unwrap().unwrap();
    assert_eq!(orders.values().next().unwrap().oid, Some(42));
    l.sync_orders(true).await.unwrap();
    assert_eq!(
        mock.state.lock().unwrap().actions,
        ["order", "cancelByCloid"]
    );
}

#[tokio::test]
async fn oid_response_for_another_cloid_blocks_without_cancel_or_replacement() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().status_script.push_back(json!({"status":"order",
        "order":{"status":"open","order":{"oid":42,"cloid":"0x11111111111111111111111111111111"}}}));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let error = live(config(), store.clone(), &mock.url)
        .hedge(0.008, false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cloid mismatch"));
    assert!(!crate::runtime::retryable(&error));
    assert_eq!(mock.state.lock().unwrap().actions, ["order"]);
    assert!(store.read::<Value>("hedge_order.json").unwrap().is_some());
}

#[tokio::test]
async fn stale_open_status_after_cancel_is_not_treated_as_terminal() {
    let mock = Mock::start().await;
    let open = json!({"status":"order","order":{"status":"open","order":{"oid":42,"cloid":null}}});
    mock.state.lock().unwrap().after_cancel_status = [open.clone(), open].into();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    live(config(), store.clone(), &mock.url)
        .hedge(0.008, false)
        .await
        .unwrap();
    let s = mock.state.lock().unwrap();
    assert_eq!(s.actions, ["order", "cancelByCloid"]);
    assert_eq!(s.order_queries.len(), 4);
    assert!(
        store
            .read::<Orders>("orders.json")
            .unwrap()
            .unwrap()
            .values()
            .all(|o| o.status == "canceled")
    );
}

#[tokio::test]
async fn unknown_cloid_never_adopts_an_unrelated_open_order() {
    let mock = Mock::start().await;
    {
        let mut s = mock.state.lock().unwrap();
        s.submitted_id = "0x22222222222222222222222222222222".into();
        s.all_status_unknown = true;
    }
    let id = "0x11111111111111111111111111111111";
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    super::journal::prepare(
        &store,
        &json!({"type":"order","orders":[{"c":id}]}),
        USER,
        1,
    )
    .unwrap();
    let client = signed_client(&config(), store.clone(), &mock.url);
    let error = super::order_recovery::resolve(&client, &store, id, false)
        .await
        .unwrap_err();
    assert!(crate::runtime::retryable(&error));
    let ledger = store.read::<Orders>("orders.json").unwrap().unwrap();
    assert_eq!(ledger[id].oid, None);
    assert!(!ledger[id].terminal);
    assert!(mock.state.lock().unwrap().actions.is_empty());
}

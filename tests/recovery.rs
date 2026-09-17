use anyhow::Result;
use async_trait::async_trait;
use lp_maker::{
    config::{Config, Mode},
    domain::{LiquidityExecutor, LpPosition, PoolSnapshot},
    engine::{self, Live, Paper, PaperOrder},
    hyperliquid::{
        Client,
        journal::{self, Orders},
    },
    recovery,
    store::Store,
    strategy::{Phase, Strategy},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const USER: &str = "0x0000000000000000000000000000000000000001";
const ID: &str = "0x11111111111111111111111111111111";
fn config() -> Config {
    Config::load("config/paper-200.toml").unwrap()
}
fn action() -> Value {
    json!({"type":"order","orders":[{"a":1,"b":false,"p":"2500","s":"0.05","r":false,"t":{"limit":{"tif":"Alo"}},"c":ID}],"grouping":"na"})
}
fn status(state: &str, remaining: &str) -> Value {
    json!({"status":"order","order":{"order":{"coin":"ETH","oid":42,"cloid":ID,"sz":remaining,"origSz":"0.05","side":"A","limitPx":"2500"},"status":state,"statusTimestamp":12345}})
}
fn prepared(store: &Store, managed: bool) {
    if managed {
        store
            .write(
                "hedge_order.json",
                &json!({"coin":"ETH","cloid":ID,"target":0.05}),
            )
            .unwrap();
    }
    journal::prepare(store, &action(), USER, 1000).unwrap();
}

#[test]
fn checkpoint_restores_balances_pending_order_and_phase_as_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    let c = config();
    {
        let s = Store::open(dir.path()).unwrap();
        let mut strategy = Strategy {
            phase: Phase::Paused,
            pause_since: 9000,
            peak_equity: 215.0,
            ..Default::default()
        };
        strategy.healthy_hours = 4;
        let mut paper = Paper::new(&c);
        paper.portfolio.short_base = 0.015;
        paper.portfolio.hedge_equity = 58.1;
        paper.pending = Some(PaperOrder {
            target: 0.025,
            buy: false,
            price: 2400.0,
            submitted: 999,
            emergency: false,
        });
        recovery::save(&s, &c, &strategy, &paper).unwrap();
        // Simulate a crash leaving the compatibility files stale or malformed.
        s.write("strategy.json", &json!({"bad":"stale export"}))
            .unwrap();
        s.write("paper.json", &json!({"wallet":"obsolete"}))
            .unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    let (strategy, paper) = recovery::load(&s, &c).unwrap();
    assert_eq!(strategy.phase, Phase::Paused);
    assert_eq!(strategy.pause_since, 9000);
    assert_eq!(strategy.peak_equity, 215.0);
    assert_eq!(paper.portfolio.hedge_equity, 58.1);
    assert_eq!(paper.portfolio.short_base, 0.015);
    assert_eq!(paper.pending.unwrap().submitted, 999);
    assert_eq!(
        s.read::<Strategy>("strategy.json").unwrap().unwrap().phase,
        Phase::Paused
    );
}

#[test]
fn legacy_migration_preserves_inventory_and_corrupt_checkpoint_never_resets() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let c = config();
    let mut p = Paper::new(&c);
    p.portfolio.wallet_quote = 17.5;
    s.write("paper.json", &p).unwrap();
    s.write(
        "strategy.json",
        &Strategy {
            phase: Phase::Halted,
            ..Default::default()
        },
    )
    .unwrap();
    let (strategy, paper) = recovery::load(&s, &c).unwrap();
    assert_eq!(strategy.phase, Phase::Halted);
    assert_eq!(paper.portfolio.wallet_quote, 17.5);
    s.write("checkpoint.json", &json!({"schema":999})).unwrap();
    assert!(recovery::load(&s, &c).is_err());
    assert_eq!(
        s.read::<Paper>("paper.json")
            .unwrap()
            .unwrap()
            .portfolio
            .wallet_quote,
        17.5
    );
}

#[test]
fn restart_missing_layers_pauses_without_clearing_drawdown_halt() {
    let c = config();
    let mut strategy = Strategy {
        phase: Phase::Active,
        peak_equity: 300.0,
        healthy_hours: 8,
        ..Default::default()
    };
    assert!(recovery::pause_incomplete_inventory(&mut strategy, &[], &c));
    assert_eq!(strategy.phase, Phase::Paused);
    assert_eq!(strategy.healthy_hours, 0);
    assert_eq!(strategy.peak_equity, 300.0);
    strategy.phase = Phase::Halted;
    assert!(!recovery::pause_incomplete_inventory(
        &mut strategy,
        &[],
        &c
    ));
    assert_eq!(strategy.phase, Phase::Halted);
    recovery::finish_workflow_recovery(&mut strategy);
    assert_eq!(strategy.phase, Phase::Halted);
}

#[test]
fn missing_legacy_balance_file_is_not_replaced_with_starting_capital() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.write("strategy.json", &Strategy::default()).unwrap();
    assert!(recovery::load(&s, &config()).is_err());
    assert!(s.read::<Paper>("paper.json").unwrap().is_none());
}

#[test]
fn partial_fill_and_cancel_are_recorded_without_replaying_remaining_size() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    prepared(&s, true);
    journal::acknowledged(
        &s,
        &action(),
        &json!({"status":"ok","response":{"data":{"statuses":[{"resting":{"oid":42}}]}}}),
    )
    .unwrap();
    journal::observe(&s, ID, &status("open", "0.03")).unwrap();
    let row = &s.read::<Orders>("orders.json").unwrap().unwrap()[ID];
    assert!(!row.terminal);
    assert_eq!(row.wire["s"], "0.05");
    assert_eq!(row.exchange["order"]["order"]["sz"], "0.03");
    assert_eq!(
        journal::managed_open_orders(&s, &json!([{"oid":42,"coin":"ETH"}]), "ETH").unwrap(),
        vec![ID]
    );
    journal::observe(&s, ID, &status("canceled", "0.03")).unwrap();
    assert!(s.read::<Orders>("orders.json").unwrap().unwrap()[ID].terminal);
    assert!(
        journal::managed_open_orders(&s, &json!([]), "ETH")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn manual_unknown_and_inconsistent_orders_are_never_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert!(journal::managed_open_orders(&s, &json!([{"oid":99,"coin":"ETH"}]), "ETH").is_err());
    prepared(&s, false);
    journal::observe(&s, ID, &status("open", "0.03")).unwrap();
    assert!(journal::managed_open_orders(&s, &json!([{"oid":42,"coin":"ETH"}]), "ETH").is_err());
    assert!(journal::managed_open_orders(&s, &json!([]), "ETH").is_err());
    let mut changed = status("filled", "0");
    changed["order"]["order"]["oid"] = json!(999);
    assert!(journal::observe(&s, ID, &changed).is_err());
    assert!(!s.read::<Orders>("orders.json").unwrap().unwrap()[ID].terminal);
}

#[test]
fn unknown_status_and_unknown_ack_remain_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    prepared(&s, true);
    assert!(journal::observe(&s, ID, &json!({"status":"unknownOid"})).is_err());
    assert!(!s.read::<Orders>("orders.json").unwrap().unwrap()[ID].terminal);
    assert!(journal::observe(&s, ID, &status("futureUnknownStatus", "0.03")).is_err());
    assert!(
        journal::acknowledged(
            &s,
            &action(),
            &json!({"status":"ok","response":{"data":{"statuses":[{"unexpected":true}]}}})
        )
        .is_err()
    );
    assert!(!s.read::<Orders>("orders.json").unwrap().unwrap()[ID].terminal);
}

#[test]
fn startup_report_and_concurrent_updates_are_durable() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(dir.path()).unwrap());
    recovery::start(&s).unwrap();
    recovery::stage(&s, "checkpoint_loaded", json!({"phase":"Paused"})).unwrap();
    recovery::stage(&s, "blocked", json!({"reason":"unknown order"})).unwrap();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    s.update::<u64>("counter.json", |n| {
                        *n += 1;
                        Ok(())
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(s.read::<u64>("counter.json").unwrap(), Some(160));
    drop(s);
    let s = Store::open(dir.path()).unwrap();
    let report = s
        .read::<Value>("startup_reconciliation.json")
        .unwrap()
        .unwrap();
    assert_eq!(report["status"], "blocked");
    assert_eq!(report["stages"].as_array().unwrap().len(), 2);
}

/// This mock accepts info requests only; any exchange attempt fails the test.
async fn info_server(
    order_status: Value,
    account: Value,
) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let records = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![];
            let mut part = [0u8; 4096];
            let (header_end, length) = loop {
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&part[..n]);
                if let Some(index) = buf.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..index]).to_lowercase();
                    assert!(
                        headers.starts_with("post /info "),
                        "unexpected signed exchange request"
                    );
                    let length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    break (index + 4, length);
                }
            };
            while buf.len() < header_end + length {
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&part[..n]);
            }
            let request: Value =
                serde_json::from_slice(&buf[header_end..header_end + length]).unwrap();
            let response = match request["type"].as_str().unwrap() {
                "orderStatus" => order_status.clone(),
                "clearinghouseState" => account.clone(),
                "openOrders" => json!([]),
                other => panic!("unexpected request {other}"),
            };
            records.lock().unwrap().push(request);
            let body = response.to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        }
    });
    (url, requests, task)
}
fn pending(s: &Store) {
    s.begin(json!({"venue":"hyperliquid","user":USER,"request":{"action":action(),"nonce":1000,"expiresAfter":1}})).unwrap();
    s.write(
        "hedge_order.json",
        &json!({"coin":"ETH","cloid":ID,"target":0.05}),
    )
    .unwrap();
}

#[tokio::test]
async fn restart_resolves_lost_ack_by_cloid_without_resubmission() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Store::open(dir.path()).unwrap();
        pending(&s);
    }
    let s = Arc::new(Store::open(dir.path()).unwrap());
    let (url, requests, server) = info_server(status("filled", "0"), json!({})).await;
    let mut c = config();
    c.hyperliquid.http_url = url;
    c.hyperliquid.account = Some(USER.into());
    let r = engine::reconcile(&c, s.clone()).await.unwrap();
    assert_eq!(r["statuses"][0]["order"]["status"], "filled");
    assert!(s.pending().unwrap().is_none());
    assert!(s.read::<Orders>("orders.json").unwrap().unwrap()[ID].terminal);
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn expired_unknown_order_keeps_pending_and_blocks_replay() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(dir.path()).unwrap());
    pending(&s);
    let (url, _, server) = info_server(json!({"status":"unknownOid"}), json!({})).await;
    let mut c = config();
    c.hyperliquid.http_url = url;
    c.hyperliquid.account = Some(USER.into());
    assert!(engine::reconcile(&c, s.clone()).await.is_err());
    assert!(s.pending().unwrap().is_some());
    assert!(s.begin(json!({"venue":"another"})).is_err());
    assert_eq!(
        s.read::<Orders>("orders.json").unwrap().unwrap()[ID].status,
        "unknown"
    );
    server.abort();
}

struct ReadOnlyLiquidity;
#[async_trait]
impl LiquidityExecutor for ReadOnlyLiquidity {
    async fn snapshot(&self) -> Result<PoolSnapshot> {
        anyhow::bail!("not needed")
    }
    async fn current_positions(&self) -> Result<Vec<LpPosition>> {
        Ok(vec![])
    }
    async fn wallet_balances(&self) -> Result<(f64, f64)> {
        Ok((0.01, 55.0))
    }
    async fn mint_layer(&self, _: &str, _: f64, _: f64) -> Result<()> {
        panic!("must not mint during observation")
    }
    async fn increase_position(&self, _: &LpPosition, _: f64) -> Result<()> {
        panic!("must not increase")
    }
    async fn remove_position(&self, _: &LpPosition) -> Result<()> {
        panic!("must not remove")
    }
    async fn swap_inventory(&self, _: bool, _: f64) -> Result<()> {
        panic!("must not swap")
    }
}
#[tokio::test]
async fn restart_filled_order_refreshes_actual_position_and_keeps_audit() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(dir.path()).unwrap());
    prepared(&s, true);
    s.write(
        "live_inventory.json",
        &json!({"portfolio":{"short_base":0.0}}),
    )
    .unwrap();
    let account = json!({"assetPositions":[{"position":{"coin":"ETH","szi":"-0.02"}}],"marginSummary":{"accountValue":"59.1"}});
    let (url, requests, server) = info_server(status("filled", "0"), account).await;
    let mut c = config();
    c.mode = Mode::Live;
    c.hyperliquid.http_url = url;
    c.hyperliquid.account = Some(USER.into());
    let hl = Client::new(c.hyperliquid.clone(), s.clone()).unwrap();
    let live = Live {
        liquidity: Arc::new(ReadOnlyLiquidity),
        store: s.clone(),
        hl,
        cfg: c,
    };
    live.sync_orders(true).await.unwrap();
    let p = live.portfolio().await.unwrap();
    assert_eq!(p.short_base, 0.02);
    assert_eq!(p.hedge_equity, 59.1);
    assert_eq!(
        s.read::<Value>("live_inventory.json").unwrap().unwrap()["portfolio"]["short_base"],
        0.02
    );
    assert!(s.read::<Value>("hedge_order.json").unwrap().is_none());
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r["type"] != "exchange")
    );
    assert!(
        std::fs::read_to_string(dir.path().join("events.jsonl"))
            .unwrap()
            .contains("inventory_corrected")
    );
    server.abort();
}

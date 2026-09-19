use anyhow::{Result, bail};
use async_trait::async_trait;
use lp_maker::{
    config::Mode,
    domain::{Decision, LpIntent},
    solana::{
        bridge::Adapter,
        config::Config,
        dlmm::{self, Paper, Position, Snapshot, Wallet},
        journal::{self, Pending, Positions},
        runner,
    },
    store::Store,
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn config() -> Config {
    Config::load("config/solana.toml").unwrap()
}
struct Fake {
    sent: AtomicUsize,
    unknown: bool,
    status: Mutex<Value>,
}
#[async_trait]
impl Adapter for Fake {
    async fn call(&self, r: Value) -> Result<Value> {
        match r["method"].as_str().unwrap() {
            "plan" => Ok(json!({"count":1,"position":"position-A"})),
            "prepare" => Ok(
                json!({"signature":"original-signature","raw_transaction":"signed-original","position":"position-A","index":0,"count":1,"last_valid_block_height":100}),
            ),
            "send" => {
                self.sent.fetch_add(1, Ordering::SeqCst);
                if self.unknown {
                    bail!("lost RPC response")
                };
                Ok(json!({"signature":"original-signature"}))
            }
            "status" => Ok(self.status.lock().unwrap().clone()),
            "ack" => Ok(json!({})),
            _ => panic!("unexpected operation"),
        }
    }
}
fn fake(unknown: bool, status: Value) -> Fake {
    Fake {
        sent: AtomicUsize::new(0),
        unknown,
        status: Mutex::new(status),
    }
}
#[tokio::test]
async fn solana_lost_broadcast_keeps_original_and_blocks_another_send() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let f = fake(true, json!({"status":null,"finalized_height":9999}));
    assert!(
        journal::execute(&f, &store, json!({"kind":"mint"}), Some("core"))
            .await
            .is_err()
    );
    let p = store
        .read::<Pending>("solana_pending.json")
        .unwrap()
        .unwrap();
    assert_eq!(p.prepared["raw_transaction"], "signed-original");
    assert!(journal::reconcile(&f, &store).await.is_err());
    assert!(
        journal::execute(&f, &store, json!({"kind":"mint"}), Some("core"))
            .await
            .is_err()
    );
    assert_eq!(f.sent.load(Ordering::SeqCst), 1);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    *f.status.lock().unwrap() = json!({"status":{"confirmationStatus":"finalized","err":null}});
    journal::reconcile(&f, &store).await.unwrap();
    assert!(
        store
            .read::<Pending>("solana_pending.json")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .read::<Positions>("solana_positions.json")
            .unwrap()
            .unwrap()["core"]
            .address,
        "position-A"
    );
    assert_eq!(f.sent.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn confirmed_chain_failure_never_creates_position_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let f = fake(
        false,
        json!({"status":{"confirmationStatus":"confirmed","err":{"InstructionError":[0,"failure"]}}}),
    );
    assert!(
        journal::execute(&f, &store, json!({"kind":"mint"}), Some("core"))
            .await
            .is_err()
    );
    assert!(
        store
            .read::<Positions>("solana_positions.json")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .read::<Pending>("solana_pending.json")
            .unwrap()
            .is_none()
    );
}
#[test]
fn solana_finality_and_mapping_are_idempotent() {
    assert!(
        !journal::resolved(&json!({"status":{"confirmationStatus":"processed","err":null}}))
            .unwrap()
    );
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let p = Pending {
        operation: "mint".into(),
        layer: Some("core".into()),
        prepared: json!({"position":"pubkey","index":0,"count":2}),
    };
    journal::apply_confirmed(&store, &p).unwrap();
    let first = store
        .read::<Positions>("solana_positions.json")
        .unwrap()
        .unwrap();
    journal::apply_confirmed(&store, &p).unwrap();
    assert_eq!(
        first["core"].since_ms,
        store
            .read::<Positions>("solana_positions.json")
            .unwrap()
            .unwrap()["core"]
            .since_ms
    );
    let mut p = p;
    p.operation = "remove".into();
    journal::apply_confirmed(&store, &p).unwrap();
    assert!(
        store
            .read::<Positions>("solana_positions.json")
            .unwrap()
            .unwrap()
            .contains_key("core")
    );
    p.prepared["index"] = json!(1);
    journal::apply_confirmed(&store, &p).unwrap();
    assert!(
        store
            .read::<Positions>("solana_positions.json")
            .unwrap()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn solana_does_not_accept_evm_checkpoints_or_different_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut c = config();
    runner::bind(&store, &c).unwrap();
    c.hyperliquid.account = Some("0x0000000000000000000000000000000000000001".into());
    assert!(runner::bind(&store, &c).is_err());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .write("checkpoint.json", &json!({"schema":1}))
        .unwrap();
    assert!(runner::bind(&store, &config()).is_err());
}
#[test]
fn solana_transport_changes_preserve_economic_identity_but_live_does_not() {
    let mut c = config();
    let before = c.fingerprint();
    c.solana.grpc_url_env = "NEW_URL".into();
    c.solana.rpc_url_env = "NEW_RPC".into();
    assert_eq!(before, c.fingerprint());
    c.mode = Mode::Live;
    assert_ne!(before, c.fingerprint());
    assert!(c.validate().is_err());
}
#[test]
fn discrete_bin_roundtrip_conserves_inventory_at_execution_prices() {
    let price = dlmm::bin_price(-5600);
    let mut p = dlmm::PaperPosition::new("test", 100., 0.008, price, 1);
    let original = p.position();
    assert!((original.base * price + original.quote - 100.).abs() < 1e-8);
    p.mark(original.upper * 1.01);
    assert_eq!(p.position().base, 0.);
    assert!(p.position().quote > 0.);
    let after_up = p.position().quote;
    p.mark(original.lower * 0.99);
    assert_eq!(p.position().quote, 0.);
    p.mark(original.upper * 1.01);
    assert!((p.position().quote - after_up).abs() < 1e-8);
}
#[test]
fn paused_flat_paper_has_no_phantom_swap_or_transaction_cost() {
    let c = config();
    let mut p = Paper::new(&c.strategy);
    let d = Decision {
        state: "Paused".into(),
        reasons: vec![],
        lp: LpIntent::ExitToQuote,
        target_short_base: 0.,
        emergency: true,
    };
    for _ in 0..100 {
        p.apply(&d, &c.strategy, 100., 100., 1000, 0., 0.1, 0.00075)
            .unwrap();
    }
    assert_eq!(p.operations, 0);
    assert_eq!(p.portfolio(&c.strategy).equity(100.), 200.);
}
#[test]
fn solana_fee_apr_uses_token_deltas_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut c = config();
    c.solana.owner = Some("public-owner".into());
    let start = lp_maker::now_ms();
    let price = dlmm::bin_price(-5600);
    let mut s = Snapshot {
        slot: 100,
        block_hash: "a".into(),
        time_ms: start,
        observed_ms: start,
        active_bin: -5600,
        bin_step: 4,
        price,
        positions: vec![Position {
            address: "p".into(),
            revision: "unchanged".into(),
            lower_bin: -5610,
            upper_bin: -5590,
            lower: 99.,
            upper: 101.,
            base: 0.,
            quote: 100.,
            fee_base: 0.1,
            fee_quote: 0.,
        }],
        wallet: Wallet::default(),
        grpc: json!({}),
        fee_pct: 0.04,
    };
    lp_maker::solana::performance::observe_at(&c, &store, &s, start).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    s.slot += 1;
    s.block_hash = "b".into();
    s.time_ms += 60000;
    s.price *= 1.1;
    let result = lp_maker::solana::performance::observe_at(&c, &store, &s, s.time_ms).unwrap();
    assert_eq!(result["fee_apr_1h"]["fees_usdc"], 0.);
    assert_eq!(result["positions"][0]["holding"]["seconds"], 60);
    s.slot += 1;
    s.block_hash = "c".into();
    s.time_ms += 60000;
    s.positions[0].revision = "liquidity increased".into();
    s.positions[0].fee_base += 0.1;
    let result = lp_maker::solana::performance::observe_at(&c, &store, &s, s.time_ms).unwrap();
    assert!(result["fee_apr_1h"]["apr_pct"].is_null());
}

#[test]
fn recovery_recenter_keeps_current_fraction_in_solana_ledger() {
    let c = config();
    let mut p = Paper::new(&c.strategy);
    let deploy = Decision {
        state: "Recovering".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 0.25 },
        target_short_base: 0.,
        emergency: true,
    };
    p.apply(&deploy, &c.strategy, 100., 100., 1000, 0.25, 0., 0.00075)
        .unwrap();
    let recenter = Decision {
        lp: LpIntent::Recenter {
            layers: vec!["core".into()],
        },
        ..deploy
    };
    p.apply(&recenter, &c.strategy, 100., 100., 2000, 0.25, 0., 0.00075)
        .unwrap();
    let pos = p.portfolio(&c.strategy);
    let core = pos.positions.iter().find(|x| x.layer == "core").unwrap();
    assert!(core.base * 100. + core.quote < 21.);
    assert!(
        pos.positions
            .iter()
            .map(|x| x.base * 100. + x.quote)
            .sum::<f64>()
            < 31.
    );
}
#[test]
fn funding_changes_equity_once_without_multiplying_by_leverage() {
    let c = config();
    let mut p = Paper::new(&c.strategy);
    p.short = 0.5;
    p.funding(0.001, 100., 1000);
    p.funding(0.001, 100., 1000);
    assert!((p.funding_pnl - 0.05).abs() < 1e-9);
    assert!((p.hedge_equity - 60.05).abs() < 1e-9);
}

#[test]
fn solana_hedge_margin_uses_configured_leverage() {
    let mut c = config();
    c.hyperliquid.leverage = 1;
    assert!(c.validate().is_err());
    let mut p = Paper::new(&c.strategy);
    p.leverage = 1;
    p.hedge(0.6, 100., true, &c.strategy, 0.00075);
    assert_eq!(p.short, 0.);
    p.leverage = 3;
    p.hedge(0.6, 100., true, &c.strategy, 0.00075);
    assert_eq!(p.short, 0.6);
}

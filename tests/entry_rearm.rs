//! 仅内存行情与临时状态；不加载私钥、不请求网络、不执行交易。
use lp_maker::{
    config::{Config, Mode},
    domain::{Candle, LpIntent, MarketFrame, PoolSnapshot},
    engine::{
        Paper,
        entry_rearm::{self, Permit, Status},
    },
    recovery,
    store::Store,
    strategy::{EntryHistory, Phase, Strategy},
};
use serde_json::{Value, json};

fn fixture(store: &Store) -> (Config, Strategy, MarketFrame) {
    let mut c = Config::load("config/paper-200.toml").unwrap();
    c.mode = Mode::Live;
    let bars: Vec<_> = (0..200)
        .map(|i| {
            let price = 2500.0 * (1.0001_f64).powi(i);
            Candle {
                open_ms: 1_700_000_000_000 + i as u64 * 3_600_000,
                close_ms: 1_700_000_000_000 + (i as u64 + 1) * 3_600_000 - 1,
                open: price,
                close: price * 1.0001,
                high: price * 1.0002,
                low: price * 0.9999,
            }
        })
        .collect();
    let now = bars.last().unwrap().close_ms + 1;
    let price = bars.last().unwrap().close;
    let paper = Paper::new(&c);
    let strategy = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: now,
        peak_equity: 200.,
        ..Default::default()
    };
    recovery::save(store, &c, &strategy, &paper).unwrap();
    recovery::record_lp_history(store, json!({"source":"old_entry"})).unwrap();
    store.write("equity_baseline.json", &199.5).unwrap();
    let f = MarketFrame {
        now_ms: now,
        pool: PoolSnapshot {
            block: 1,
            block_hash: "test".into(),
            time_ms: now,
            price,
            tick: 0,
            tick_spacing: 1,
            liquidity: "1".into(),
            sqrt_price_x96: "1".into(),
            base_is_token0: true,
        },
        hedge_price: price,
        hedge_time_ms: now,
        candles: bars,
        portfolio: paper.portfolio,
    };
    (c, strategy, f)
}

#[test]
fn rearm_survives_restart_and_is_consumed_once_without_erasing_history() {
    let dir = tempfile::tempdir().unwrap();
    let (c, f, id, original);
    {
        let store = Store::open(dir.path()).unwrap();
        let (config, s, frame) = fixture(&store);
        original = store.read::<Value>("checkpoint.json").unwrap().unwrap();
        let request = entry_rearm::arm(&store, &config, &s, &frame.portfolio).unwrap();
        assert_eq!(
            entry_rearm::arm(&store, &config, &s, &frame.portfolio)
                .unwrap()
                .id,
            request.id
        );
        assert_eq!(
            store.read::<Value>("checkpoint.json").unwrap().unwrap(),
            original
        );
        (c, f, id) = (config, frame, request.id);
    }
    let store = Store::open(dir.path()).unwrap();
    let (mut s, paper) = recovery::load(&store, &c).unwrap();
    assert_eq!(s.entry_history, EntryHistory::Established);
    let mut without_permit = s.clone();
    assert_eq!(
        without_permit.evaluate(&c.strategy, &f).lp,
        LpIntent::ExitToQuote
    );
    let d = entry_rearm::evaluate(&store, &c, &mut s, &f).unwrap();
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 1. });
    assert_eq!(s.entry_history, EntryHistory::Established);
    assert_eq!(s.peak_equity, 200.);
    entry_rearm::consume(&store, &c, &d).unwrap();
    assert!(entry_rearm::consume(&store, &c, &d).is_err());
    // Simulate a crash after consuming the grant but before saving the new strategy/minting.
    let (mut old, _) = recovery::load(&store, &c).unwrap();
    assert_eq!(
        entry_rearm::evaluate(&store, &c, &mut old, &f).unwrap().lp,
        LpIntent::ExitToQuote
    );
    assert_eq!(
        store
            .read::<Permit>("entry_rearm.json")
            .unwrap()
            .unwrap()
            .status,
        Status::Consumed
    );
    assert_eq!(
        store
            .read::<Value>(&format!("entry_rearm_backup_{id}.json"))
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        store.read::<Value>("lp_history.json").unwrap().unwrap()["ever_opened"],
        true
    );
    assert_eq!(
        store.read::<f64>("equity_baseline.json").unwrap(),
        Some(199.5)
    );
    recovery::save(&store, &c, &s, &paper).unwrap();
}

#[test]
fn rearm_rejects_positions_pending_workflows_and_halt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (c, mut s, mut f) = fixture(&store);
    for (base, short) in [(0.01, 0.), (0., 0.01), (f64::NAN, 0.)] {
        f.portfolio.wallet_base = base;
        f.portfolio.short_base = short;
        assert!(entry_rearm::arm(&store, &c, &s, &f.portfolio).is_err());
    }
    f.portfolio.wallet_base = 0.;
    f.portfolio.short_base = 0.;
    for name in ["pending.json", "workflow.json", "hedge_order.json"] {
        store.write(name, &json!({"unresolved":true})).unwrap();
        assert!(entry_rearm::arm(&store, &c, &s, &f.portfolio).is_err());
        store.write(name, &Value::Null).unwrap();
    }
    s.phase = Phase::Halted;
    assert!(entry_rearm::arm(&store, &c, &s, &f.portfolio).is_err());
    assert!(store.read::<Permit>("entry_rearm.json").unwrap().is_none());
}

#[test]
fn rearm_keeps_downtrend_volatility_staleness_basis_and_drawdown_guards() {
    for scenario in ["downtrend", "volatility", "stale", "basis", "drawdown"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (c, mut s, mut f) = fixture(&store);
        entry_rearm::arm(&store, &c, &s, &f.portfolio).unwrap();
        match scenario {
            "downtrend" => {
                for (i, b) in f.candles.iter_mut().enumerate() {
                    b.open = 3000. * 0.999_f64.powi(i as i32);
                    b.close = b.open * 0.999;
                    b.high = b.open * 1.0001;
                    b.low = b.close * 0.9999;
                }
                f.pool.price = f.candles.last().unwrap().close;
                f.hedge_price = f.pool.price;
            }
            "volatility" => {
                let len = f.candles.len();
                for (i, b) in f.candles.iter_mut().enumerate().skip(len - 6) {
                    b.close = b.open * if i % 2 == 0 { 1.02 } else { 0.98 };
                    b.high = b.open.max(b.close) * 1.0001;
                    b.low = b.open.min(b.close) * 0.9999;
                }
                f.pool.price = f.candles.last().unwrap().close;
                f.hedge_price = f.pool.price;
            }
            "stale" => f.pool.time_ms = 1,
            "basis" => f.hedge_price *= 1.1,
            "drawdown" => s.peak_equity = 300.,
            _ => unreachable!(),
        }
        let d = entry_rearm::evaluate(&store, &c, &mut s, &f).unwrap();
        assert!(
            !matches!(d.lp, LpIntent::Deploy { .. }),
            "{scenario}: {d:?}"
        );
        assert_eq!(s.entry_history, EntryHistory::Established);
        if scenario == "drawdown" {
            assert_eq!(s.phase, Phase::Halted);
            assert_eq!(
                store
                    .read::<Permit>("entry_rearm.json")
                    .unwrap()
                    .unwrap()
                    .status,
                Status::Canceled
            );
        }
    }
}

#[test]
fn deferred_entry_keeps_permission_and_changed_inventory_cancels_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (c, mut s, mut f) = fixture(&store);
    entry_rearm::arm(&store, &c, &s, &f.portfolio).unwrap();
    let previous = s.clone();
    let mut d = entry_rearm::evaluate(&store, &c, &mut s, &f).unwrap();
    // 模拟保证金检查后的最终 Hold 决策；许可只可由最终 Deploy 消费。
    s = previous;
    d.lp = LpIntent::Hold;
    entry_rearm::consume(&store, &c, &d).unwrap();
    assert_eq!(
        store
            .read::<Permit>("entry_rearm.json")
            .unwrap()
            .unwrap()
            .status,
        Status::Armed
    );
    f.portfolio.wallet_base = 0.01;
    let d = entry_rearm::evaluate(&store, &c, &mut s, &f).unwrap();
    assert!(!matches!(d.lp, LpIntent::Deploy { .. }));
    assert_eq!(
        store
            .read::<Permit>("entry_rearm.json")
            .unwrap()
            .unwrap()
            .status,
        Status::Canceled
    );
}

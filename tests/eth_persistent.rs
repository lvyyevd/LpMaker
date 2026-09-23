//! 新 ETH 策略的风险、执行意图和重启边界；全部离线，不加载账户密钥。
use lp_maker::{
    config::Config,
    domain::*,
    engine::Paper,
    strategy::{
        EntryHistory, Phase, Strategy,
        eth::{self, Feature},
    },
};
fn config() -> Config {
    Config::load("config/eth-persistent-dd5-paper.toml").unwrap()
}
fn frame(c: &Config) -> MarketFrame {
    let now = 100 * 3_600_000;
    MarketFrame {
        now_ms: now,
        pool: PoolSnapshot {
            block: 1,
            block_hash: "test".into(),
            time_ms: now,
            price: 2000.,
            tick: 0,
            tick_spacing: 1,
            liquidity: "0".into(),
            sqrt_price_x96: "0".into(),
            base_is_token0: true,
        },
        hedge_price: 2000.,
        hedge_time_ms: now,
        candles: vec![],
        portfolio: Paper::new(c).portfolio,
    }
}
fn feature(f: &MarketFrame) -> Feature {
    Feature {
        time: f.now_ms - 1,
        close: 2000.,
        r1: 0.,
        r6: 0.,
        r24: 0.,
        r72: 0.,
        vol6: 0.001,
    }
}
fn advance(f: &mut MarketFrame, ms: u64) {
    f.now_ms += ms;
    f.pool.time_ms = f.now_ms;
    f.hedge_time_ms = f.now_ms;
}
fn position(f: &mut MarketFrame, c: &Config) {
    let (lo, hi) = lp_maker::math::range(f.pool.price, c.strategy.layers[0].half_width);
    let l = lp_maker::math::liquidity_for_value(120., lo, hi, f.pool.price).unwrap();
    let (base, quote) = lp_maker::math::amounts(l, lo, hi, f.pool.price);
    f.portfolio.wallet_quote = 0.;
    f.portfolio.positions = vec![LpPosition {
        layer: "persistent".into(),
        token_id: Some("test".into()),
        lower: lo,
        upper: hi,
        liquidity: l,
        raw_liquidity: "test".into(),
        unclaimed_base: 0.,
        unclaimed_quote: 0.,
        base,
        quote,
    }];
}
#[test]
fn legacy_serialization_and_new_config_are_isolated() {
    let old = Config::load("config/paper-200.toml").unwrap();
    assert!(
        serde_json::to_value(&old.strategy)
            .unwrap()
            .get("eth_persistent")
            .is_none()
    );
    assert!(
        serde_json::to_value(Strategy::default())
            .unwrap()
            .get("eth_persistent")
            .is_none()
    );
    let mut c = config();
    assert_ne!(c.state_dir, old.state_dir);
    c.hyperliquid.cross_margin = false;
    assert!(c.validate().is_err());
    c.hyperliquid.cross_margin = true;
    c.strategy.max_drawdown = 0.051;
    assert!(c.validate().is_err());
    c.strategy.max_drawdown = 0.05;
    c.hyperliquid.hedge_coin = "SOL".into();
    assert!(c.validate().is_err());
}
#[test]
fn first_entry_is_full_lp_budget_but_not_whole_account() {
    let c = config();
    let f = frame(&c);
    let mut s = Strategy::default();
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &feature(&f));
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 1. });
    let mut paper = Paper::new(&c);
    paper.apply(&d, &c, 2000., 2000., f.now_ms).unwrap();
    let p = &paper.portfolio.positions[0];
    assert!((p.lower - 200.).abs() < 1e-8 && (p.upper - 20000.).abs() < 1e-8);
    assert!((p.base * 2000. + p.quote - 119.4).abs() < 1e-8);
    assert!((paper.pending.unwrap().target - p.base * 0.5).abs() < 1e-8);
    assert_eq!(paper.portfolio.hedge_equity, 60.);
}
#[test]
fn all_risk_filters_exit_even_on_first_entry() {
    let c = config();
    let f = frame(&c);
    let safe = feature(&f);
    for x in [
        Feature {
            r1: -0.04,
            ..safe.clone()
        },
        Feature {
            r24: -0.10,
            ..safe.clone()
        },
        Feature {
            r72: -0.17,
            ..safe.clone()
        },
        Feature {
            vol6: 0.017,
            ..safe
        },
    ] {
        let mut s = Strategy::default();
        let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
        assert_eq!(d.lp, LpIntent::ExitToQuote);
        assert_eq!(s.phase, Phase::Paused);
    }
}
#[test]
fn halt_survives_restart_and_missing_candles() {
    let c = config();
    let mut f = frame(&c);
    f.portfolio.wallet_quote = 109.9;
    let mut s = Strategy::default();
    let d = eth::evaluate(&mut s, &c.strategy, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(s.phase, Phase::Halted);
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    f.portfolio.wallet_quote = 130.;
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &feature(&f));
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(s.phase, Phase::Halted);
}
#[test]
fn stale_prices_never_trigger_transactions() {
    let c = config();
    let mut f = frame(&c);
    let x = feature(&f);
    advance(&mut f, 61_000);
    f.pool.time_ms -= 61_000;
    let mut s = Strategy::default();
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(d.lp, LpIntent::Hold);
    assert!(d.reasons[0].starts_with("stale_or_invalid_data"));
}
#[test]
fn restart_keeps_cooldown_and_cannot_replay_healthy_hours() {
    let c = config();
    let mut f = frame(&c);
    position(&mut f, &c);
    let mut s = Strategy::default();
    let x = Feature {
        r24: -0.1,
        ..feature(&f)
    };
    eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    let deadline = s.eth_persistent.as_ref().unwrap().reentry_not_before_ms;
    let peak = s.peak_equity;
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    f.portfolio.positions.clear();
    f.portfolio.wallet_quote = 120.;
    advance(&mut f, 12 * 3_600_000);
    let x = feature(&f);
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(s.healthy_hours, 1);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(
        s.eth_persistent.as_ref().unwrap().reentry_not_before_ms,
        deadline
    );
    assert_eq!(s.peak_equity, peak);
    for _ in 0..59 {
        advance(&mut f, 60_000);
        eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    }
    advance(&mut f, 60_000);
    let x = feature(&f);
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(s.healthy_hours, 2);
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 1. });
}
#[test]
fn cooldown_expiry_alone_does_not_reenter() {
    let c = config();
    let mut f = frame(&c);
    let mut s = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: 1,
        ..Default::default()
    };
    let x = Feature {
        r6: -0.02,
        ..feature(&f)
    };
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(s.healthy_hours, 0);
    advance(&mut f, 3_600_000);
    let x = Feature {
        r6: -0.02,
        ..feature(&f)
    };
    assert_eq!(
        eth::evaluate_feature(&mut s, &c.strategy, &f, &x).lp,
        LpIntent::ExitToQuote
    );
}
#[test]
fn hedge_target_survives_restart_until_observed_fill() {
    let c = config();
    let mut f = frame(&c);
    position(&mut f, &c);
    let mut s = Strategy::default();
    let d = eth::evaluate_feature(&mut s, &c.strategy, &f, &feature(&f));
    assert!((d.target_short_base - 0.015).abs() < 1e-8);
    assert!(!d.emergency);
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    advance(&mut f, 15_000);
    let x = Feature {
        time: 100 * 3_600_000 - 1,
        ..feature(&f)
    };
    let next = eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(next.target_short_base, d.target_short_base);
    f.portfolio.short_base = d.target_short_base;
    advance(&mut f, 15_000);
    eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert!(
        s.eth_persistent
            .as_ref()
            .unwrap()
            .pending_hedge_target
            .is_none()
    );
}
#[test]
fn bad_future_and_missing_candles_are_rejected() {
    let mut a: Vec<_> = (0..74)
        .map(|i| Candle {
            open_ms: i * 3_600_000,
            close_ms: (i + 1) * 3_600_000 - 1,
            open: 2000.,
            close: 2000.,
            high: 2001.,
            low: 1999.,
        })
        .collect();
    let x = eth::latest_feature(&a, 73 * 3_600_000).unwrap();
    assert_eq!(x.time, 73 * 3_600_000 - 1);
    a[73].close = 1e9; // 未完成的未来 K 线不影响当前特征。
    assert_eq!(eth::latest_feature(&a, 73 * 3_600_000).unwrap().r1, 0.);
    a[72].open_ms += 1;
    assert!(eth::latest_feature(&a, 73 * 3_600_000).is_err());
}

#[test]
fn workflow_recovery_restarts_cooldown_without_resetting_peak() {
    let c = config();
    let mut f = frame(&c);
    let mut s = Strategy::default();
    position(&mut f, &c);
    eth::evaluate_feature(&mut s, &c.strategy, &f, &feature(&f));
    s.peak_equity = 205.;
    s.entry_history = EntryHistory::Established;
    s.phase = Phase::Paused;
    s.pause_since = f.now_ms; // 执行器对未完成流程的退出结果。
    s.eth_persistent.as_mut().unwrap().reentry_not_before_ms = 1;
    f.portfolio.positions.clear();
    f.portfolio.wallet_quote = 120.;
    advance(&mut f, 15_000);
    let x = Feature {
        time: 100 * 3_600_000 - 1,
        ..feature(&f)
    };
    eth::evaluate_feature(&mut s, &c.strategy, &f, &x);
    assert_eq!(
        s.eth_persistent.as_ref().unwrap().reentry_not_before_ms,
        s.pause_since + 12 * 3_600_000
    );
    assert_eq!(s.peak_equity, 205.);
    assert_eq!(s.phase, Phase::Paused);
}

use lp_maker::{
    domain::{Candle, LpIntent, MarketFrame},
    solana::{
        config::Config,
        dlmm::{Paper, PaperPosition, strategy_snapshot},
        regime::{self, Signals},
        research::{Bar, fee_for_bar},
    },
    strategy::{EntryHistory, Phase, Strategy},
};
const H: u64 = 3_600_000;
fn cfg() -> (Config, regime::Config) {
    let c = Config::load("config/solana.toml").unwrap();
    let r = regime::Config {
        ema_entry_hours: 48,
        ema_exit_hours: 48,
        momentum_hours: 24,
        min_momentum: 0.01,
        max_hourly_vol: 0.01,
        entry_vol_fraction: 1.,
        max_momentum: 1.,
        max_bar_range: 0.04,
        exit_buffer: 0.005,
        entry_confirm_hours: 3,
    };
    (c, r)
}
fn frame(c: &Config, paper: &Paper, t: u64, price: f64) -> MarketFrame {
    MarketFrame {
        now_ms: t,
        pool: strategy_snapshot(price, t),
        hedge_price: price,
        hedge_time_ms: t,
        candles: vec![],
        portfolio: paper.portfolio(&c.strategy),
    }
}
fn signal(t: u64) -> Signals {
    Signals {
        entry: true,
        exit: false,
        last_close: 100.,
        last_hour: t - 1,
        reason: "test".into(),
    }
}
#[test]
fn apr_accrues_on_actual_principal_and_only_while_in_range() {
    let (c, _) = cfg();
    let mut p = Paper::new(&c.strategy);
    let b = Bar {
        timestamp: 0,
        open: 100.,
        high: 100.,
        low: 100.,
        close: 100.,
    };
    assert_eq!(fee_for_bar(&p, &b, 0.4, 1.), 0.);
    p.positions
        .push(PaperPosition::new("core", 30., 0.025, 100., 1));
    assert!((fee_for_bar(&p, &b, 0.4, 1.) - 30. * 0.4 / (365. * 24.)).abs() < 1e-12);
    let outside = Bar { low: 90., ..b };
    assert_eq!(fee_for_bar(&p, &outside, 0.4, 1.), 0.);
}
#[test]
fn repeated_messages_cannot_satisfy_hourly_entry_confirmation() {
    let (c, r) = cfg();
    let p = Paper::new(&c.strategy);
    let mut state = Strategy::default();
    for _ in 0..100 {
        let f = frame(&c, &p, 100 * H, 100.);
        let d =
            regime::evaluate_with_signals(&r, &c.strategy, &mut state, &f, Some(&signal(100 * H)));
        assert_ne!(d.lp, LpIntent::Deploy { fraction: 1. });
    }
    assert_eq!(state.healthy_hours, 1);
    for h in [101, 102] {
        let f = frame(&c, &p, h * H, 100.);
        regime::evaluate_with_signals(&r, &c.strategy, &mut state, &f, Some(&signal(h * H)));
    }
    assert_eq!(state.phase, Phase::Active);
}
#[test]
fn risk_exit_and_restart_keep_cooldown_and_halt_latched() {
    let (c, r) = cfg();
    let mut p = Paper::new(&c.strategy);
    p.cash -= 100.;
    p.positions
        .push(PaperPosition::new("core", 100., 0.1, 100., 1));
    let mut state = Strategy {
        phase: Phase::Active,
        entry_history: EntryHistory::Established,
        ..Strategy::default()
    };
    let mut sig = signal(100 * H);
    sig.exit = true;
    sig.reason = "sol_high_volatility".into();
    let d = regime::evaluate_with_signals(
        &r,
        &c.strategy,
        &mut state,
        &frame(&c, &p, 100 * H, 100.),
        Some(&sig),
    );
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(state.pause_since, 100 * H);
    let mut state: Strategy = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    lp_maker::recovery::invalidate_observation_streaks(&mut state);
    p.positions.clear();
    p.cash = c.strategy.lp_budget;
    for h in [101, 102, 103] {
        let d = regime::evaluate_with_signals(
            &r,
            &c.strategy,
            &mut state,
            &frame(&c, &p, h * H, 100.),
            Some(&signal(h * H)),
        );
        assert_eq!(d.lp, LpIntent::ExitToQuote);
    }
    state.phase = Phase::Halted;
    let d = regime::evaluate_with_signals(
        &r,
        &c.strategy,
        &mut state,
        &frame(&c, &p, 200 * H, 100.),
        Some(&signal(200 * H)),
    );
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(state.phase, Phase::Halted);
}
#[test]
fn falling_market_and_large_volatility_are_not_entry_signals() {
    let (c, r) = cfg();
    let bars: Vec<_> = (0..500)
        .map(|i| {
            let p = 100. * 0.999_f64.powi(i);
            Candle {
                open_ms: i as u64 * H,
                close_ms: (i as u64 + 1) * H - 1,
                open: p,
                close: p,
                high: p * 1.001,
                low: p * 0.999,
            }
        })
        .collect();
    let sig = regime::signals(&r, &c.strategy, &bars, 500 * H).unwrap();
    assert!(sig.exit);
    assert!(!sig.entry);
    let mut up: Vec<_> = (0..500)
        .map(|i| {
            let p = 100. * 1.001_f64.powi(i);
            Candle {
                open_ms: i as u64 * H,
                close_ms: (i as u64 + 1) * H - 1,
                open: p,
                close: p,
                high: p * 1.001,
                low: p * 0.999,
            }
        })
        .collect();
    assert!(
        regime::signals(&r, &c.strategy, &up, 500 * H)
            .unwrap()
            .entry
    );
    up[499].high *= 1.10;
    assert!(regime::signals(&r, &c.strategy, &up, 500 * H).unwrap().exit);
}
#[test]
fn unclosed_future_candles_cannot_change_signal() {
    let (c, r) = cfg();
    let bars: Vec<_> = (0..600)
        .map(|i| {
            let p = 100. * 1.001_f64.powi(i);
            Candle {
                open_ms: i as u64 * H,
                close_ms: (i as u64 + 1) * H - 1,
                open: p,
                close: p,
                high: p,
                low: p,
            }
        })
        .collect();
    let a = regime::signals(&r, &c.strategy, &bars, 500 * H).unwrap();
    let mut changed = bars.clone();
    for b in &mut changed[500..] {
        b.close = 1e9;
        b.high = 1e10;
        b.low = 0.0001;
    }
    let b = regime::signals(&r, &c.strategy, &changed, 500 * H).unwrap();
    assert_eq!(
        (a.entry, a.exit, a.last_close),
        (b.entry, b.exit, b.last_close)
    );
}
#[test]
fn legacy_fingerprint_stays_unchanged_and_regime_requires_own_state() {
    let (mut c, r) = cfg();
    let old = c.fingerprint();
    assert!(old.get("regime").is_none());
    c.regime = Some(r);
    assert_ne!(c.fingerprint(), old);
    let dir = tempfile::tempdir().unwrap();
    let store = lp_maker::store::Store::open(dir.path()).unwrap();
    store.write("solana_identity.json", &old).unwrap();
    assert!(lp_maker::solana::runner::bind(&store, &c).is_err());
}

#[test]
fn entry_volatility_hysteresis_does_not_force_an_early_exit() {
    let (c, mut r) = cfg();
    r.min_momentum = 0.;
    r.max_hourly_vol = 0.005;
    let bars: Vec<_> = (0..500)
        .map(|i| {
            let p = 100. * 1.0003_f64.powi(i) * if i % 2 == 0 { 0.999 } else { 1.0015 };
            Candle {
                open_ms: i as u64 * H,
                close_ms: (i as u64 + 1) * H - 1,
                open: p,
                close: p,
                high: p,
                low: p,
            }
        })
        .collect();
    let normal = regime::signals(&r, &c.strategy, &bars, 500 * H).unwrap();
    assert!(normal.entry);
    assert!(!normal.exit);
    r.entry_vol_fraction = 0.4;
    let calm_entry = regime::signals(&r, &c.strategy, &bars, 500 * H).unwrap();
    assert!(!calm_entry.entry);
    assert!(!calm_entry.exit);
}

#[test]
fn changing_future_prices_and_funding_cannot_change_an_earlier_replay() {
    use lp_maker::solana::research::{self, Data, Parameters};
    let (c, r) = cfg();
    let c = research::configure(
        &c,
        &Parameters {
            width: 0.1,
            hedge_ratio: 0.25,
            cooldown_hours: 6,
            regime: r,
        },
    );
    let hedge: Vec<_> = (0..650)
        .map(|i| {
            let p = 100. * 1.001_f64.powi(i);
            Candle {
                open_ms: i as u64 * H,
                close_ms: (i as u64 + 1) * H - 1,
                open: p,
                close: p * 1.0005,
                high: p * 1.001,
                low: p,
            }
        })
        .collect();
    let pool = hedge[500..]
        .iter()
        .map(|b| Bar {
            timestamp: b.open_ms / 1000,
            open: b.open,
            close: b.close,
            high: b.high,
            low: b.low,
        })
        .collect();
    let mut data = Data {
        step_ms: H,
        intrabar_stress: false,
        pool,
        hedge,
        funding: (500..650).map(|h| (h * H, 0.00001)).collect(),
        start: 500 * H,
        end: 650 * H,
    };
    let sig = research::cached_signals(&c, &data);
    let original = research::simulate(&c, &data, &sig, 500 * H, 550 * H, 0.4, 1., false).unwrap();
    for b in &mut data.pool[50..] {
        b.open *= 10.;
        b.high *= 10.;
        b.low *= 10.;
        b.close *= 10.;
    }
    for b in &mut data.hedge[550..] {
        b.open *= 10.;
        b.high *= 10.;
        b.low *= 10.;
        b.close *= 10.;
    }
    for (_, rate) in data.funding.range_mut(550 * H..) {
        *rate = 0.5;
    }
    let sig = research::cached_signals(&c, &data);
    let changed = research::simulate(&c, &data, &sig, 500 * H, 550 * H, 0.4, 1., false).unwrap();
    assert_eq!(original.pnl, changed.pnl);
    assert_eq!(original.lp_fees, changed.lp_fees);
    assert_eq!(original.operations, changed.operations);
}

use lp_maker::{
    domain::Candle,
    solana::{
        config::Config,
        regime::Signals,
        research::{self, AccountModel, Bar, Data, training},
    },
};
const H: u64 = 3_600_000;
fn config() -> Config {
    Config::load("config/solana-regime.toml").unwrap()
}
fn data(prices: &[f64]) -> Data {
    let start = 500 * H;
    let pool: Vec<_> = prices
        .iter()
        .enumerate()
        .map(|(i, p)| Bar {
            timestamp: (start + i as u64 * H) / 1000,
            open: *p,
            high: *p,
            low: *p,
            close: *p,
        })
        .collect();
    let hedge = pool
        .iter()
        .map(|b| Candle {
            open_ms: b.timestamp * 1000,
            close_ms: b.timestamp * 1000 + H - 1,
            open: b.open,
            high: b.high,
            low: b.low,
            close: b.close,
        })
        .collect();
    Data {
        step_ms: H,
        intrabar_stress: false,
        pool,
        hedge,
        funding: (0..prices.len())
            .map(|i| (start + i as u64 * H, 0.))
            .collect(),
        start,
        end: start + prices.len() as u64 * H,
    }
}
#[test]
fn idle_native_reserve_is_marked_to_market_and_roundtrip_costs_are_charged() {
    let c = config();
    let d = data(&[100., 80.]);
    let o = research::simulate_account(
        &c,
        &d,
        &[None, None],
        d.start,
        d.end,
        0.4,
        1.,
        true,
        AccountModel { native_sol: 0.1 },
    )
    .unwrap();
    assert!((o.native_reserve_pnl + 2.).abs() < 1e-9);
    assert!((o.native_reserve_cost - (10. + 8.) * 0.0034).abs() < 1e-9);
    assert!((o.pnl - (-2. - 18. * 0.0034)).abs() < 1e-9);
    assert_eq!(o.lp_fees, 0.);
    assert_eq!(o.operations, 0);
}
#[test]
fn rent_is_part_of_the_same_capital_and_cannot_be_borrowed_implicitly() {
    let c = config();
    let d = data(&[100.]);
    assert!(
        research::simulate_account(
            &c,
            &d,
            &[None],
            d.start,
            d.end,
            0.4,
            1.,
            false,
            AccountModel { native_sol: 1. }
        )
        .is_err()
    );
}
#[test]
fn parameter_search_preserves_market_safety_and_three_times_margin_capacity() {
    let base = config();
    for i in 0..1000 {
        let trial = training::candidate(250919, i, 200.);
        let c = training::trial_config(&base, &trial);
        c.validate().unwrap();
        assert!(
            (c.strategy.lp_budget + c.strategy.hedge_collateral + c.strategy.reserve - 200.).abs()
                < 1e-8
        );
        assert!(c.strategy.lp_budget / 3. <= c.strategy.hedge_collateral * 0.9);
        assert_eq!(c.strategy.max_drawdown, base.strategy.max_drawdown);
        assert!(c.strategy.cooldown_hours >= 6);
        assert_eq!(c.strategy.fast_drop_1h, 0.03);
        assert_eq!(
            serde_json::to_string(&trial).unwrap(),
            serde_json::to_string(&training::candidate(250919, i, 200.)).unwrap()
        );
    }
}
#[test]
fn adverse_perp_wick_disqualifies_an_apparently_flat_profit_curve() {
    let mut c = config();
    c.strategy.inside_hedge_ratio = 1.;
    c.regime.as_mut().unwrap().entry_confirm_hours = 1;
    let mut d = data(&[100., 100.]);
    d.hedge[0].high = 140.;
    let sig: Vec<_> = d
        .pool
        .iter()
        .map(|b| {
            Some(Signals {
                entry: true,
                exit: false,
                last_close: 100.,
                last_hour: b.timestamp * 1000 - 1,
                reason: "test".into(),
            })
        })
        .collect();
    let o = research::simulate_account(
        &c,
        &d,
        &sig,
        d.start,
        d.end,
        0.4,
        1.,
        false,
        AccountModel { native_sol: 0.1 },
    )
    .unwrap();
    assert!(
        o.model_rejections
            .iter()
            .any(|r| r == "isolated_margin_intrabar_buffer")
    );
}
#[test]
fn later_prices_do_not_change_an_earlier_account_replay() {
    let c = config();
    let mut d = data(&[100., 101., 102., 103.]);
    let sig = vec![None; 4];
    let a = research::simulate_account(
        &c,
        &d,
        &sig,
        d.start,
        d.start + 2 * H,
        0.4,
        1.,
        false,
        AccountModel { native_sol: 0.1 },
    )
    .unwrap();
    for b in &mut d.pool[2..] {
        b.open = 0.1;
        b.close = 0.1;
        b.low = 0.1;
        b.high = 1e9;
    }
    for b in &mut d.hedge[2..] {
        b.high = 1e9;
    }
    let b = research::simulate_account(
        &c,
        &d,
        &sig,
        d.start,
        d.start + 2 * H,
        0.4,
        1.,
        false,
        AccountModel { native_sol: 0.1 },
    )
    .unwrap();
    assert_eq!(a.pnl, b.pnl);
    assert_eq!(a.model_rejections, b.model_rejections);
}

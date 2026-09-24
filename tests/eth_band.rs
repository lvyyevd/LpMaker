//! 新候选的入场/对冲/暂停/恢复/重启均离线测试，绝不读取私钥。
use lp_maker::{
    config::Config,
    domain::*,
    engine::{Paper, inventory_hedge_target},
    strategy::{
        EntryHistory, Phase, Strategy,
        eth::{self, Feature, band},
    },
};
const H: u64 = 3_600_000;
fn config() -> Config {
    Config::load("config/eth-band-paper.toml").unwrap()
}
fn frame(c: &Config) -> MarketFrame {
    MarketFrame {
        now_ms: 100 * H,
        pool: PoolSnapshot {
            block: 1,
            block_hash: "test".into(),
            time_ms: 100 * H,
            price: 2000.,
            tick: 0,
            tick_spacing: 1,
            liquidity: "0".into(),
            sqrt_price_x96: "0".into(),
            base_is_token0: true,
        },
        hedge_price: 2000.,
        hedge_time_ms: 100 * H,
        candles: vec![],
        portfolio: Paper::new(c).portfolio,
    }
}
fn x(f: &MarketFrame) -> Feature {
    Feature {
        time: f.now_ms / H * H - 1,
        close: f.pool.price,
        r1: 0.,
        r6: 0.,
        r24: 0.01,
        r72: 0.,
        vol6: 0.001,
    }
}
fn advance(f: &mut MarketFrame, ms: u64) {
    f.now_ms += ms;
    f.pool.time_ms = f.now_ms;
    f.hedge_time_ms = f.now_ms;
}
fn eval(s: &mut Strategy, c: &Config, f: &MarketFrame) -> Decision {
    eth::evaluate_feature(s, &c.strategy, f, &x(f))
}
fn deploy(s: &mut Strategy, c: &Config, f: &mut MarketFrame) {
    let d = eval(s, c, f);
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 1. });
    let widths = band::config(&c.strategy).unwrap().widths(&x(f));
    let mut paper = Paper::new(c);
    paper.portfolio = f.portfolio.clone();
    paper
        .apply_with_widths(&d, c, f.pool.price, f.hedge_price, f.now_ms, Some(widths))
        .unwrap();
    f.portfolio = paper.portfolio;
    advance(f, 15_000);
    eval(s, c, f);
}
#[test]
fn first_entry_arithmetic_range_and_budget_not_geometric_or_whole_wallet() {
    let c = config();
    let mut f = frame(&c);
    f.portfolio.wallet_quote = 2120.;
    let mut s = Strategy::default();
    deploy(&mut s, &c, &mut f);
    let p = &f.portfolio.positions[0];
    assert!((p.lower - 1843.5).abs() < 1e-8);
    assert_eq!(p.upper, 2160.);
    assert!(p.base * 2000. + p.quote <= 120.);
    assert!(f.portfolio.wallet_quote > 1990.);
    assert_eq!(s.peak_equity, 200.);
    assert_eq!(
        band::config(&c.strategy).unwrap().widths(&Feature {
            r24: -0.01,
            ..x(&f)
        }),
        (0.08, 0.07825)
    );
}
#[test]
fn band_target_and_collateral_limit_are_from_actual_inventory() {
    let c = config();
    let mut p = Paper::new(&c).portfolio;
    p.wallet_base = 0.03;
    assert!((inventory_hedge_target(&c.strategy, &p, 2000.) - 0.00638).abs() < 1e-10);
    p.short_base = 0.006;
    assert!((inventory_hedge_target(&c.strategy, &p, 2000.) - 0.00638).abs() < 1e-10);
    p.short_base = 0.01;
    assert_eq!(inventory_hedge_target(&c.strategy, &p, 2000.), 0.01);
    p.short_base = 0.07;
    assert!((inventory_hedge_target(&c.strategy, &p, 2000.) - 0.06).abs() < 1e-9);
    p.short_base = 0.;
    p.wallet_base = 0.1;
    p.hedge_equity = 10.;
    assert!((inventory_hedge_target(&c.strategy, &p, 2000.) * 2000. - 25.5).abs() < 1e-8);
}
#[test]
fn partial_hedge_recomputes_until_fill_and_survives_restart() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    deploy(&mut s, &c, &mut f);
    f.portfolio.wallet_base += 0.012;
    f.portfolio.wallet_quote -= 24.; // move quote to ETH, equity unchanged
    let d = eval(&mut s, &c, &f);
    assert_eq!(d.target_short_base, 0.);
    advance(&mut f, 2 * H);
    let d = eval(&mut s, &c, &f);
    assert!(d.target_short_base > 0.015);
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    advance(&mut f, 15_000);
    assert_eq!(eval(&mut s, &c, &f).target_short_base, d.target_short_base);
    f.portfolio.short_base = d.target_short_base;
    advance(&mut f, 15_000);
    eval(&mut s, &c, &f);
    assert!(s.eth_persistent.unwrap().pending_hedge_target.is_none());
}
#[test]
fn episode_exit_healthy_resume_keeps_account_peak_and_origin() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    deploy(&mut s, &c, &mut f);
    f.portfolio.wallet_quote -= 4.1;
    advance(&mut f, 15_000);
    let d = eval(&mut s, &c, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert!(d.reasons.contains(&"early_drawdown_pause".into()));
    let equity = f.portfolio.equity(f.pool.price);
    let origin = s
        .eth_persistent
        .as_ref()
        .unwrap()
        .band
        .as_ref()
        .unwrap()
        .account_origin_equity;
    f.portfolio.positions.clear();
    f.portfolio.wallet_base = 0.;
    f.portfolio.wallet_quote = equity - 80.;
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    for hour in 1..=4 {
        // 连续观察而非只在4小时后重启补算历史。
        for _ in 0..240 {
            advance(&mut f, 15_000);
            let d = eval(&mut s, &c, &f);
            if hour < 4 {
                assert_ne!(d.lp, LpIntent::Deploy { fraction: 1. });
            }
        }
    }
    assert_eq!(s.phase, Phase::Active);
    assert_eq!(s.peak_equity, 200.);
    assert_eq!(
        s.eth_persistent
            .as_ref()
            .unwrap()
            .band
            .as_ref()
            .unwrap()
            .account_origin_equity,
        origin
    );
    assert!(s.eth_persistent.unwrap().band.unwrap().risk_equity < 196.);
}
#[test]
fn five_percent_halt_is_not_cleared_by_recovery_or_restart() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    deploy(&mut s, &c, &mut f);
    f.portfolio.wallet_quote -= 10.1;
    advance(&mut f, 15_000);
    assert_eq!(eval(&mut s, &c, &f).lp, LpIntent::ExitToQuote);
    assert_eq!(s.phase, Phase::Halted);
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    f.portfolio.wallet_quote += 15.;
    advance(&mut f, 8 * H);
    assert_eq!(eval(&mut s, &c, &f).lp, LpIntent::ExitToQuote);
    assert_eq!(s.phase, Phase::Halted);
}
#[test]
fn outside_confirmation_needs_48h_and_does_not_disable_emergency() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    deploy(&mut s, &c, &mut f);
    f.portfolio.positions[0].upper = 1999.; // no price PnL: isolate ordinary out-of-range confirmation
    for _ in 0..16 {
        for _ in 0..60 {
            advance(&mut f, 15_000);
            let d = eval(&mut s, &c, &f);
            assert_eq!(d.lp, LpIntent::Hold);
        }
    }
    s.eth_persistent
        .as_mut()
        .unwrap()
        .band
        .as_mut()
        .unwrap()
        .entered_ms = f.now_ms - 48 * H;
    let mut recentered = false;
    for _ in 0..60 {
        advance(&mut f, 15_000);
        recentered |= matches!(eval(&mut s, &c, &f).lp, LpIntent::Recenter { .. });
    }
    assert!(recentered);
    let danger = Feature { r1: -0.04, ..x(&f) };
    assert_eq!(
        eth::evaluate_feature(&mut s, &c.strategy, &f, &danger).lp,
        LpIntent::ExitToQuote
    );
}
#[test]
fn interrupted_observation_does_not_accumulate_healthy_hours_or_reset_loss_basis() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    deploy(&mut s, &c, &mut f);
    let feature = Feature { r24: -0.1, ..x(&f) };
    eth::evaluate_feature(&mut s, &c.strategy, &f, &feature);
    f.portfolio.positions.clear();
    f.portfolio.wallet_quote = 120.;
    f.portfolio.wallet_base = 0.;
    advance(&mut f, 8 * H);
    assert_eq!(eval(&mut s, &c, &f).lp, LpIntent::ExitToQuote);
    assert_eq!(s.healthy_hours, 1);
    let entered = s
        .eth_persistent
        .as_ref()
        .unwrap()
        .band
        .as_ref()
        .unwrap()
        .entered_ms;
    advance(&mut f, 15_000);
    eval(&mut s, &c, &f);
    assert_eq!(s.healthy_hours, 1);
    assert_eq!(s.eth_persistent.unwrap().band.unwrap().entered_ms, entered);
}
#[test]
fn strict_config_keeps_old_profiles_and_other_chains_unchanged() {
    let old = Config::load("config/eth-persistent-dd5-paper.toml").unwrap();
    assert!(
        serde_json::to_value(old.strategy.eth_persistent)
            .unwrap()
            .get("band")
            .is_none()
    );
    let mut c = config();
    c.strategy
        .eth_persistent
        .as_mut()
        .unwrap()
        .band
        .as_mut()
        .unwrap()
        .upper_width = 0.081;
    assert!(c.validate().is_err());
    let mut c = config();
    c.liquidity.chain_id = 8453;
    assert!(c.validate().is_err());
    let s:Strategy=serde_json::from_value(serde_json::json!({"phase":"Paused","peak_equity":200.,"pause_since":1,"last_hour":0,"healthy_hours":0,"fraction":0.,"last_scale":0,"last_decision_bar":0,"layers":{}})).unwrap();
    assert_eq!(s.entry_history, EntryHistory::LegacyUnknown);
}

#[test]
fn workflow_wait_cannot_mint_after_budget_risk_changes() {
    let c = config();
    let mut s = Strategy::default();
    let f = frame(&c);
    eval(&mut s, &c, &f);
    assert!(band::entry_equity_safe(&s, &c.strategy, 199.9));
    assert!(!band::entry_equity_safe(&s, &c.strategy, 195.9));
    assert!(!band::entry_equity_safe(&s, &c.strategy, f64::NAN));
}
#[test]
fn missing_hourly_history_still_checks_same_managed_budget() {
    let c = config();
    let mut s = Strategy::default();
    let mut f = frame(&c);
    f.portfolio.wallet_quote += 2000.;
    eth::evaluate(&mut s, &c.strategy, &f);
    assert_eq!(s.peak_equity, 200.);
    f.portfolio.wallet_quote -= 10.1;
    assert_eq!(
        eth::evaluate(&mut s, &c.strategy, &f).lp,
        LpIntent::ExitToQuote
    );
    assert_eq!(s.phase, Phase::Halted);
}

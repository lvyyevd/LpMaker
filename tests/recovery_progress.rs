//! 只用内存行情和临时状态目录，验证短暂观察故障不会冒充风险或累计新小时。
use lp_maker::{
    config::Config,
    domain::{Candle, LpIntent, MarketFrame, PoolSnapshot},
    engine::{Paper, entry_rearm},
    recovery,
    store::Store,
    strategy::{EntryHistory, Phase, Strategy, progress::RECHECK_GRACE_MS},
};
use serde_json::{Value, json};

fn config() -> Config {
    Config::load("config/paper-200.toml").unwrap()
}
fn frame(c: &Config, count: usize) -> MarketFrame {
    let candles: Vec<_> = (0..count)
        .map(|i| {
            let open = 2500.0 * 1.0001_f64.powi(i as i32);
            let close = open * 1.0001;
            Candle {
                open_ms: 1_700_000_000_000 + i as u64 * 3_600_000,
                close_ms: 1_700_000_000_000 + (i as u64 + 1) * 3_600_000 - 1,
                open,
                close,
                high: close * 1.0001,
                low: open * 0.9999,
            }
        })
        .collect();
    let last = candles.last().unwrap();
    let now = last.close_ms + 1;
    MarketFrame {
        now_ms: now,
        pool: PoolSnapshot {
            block: count as u64,
            block_hash: "test".into(),
            time_ms: now,
            price: last.close,
            tick: 0,
            tick_spacing: 1,
            liquidity: "1".into(),
            sqrt_price_x96: "1".into(),
            base_is_token0: true,
        },
        hedge_price: last.close,
        hedge_time_ms: now,
        candles,
        portfolio: Paper::new(c).portfolio,
    }
}
fn advance(f: &mut MarketFrame, ms: u64) {
    f.now_ms += ms;
    f.pool.time_ms = f.now_ms;
    f.hedge_time_ms = f.now_ms;
}
fn progress() -> (Config, Strategy, MarketFrame) {
    let c = config();
    let f = frame(&c, 203);
    let mut s = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: f.now_ms - 24 * 3_600_000,
        ..Default::default()
    };
    for n in 200..=203 {
        s.evaluate_with_continuity(&c.strategy, &frame(&c, n));
    }
    assert_eq!(s.healthy_hours, 4);
    (c, s, f)
}
fn reset_reason(s: &Strategy) -> &str {
    &s.recovery_progress.last_reset.as_ref().unwrap().reason
}

#[test]
fn short_restart_roundtrip_preserves_progress_and_never_counts_same_bar_twice() {
    let (c, s, mut f) = progress();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    recovery::save(&store, &c, &s, &Paper::new(&c)).unwrap();
    let (mut restored, _) = recovery::load(&store, &c).unwrap();
    recovery::prepare_observation_revalidation(&mut restored);
    assert_eq!(restored.healthy_hours, 4);
    advance(&mut f, 45_000);
    let d = entry_rearm::evaluate(&store, &c, &mut restored, &f).unwrap();
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(restored.healthy_hours, 4);
    assert_eq!(restored.pause_since, s.pause_since);
    assert_eq!(restored.peak_equity, s.peak_equity);
    assert!(
        d.reasons
            .iter()
            .any(|r| r.starts_with("recovery_progress_retained:"))
    );
    for _ in 0..50 {
        restored.evaluate_with_continuity(&c.strategy, &f);
    }
    assert_eq!(restored.healthy_hours, 4);
    restored.evaluate_with_continuity(&c.strategy, &frame(&c, 204));
    assert_eq!(restored.healthy_hours, 5);
    let d = restored.evaluate_with_continuity(&c.strategy, &frame(&c, 205));
    assert_eq!(
        d.lp,
        LpIntent::Deploy {
            fraction: c.strategy.recovery_fraction
        }
    );
}

#[test]
fn retry_saves_do_not_extend_five_minute_window_or_change_cooldown() {
    let (c, mut s, mut f) = progress();
    let observed = s.recovery_progress.observed_ms;
    let pause = s.pause_since;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for _ in 0..4 {
        recovery::prepare_observation_revalidation(&mut s);
        recovery::save(&store, &c, &s, &Paper::new(&c)).unwrap();
        s = recovery::load(&store, &c).unwrap().0;
    }
    assert_eq!(s.recovery_progress.observed_ms, observed);
    advance(&mut f, RECHECK_GRACE_MS + 1);
    s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(s.healthy_hours, 0);
    assert_eq!(s.pause_since, pause);
    assert_eq!(reset_reason(&s), "observation_gap_over_grace");
}

#[test]
fn grace_boundary_retains_progress_but_clock_reversal_does_not() {
    let (c, s, f) = progress();
    for (delta, expected) in [(RECHECK_GRACE_MS, 4), (RECHECK_GRACE_MS + 1, 0)] {
        let mut s = s.clone();
        let mut f = f.clone();
        recovery::prepare_observation_revalidation(&mut s);
        advance(&mut f, delta);
        s.evaluate_with_continuity(&c.strategy, &f);
        assert_eq!(s.healthy_hours, expected);
    }
    let mut s = s;
    let mut f = f;
    recovery::prepare_observation_revalidation(&mut s);
    s.recovery_progress.observed_ms += 1;
    advance(&mut f, 0);
    s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(s.healthy_hours, 0);
    assert_eq!(reset_reason(&s), "observation_clock_reversed");
}

#[test]
fn legacy_checkpoint_is_accepted_without_inventing_continuity_evidence() {
    let (c, s, mut f) = progress();
    let mut old = serde_json::to_value(&s).unwrap();
    old.as_object_mut().unwrap().remove("recovery_progress");
    let mut restored: Strategy = serde_json::from_value(old).unwrap();
    recovery::prepare_observation_revalidation(&mut restored);
    advance(&mut f, 20_000);
    let d = restored.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(restored.healthy_hours, 0);
    assert_eq!(reset_reason(&restored), "legacy_without_observation");
    assert_eq!(restored.entry_history, EntryHistory::Established);
    assert_eq!(restored.pause_since, s.pause_since);
}

#[test]
fn revised_anchor_candle_and_missing_hours_cannot_preserve_old_progress() {
    let (c, s, f) = progress();
    let mut revised = f.clone();
    revised.candles.last_mut().unwrap().high *= 1.001;
    let mut a = s.clone();
    recovery::prepare_observation_revalidation(&mut a);
    a.evaluate_with_continuity(&c.strategy, &revised);
    assert_eq!(a.healthy_hours, 0);
    assert_eq!(reset_reason(&a), "completed_candle_changed");
    let mut a = s;
    recovery::prepare_observation_revalidation(&mut a);
    let mut gap = frame(&c, 206);
    // 即使外部保存时间看似很新，已完成小时跨越仍不能补计。
    a.recovery_progress.observed_ms = gap.now_ms - 1000;
    advance(&mut gap, 0);
    a.evaluate_with_continuity(&c.strategy, &gap);
    assert_eq!(a.healthy_hours, 1);
    assert_eq!(reset_reason(&a), "hourly_gap");
}

#[test]
fn stale_or_invalid_observation_freezes_progress_and_cannot_deploy() {
    let (c, mut s, mut f) = progress();
    let pause = s.pause_since;
    let observed = s.recovery_progress.observed_ms;
    advance(&mut f, 30_000);
    f.pool.time_ms = f.now_ms - 120_000;
    let d = s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::Hold);
    assert_eq!(s.healthy_hours, 4);
    assert_eq!(s.pause_since, pause);
    assert_eq!(s.recovery_progress.observed_ms, observed);
    assert!(s.recovery_progress.pending_revalidation);
    f.pool.time_ms = f.now_ms;
    s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(s.healthy_hours, 4);
    assert!(!s.recovery_progress.pending_revalidation);
}

#[test]
fn real_fast_drop_still_resets_progress_and_keeps_original_reset_time() {
    let (c, mut s, mut f) = progress();
    recovery::prepare_observation_revalidation(&mut s);
    advance(&mut f, 30_000);
    f.pool.price *= 0.98;
    f.hedge_price = f.pool.price;
    let d = s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(s.healthy_hours, 0);
    assert_eq!(reset_reason(&s), "fast_drop");
    assert_eq!(
        s.recovery_progress
            .last_reset
            .as_ref()
            .unwrap()
            .previous_hours,
        4
    );
    let first = s.recovery_progress.last_reset.as_ref().unwrap().time_ms;
    advance(&mut f, 20_000);
    s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(
        s.recovery_progress.last_reset.as_ref().unwrap().time_ms,
        first
    );
}

#[test]
fn halt_and_unfinished_workflow_never_reuse_observation_grace() {
    let (c, s, f) = progress();
    let mut halted = s.clone();
    halted.phase = Phase::Halted;
    recovery::prepare_observation_revalidation(&mut halted);
    assert_eq!(
        halted.evaluate_with_continuity(&c.strategy, &f).lp,
        LpIntent::ExitToQuote
    );
    assert_eq!(halted.phase, Phase::Halted);
    let mut workflow = s;
    recovery::finish_workflow_recovery(&mut workflow);
    assert_eq!(workflow.healthy_hours, 0);
    assert!(workflow.recovery_progress.anchor.is_none());
    assert_eq!(reset_reason(&workflow), "workflow_or_inventory_changed");
}

#[test]
fn preserved_count_alone_does_not_bypass_current_market_conditions() {
    let (c, mut s, mut f) = progress();
    s.healthy_hours = c.strategy.resume_healthy_hours;
    // 无快跌/下降趋势，但低于 EMA：保存的小时数不单独授权立即入场。
    f.pool.price *= 0.998;
    f.hedge_price = f.pool.price;
    let d = s.evaluate_with_continuity(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert_eq!(s.phase, Phase::Paused);
    assert!(
        s.recovery_progress
            .report
            .as_ref()
            .unwrap()
            .blockers
            .iter()
            .any(|v| v == "below_fast_ema")
    );
}

#[test]
fn diagnostic_report_distinguishes_market_blockers_from_data_recovery() {
    let (c, mut s, mut f) = progress();
    for (i, b) in f.candles.iter_mut().rev().take(6).enumerate() {
        b.close *= if i % 2 == 0 { 1.003 } else { 0.997 };
        b.high = b.high.max(b.close);
        b.low = b.low.min(b.close);
    }
    s.evaluate_with_continuity(&c.strategy, &f);
    let r = s.recovery_progress.report.as_ref().unwrap();
    assert!(r.vol_ratio > c.strategy.vol_pause_ratio);
    assert_eq!(r.healthy_hours, 0);
    let mut report = json!({"mode":"live","time_ms":f.now_ms,"max_data_age_seconds":60,
        "snapshot":{"strategy":s,"pool":f.pool,"positions":[]}});
    let text = lp_maker::monitor::display::liquidity(&report);
    assert!(text.contains("波动率超过暂停阈值"));
    assert!(text.contains("健康小时 0/6"));
    assert!(text.contains("暂停 > 1.80倍｜恢复 < 1.20倍"));
    assert!(text.contains("最近清零"));
    report["runtime"] = json!({"status":"degraded"});
    let text = lp_maker::monitor::display::liquidity(&report);
    assert!(text.contains("上次观察，待重新核对"));
    assert!(text.contains("已冻结，核对通过前不据此入场"));
    report["time_ms"] = json!(f.now_ms + 180_000);
    report["runtime"] = Value::Null;
    assert!(lp_maker::monitor::display::liquidity(&report).contains("已过期"));
}

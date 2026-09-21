pub mod indicators;
pub mod progress;
use crate::{
    config::StrategyConfig,
    domain::{Decision, LpIntent, MarketFrame},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Phase {
    Warmup,
    Active,
    Paused,
    Recovering,
    Halted,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EntryHistory {
    Initial,
    Established,
    /// Older checkpoints cannot prove that this is the first LP entry.
    #[default]
    LegacyUnknown,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LayerState {
    pub protected: bool,
    pub outside_side: i8,
    pub outside_count: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Strategy {
    #[serde(default)]
    pub entry_history: EntryHistory,
    pub phase: Phase,
    pub peak_equity: f64,
    pub pause_since: u64,
    pub last_hour: u64,
    pub healthy_hours: u32,
    pub fraction: f64,
    pub last_scale: u64,
    pub last_decision_bar: u64,
    pub layers: BTreeMap<String, LayerState>,
    /// 新版 EVM 恢复证据；旧检查点缺失此字段时按默认值安全迁移。
    #[serde(default)]
    pub recovery_progress: progress::Progress,
}
impl Default for Strategy {
    fn default() -> Self {
        Self {
            entry_history: EntryHistory::Initial,
            phase: Phase::Warmup,
            peak_equity: 0.0,
            pause_since: 0,
            last_hour: 0,
            healthy_hours: 0,
            fraction: 0.0,
            last_scale: 0,
            last_decision_bar: 0,
            layers: BTreeMap::new(),
            recovery_progress: Default::default(),
        }
    }
}
impl Strategy {
    pub fn observe_lp(&mut self, has_positions: bool) {
        if has_positions {
            self.entry_history = EntryHistory::Established;
        }
    }
    pub fn evaluate(&mut self, c: &StrategyConfig, f: &MarketFrame) -> Decision {
        self.evaluate_inner(c, f, false)
    }
    /// EVM 运行器启用短暂观察中断的进度核对；既有 Solana 策略使用原入口。
    pub fn evaluate_with_continuity(&mut self, c: &StrategyConfig, f: &MarketFrame) -> Decision {
        self.evaluate_inner(c, f, true)
    }
    fn evaluate_inner(
        &mut self,
        c: &StrategyConfig,
        f: &MarketFrame,
        continuity: bool,
    ) -> Decision {
        self.observe_lp(!f.portfolio.positions.is_empty());
        if self.phase == Phase::Warmup
            && self.entry_history != EntryHistory::Initial
            && f.portfolio.positions.is_empty()
        {
            self.phase = Phase::Paused;
            self.pause_since = f.now_ms;
        }
        let mut d = Decision {
            state: format!("{:?}", self.phase),
            reasons: vec![],
            lp: LpIntent::Hold,
            target_short_base: f.portfolio.short_base,
            emergency: false,
        };
        let p = f.pool.price;
        let stale = f.now_ms.saturating_sub(f.pool.time_ms) > c.max_data_age_seconds * 1000
            || f.now_ms.saturating_sub(f.hedge_time_ms) > c.max_data_age_seconds * 1000;
        let valid = [
            p,
            f.hedge_price,
            f.portfolio.equity(p),
            f.portfolio.base(),
            f.portfolio.short_base,
        ]
        .iter()
        .all(|x| x.is_finite())
            && p > 0.0
            && f.hedge_price > 0.0;
        if stale || !valid {
            if continuity {
                // 数据不可用不能证明市场恶化。冻结进度且不操作，避免重置风险冷却。
                self.freeze_recovery_progress();
            } else {
                if self.phase != Phase::Halted {
                    self.phase = Phase::Paused;
                    self.pause_since = f.now_ms;
                }
                self.healthy_hours = 0;
            }
            d.state = format!("{:?}", self.phase);
            d.reasons
                .push("stale_or_invalid_data: no transactions using uncertain prices".into());
            return d;
        }
        let equity = f.portfolio.equity(p);
        self.peak_equity = self.peak_equity.max(equity).max(c.total_capital);
        if equity < self.peak_equity * (1.0 - c.max_drawdown) {
            self.phase = Phase::Halted;
            d.reasons.push("portfolio_drawdown_limit".into());
        }
        if self.phase == Phase::Halted {
            d.state = "Halted".into();
            d.lp = LpIntent::ExitToQuote;
            d.target_short_base = f.portfolio.base();
            d.emergency = true;
            return d;
        }
        let metrics = match indicators::calculate(&f.candles, c, f.now_ms) {
            Ok(m) => m,
            Err(e) => {
                if continuity {
                    self.freeze_recovery_progress();
                }
                d.reasons.push(format!("indicator_warmup_or_gap: {e}"));
                d.target_short_base = f.portfolio.base();
                d.emergency = !f.portfolio.positions.is_empty();
                return d;
            }
        };
        let fast =
            metrics.return_1h < -c.fast_drop_1h || p / metrics.last_close - 1.0 < -c.fast_drop_1h;
        let basis = (p / f.hedge_price - 1.0).abs() * 10000.0;
        let shock = metrics.vol_ratio > c.vol_pause_ratio;
        if fast {
            d.reasons.push("fast_drop".into());
        }
        if shock {
            d.reasons.push("volatility_spike".into());
        }
        if metrics.downtrend {
            d.reasons.push("persistent_downtrend".into());
        }
        if basis > c.max_basis_bps {
            d.reasons.push("pool_perp_basis_limit".into());
        }
        let unsafe_market = fast || shock || metrics.downtrend || basis > c.max_basis_bps;
        let recovery_blockers = progress::blockers(c, f, &metrics, &d.reasons);
        let healthy = recovery_blockers.is_empty();
        let mut counter_event = if continuity && !unsafe_market {
            self.recheck_progress(f, &metrics, &recovery_blockers, &mut d.reasons)
        } else {
            "unchanged"
        };
        if unsafe_market {
            if self.phase != Phase::Paused {
                self.pause_since = f.now_ms;
            }
            self.phase = Phase::Paused;
            if continuity {
                self.recovery_progress.pending_revalidation = false;
                let reason = recovery_blockers
                    .first()
                    .map(String::as_str)
                    .unwrap_or("hour_not_healthy");
                self.reset_healthy_progress(f.now_ms, reason);
                counter_event = "reset_market_risk";
            } else {
                self.healthy_hours = 0;
            }
            self.fraction = 0.0;
            for v in self.layers.values_mut() {
                v.outside_count = 0;
                v.outside_side = 0;
            }
        }
        let new_hour = metrics.last_close_ms > self.last_hour;
        if new_hour {
            if self.last_hour > 0 && metrics.last_close_ms - self.last_hour > 3_600_000 {
                if continuity {
                    self.reset_healthy_progress(f.now_ms, "hourly_gap");
                } else {
                    self.healthy_hours = 0;
                }
            }
            self.last_hour = metrics.last_close_ms;
            if healthy {
                self.healthy_hours = self.healthy_hours.saturating_add(1);
                counter_event = "hour_counted";
            } else {
                if continuity && !unsafe_market {
                    self.reset_healthy_progress(f.now_ms, "hour_not_healthy");
                } else {
                    self.healthy_hours = 0;
                }
            }
        }
        if continuity {
            if counter_event == "unchanged" {
                counter_event = "waiting_next_hour";
            }
            self.report_progress(c, f, &metrics, recovery_blockers, counter_event);
        }
        if self.phase == Phase::Paused {
            // First entry uses the current-market gates above, as a fresh Warmup does.
            // Stale data, drawdown, downtrend and volatility protection remain mandatory.
            let initial_entry = self.entry_history == EntryHistory::Initial && !unsafe_market;
            let can_resume = !unsafe_market
                && (!continuity || healthy)
                && self.healthy_hours >= c.resume_healthy_hours
                && f.now_ms.saturating_sub(self.pause_since) >= c.cooldown_hours * 3_600_000;
            if initial_entry {
                self.phase = Phase::Active;
                self.fraction = 1.0;
                d.lp = LpIntent::Deploy { fraction: 1.0 };
                d.reasons
                    .push("initial_entry: cooldown and healthy-hour waiting waived".into());
            } else if can_resume {
                self.phase = Phase::Recovering;
                self.fraction = c.recovery_fraction;
                self.last_scale = f.now_ms;
                d.lp = LpIntent::Deploy {
                    fraction: self.fraction,
                };
            } else {
                let remaining_ms = (c.cooldown_hours * 3_600_000)
                    .saturating_sub(f.now_ms.saturating_sub(self.pause_since));
                d.reasons.push(format!(
                    "resume_wait: cooldown_remaining_seconds={}, healthy_hours={}/{}",
                    remaining_ms.div_ceil(1000),
                    self.healthy_hours,
                    c.resume_healthy_hours
                ));
                d.lp = LpIntent::ExitToQuote;
                d.target_short_base = f.portfolio.base();
                d.emergency = true;
                d.state = "Paused".into();
                return d;
            }
        } else if self.phase == Phase::Warmup {
            self.phase = Phase::Active;
            self.fraction = 1.0;
            if f.portfolio.positions.is_empty() {
                d.lp = LpIntent::Deploy { fraction: 1.0 };
                d.reasons
                    .push("initial_entry: current-market checks passed".into());
            }
        } else if self.phase == Phase::Recovering
            && (!continuity || healthy)
            && self.healthy_hours >= c.resume_healthy_hours
            && f.now_ms.saturating_sub(self.last_scale) >= c.recovery_step_hours * 3_600_000
        {
            self.fraction = (self.fraction + c.recovery_fraction).min(1.0);
            self.last_scale = f.now_ms;
            if self.fraction >= 1.0 {
                self.phase = Phase::Active;
            }
            d.lp = LpIntent::Deploy {
                fraction: self.fraction,
            };
        }
        let bucket = f.now_ms / (c.decision_bar_seconds * 1000);
        let new_bar = bucket > self.last_decision_bar;
        if new_bar {
            if self.last_decision_bar > 0 && bucket > self.last_decision_bar + 1 {
                for layer in self.layers.values_mut() {
                    layer.outside_count = 0;
                    layer.outside_side = 0;
                }
            }
            self.last_decision_bar = bucket;
        }
        let mut resets = vec![];
        let mut target = f.portfolio.wallet_base;
        for pos in &f.portfolio.positions {
            let s = self.layers.entry(pos.layer.clone()).or_default();
            if p < pos.lower {
                s.protected = true;
            }
            if p > pos.lower * (1.0 + c.hedge_release_buffer) {
                s.protected = false;
            }
            if s.protected {
                d.emergency = true;
            }
            target += pos.base
                * if s.protected {
                    1.0
                } else {
                    c.inside_hedge_ratio
                };
            if new_bar {
                let side = if p < pos.lower * (1.0 - c.breakout_buffer) {
                    -1
                } else if p > pos.upper * (1.0 + c.breakout_buffer) {
                    1
                } else {
                    0
                };
                if side == 0 {
                    s.outside_count = 0;
                } else if side == s.outside_side {
                    s.outside_count += 1;
                } else {
                    s.outside_count = 1;
                }
                s.outside_side = side;
                if s.outside_count >= c.breakout_confirm_bars {
                    resets.push(pos.layer.clone());
                    s.outside_count = 0;
                }
            }
        }
        if d.lp == LpIntent::Hold && !resets.is_empty() {
            d.lp = LpIntent::Recenter { layers: resets };
        }
        // A target is only an intent. The executor re-reads actual fills and inventory.
        d.target_short_base = target.max(0.0);
        d.state = format!("{:?}", self.phase);
        d
    }
}

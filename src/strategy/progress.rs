//! EVM 运行器的恢复进度：短暂读故障冻结计数，完成新一轮核对后再验证连续性。
//! 不回放缺失小时、不放宽市场阈值；Solana 保持原 evaluate 路径。
use super::{Strategy, indicators::Metrics};
use crate::{
    config::StrategyConfig,
    domain::{Candle, MarketFrame},
};
use serde::{Deserialize, Serialize};

/// 仅给已有、可核对的健康记录短暂观察宽限，不是行情新鲜度或入场豁免。
pub const RECHECK_GRACE_MS: u64 = 300_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub observed_ms: u64,
    pub anchor: Option<Candle>,
    pub pending_revalidation: bool,
    pub last_reset: Option<Reset>,
    pub report: Option<Report>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reset {
    pub time_ms: u64,
    pub previous_hours: u32,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub observed_ms: u64,
    pub completed_hour_ms: u64,
    pub recent_hours: usize,
    pub baseline_hours: usize,
    pub recent_vol_pct: f64,
    pub baseline_vol_pct: f64,
    pub baseline_used_vol_pct: f64,
    pub vol_ratio: f64,
    pub pause_ratio: f64,
    pub resume_ratio: f64,
    pub healthy_hours: u32,
    pub required_hours: u32,
    pub cooldown_remaining_seconds: u64,
    pub blockers: Vec<String>,
    pub counter_event: String,
}

pub fn blockers(c: &StrategyConfig, f: &MarketFrame, m: &Metrics, risk: &[String]) -> Vec<String> {
    let mut out = risk.to_vec();
    if m.vol_ratio >= c.vol_resume_ratio {
        out.push("resume_volatility_high".into());
    }
    if !m.no_new_low {
        out.push("new_hourly_low".into());
    }
    if f.pool.price < m.ema_fast {
        out.push("below_fast_ema".into());
    }
    out
}

impl Strategy {
    /// 只冻结恢复进度；pending 交易和真实仓位仍由运行器先完整对账。
    pub fn freeze_recovery_progress(&mut self) {
        self.recovery_progress.pending_revalidation = true;
    }
    pub fn reset_healthy_progress(&mut self, now: u64, reason: &str) {
        let previous_hours = self.healthy_hours;
        self.healthy_hours = 0;
        // 同一风险连续触发时保留首次清零时间，不让每次轮询覆盖诊断证据。
        if previous_hours > 0
            || self
                .recovery_progress
                .last_reset
                .as_ref()
                .is_none_or(|r| r.reason != reason)
        {
            self.recovery_progress.last_reset = Some(Reset {
                time_ms: now,
                previous_hours,
                reason: reason.into(),
            });
        }
    }
    /// 只在读故障/重启后执行。宽限按上次有效市场观察计算，重试不能续期。
    pub(super) fn recheck_progress(
        &mut self,
        f: &MarketFrame,
        m: &Metrics,
        blockers: &[String],
        reasons: &mut Vec<String>,
    ) -> &'static str {
        if !self.recovery_progress.pending_revalidation {
            return "unchanged";
        }
        self.recovery_progress.pending_revalidation = false;
        if self.healthy_hours == 0 {
            return "rechecked_empty";
        }
        let p = &self.recovery_progress;
        let reason = if p.observed_ms == 0 || p.anchor.is_none() {
            Some("legacy_without_observation")
        } else if f.now_ms < p.observed_ms {
            Some("observation_clock_reversed")
        } else if f.now_ms - p.observed_ms > RECHECK_GRACE_MS {
            Some("observation_gap_over_grace")
        } else if m.last_close_ms < self.last_hour || m.last_close_ms - self.last_hour > 3_600_000 {
            Some("hourly_gap")
        } else if !p.anchor.as_ref().is_some_and(|a| {
            a.close_ms == self.last_hour && f.candles.iter().any(|b| same_bar(a, b))
        }) {
            Some("completed_candle_changed")
        } else if !blockers.is_empty() {
            Some("resume_conditions_failed")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.reset_healthy_progress(f.now_ms, reason);
            reasons.push(format!("recovery_progress_reset: {reason}"));
            "reset_after_recheck"
        } else {
            reasons.push(format!(
                "recovery_progress_retained: healthy_hours={}, observation_gap_seconds={}",
                self.healthy_hours,
                (f.now_ms - p.observed_ms).div_ceil(1000)
            ));
            "retained_after_recheck"
        }
    }
    pub(super) fn report_progress(
        &mut self,
        c: &StrategyConfig,
        f: &MarketFrame,
        m: &Metrics,
        blockers: Vec<String>,
        event: &str,
    ) {
        self.recovery_progress.observed_ms = f.now_ms;
        self.recovery_progress.anchor = f
            .candles
            .iter()
            .find(|b| b.close_ms == m.last_close_ms)
            .cloned();
        self.recovery_progress.report = Some(Report {
            observed_ms: f.now_ms,
            completed_hour_ms: m.last_close_ms,
            recent_hours: c.vol_short_hours,
            baseline_hours: c.vol_long_hours,
            recent_vol_pct: m.recent_vol * 100.0,
            baseline_vol_pct: m.baseline_vol * 100.0,
            baseline_used_vol_pct: m.baseline_vol.max(1e-6) * 100.0,
            vol_ratio: m.vol_ratio,
            pause_ratio: c.vol_pause_ratio,
            resume_ratio: c.vol_resume_ratio,
            healthy_hours: self.healthy_hours,
            required_hours: c.resume_healthy_hours,
            cooldown_remaining_seconds: (c.cooldown_hours * 3_600_000)
                .saturating_sub(f.now_ms.saturating_sub(self.pause_since))
                .div_ceil(1000),
            blockers,
            counter_event: event.into(),
        });
    }
}
fn same_bar(a: &Candle, b: &Candle) -> bool {
    a.open_ms == b.open_ms
        && a.close_ms == b.close_ms
        && a.open == b.open
        && a.high == b.high
        && a.low == b.low
        && a.close == b.close
}

pub fn reason_zh(reason: &str) -> &str {
    match reason {
        "volatility_spike" => "波动率超过暂停阈值",
        "fast_drop" => "触发快跌保护",
        "persistent_downtrend" => "下降趋势",
        "pool_perp_basis_limit" => "池价与合约价差过大",
        "resume_volatility_high" => "波动率尚未低于恢复阈值",
        "new_hourly_low" => "最新小时创近期新低",
        "below_fast_ema" => "池价低于快速均线",
        "legacy_without_observation" => "旧记录缺少有效观察证据，本次重新积累",
        "observation_clock_reversed" => "观察时间倒退",
        "observation_gap_over_grace" => "有效观察中断超过5分钟",
        "hourly_gap" => "已完成小时K线不连续",
        "completed_candle_changed" => "历史小时K线缺失或修订",
        "resume_conditions_failed" => "恢复核对时市场条件未通过",
        "hour_not_healthy" => "新小时未满足全部恢复条件",
        "workflow_or_inventory_changed" => "交易流程恢复或仓位变化，重新积累",
        "retained_after_recheck" => "短暂中断核对通过，已保留原进度",
        "reset_after_recheck" => "核对未通过，进度已清零",
        "reset_market_risk" => "行情风险触发，进度已清零",
        "hour_counted" => "新健康小时已计入",
        "waiting_next_hour" => "等待新的已完成小时K线",
        "rechecked_empty" => "已核对，从当前进度继续",
        _ => reason,
    }
}

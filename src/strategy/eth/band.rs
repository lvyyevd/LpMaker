//! Robinhood 单区间净敞口策略。只计算意图，不读取账户密钥、不发送交易。
//! 对应离线候选 51f16382b6c69283；5%硬停机是实盘边界，不能用重启抹去。
use super::{Feature, HOUR, Report};
use crate::{
    config::{Config, StrategyConfig},
    domain::*,
    strategy::{EntryHistory, Phase, Strategy},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BandConfig {
    pub lower_width: f64,
    pub upper_width: f64,
    pub net_lower_usd: f64,
    pub net_upper_usd: f64,
    pub minimum_holding_hours: u64,
    pub roll_delay_seconds: u64,
    pub episode_drawdown: f64,
    pub pause_drawdown: f64,
    pub reentry_loss_budget: f64,
}
impl BandConfig {
    pub fn validate(&self, c: &Config) -> Result<()> {
        ensure!(
            c.liquidity.chain_id == crate::liquidity::chains::robinhood::CHAIN.id
                && c.liquidity
                    .pool
                    .eq_ignore_ascii_case("0x52e65B17fB6E5BA00Ed806f37Afcd2DaA50271Ca"),
            "ETH band profile is limited to the reviewed Robinhood pool"
        );
        ensure!(
            [self.lower_width, self.upper_width]
                .iter()
                .all(|v| v.is_finite() && *v > 0.001 && *v <= 0.08),
            "band boundaries must be within 8% of entry quote"
        );
        ensure!(
            self.net_lower_usd.is_finite()
                && self.net_upper_usd.is_finite()
                && self.net_lower_usd <= 0.
                && self.net_upper_usd > 0.,
            "invalid net exposure band"
        );
        ensure!(
            (1..=720).contains(&self.minimum_holding_hours)
                && (1..=300).contains(&self.roll_delay_seconds),
            "invalid band holding/roll time"
        );
        ensure!(
            [
                self.episode_drawdown,
                self.pause_drawdown,
                self.reentry_loss_budget
            ]
            .iter()
            .all(|v| v.is_finite() && *v > 0. && *v < c.strategy.max_drawdown),
            "invalid band drawdown thresholds"
        );
        ensure!(
            c.strategy.total_capital == 200.
                && c.strategy.lp_budget == 120.
                && c.strategy.hedge_collateral == 60.
                && c.strategy.reserve == 20.
                && c.strategy.layers.len() == 1
                && c.strategy.layers[0].half_width <= 0.08,
            "reviewed band profile requires 200 = 120 LP + 60 hedge + 20 reserve and one bounded layer"
        );
        Ok(())
    }
    /// 已完成24小时上涨时下侧略窄，其他时候交换两侧；不是根据未来走势选边。
    pub fn widths(&self, x: &Feature) -> (f64, f64) {
        let (n, w) = (
            self.lower_width.min(self.upper_width),
            self.lower_width.max(self.upper_width),
        );
        if x.r24 > 0. { (n, w) } else { (w, n) }
    }
    pub fn target(&self, p: &Portfolio, price: f64) -> f64 {
        let net = (p.base() - p.short_base) * price;
        let target = if net > self.net_upper_usd {
            (p.base() - self.net_upper_usd / price).max(0.)
        } else if net < self.net_lower_usd {
            (p.base() - self.net_lower_usd / price).max(0.)
        } else {
            p.short_base
        };
        target.min(p.hedge_equity.clamp(0., 60.) * 2.55 / price)
    }
}
pub fn config(c: &StrategyConfig) -> Option<&BandConfig> {
    c.eth_persistent.as_ref()?.band.as_ref()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BandState {
    /// 首次可信观测的全账户净值。风控权益=200+全账户净值变化，闲置资金不放大5%额度。
    pub account_origin_equity: Option<f64>,
    pub risk_equity: f64,
    pub entered_ms: u64,
    pub episode_peak: f64,
    pub entry_drawdown: f64,
    pub net_exposure_usd: f64,
    pub pause_threshold: f64,
}

/// 保留首次观测基准；重启/风险恢复不得将亏损重新记成200U。
/// 沿用原观测口径，入出金与Gas仍不能自动分离，日志明确提示这一限制。
pub(super) fn managed_equity(s: &mut Strategy, c: &StrategyConfig, raw: f64) -> f64 {
    let st = s
        .eth_persistent
        .get_or_insert_with(Default::default)
        .band
        .get_or_insert_with(Default::default);
    let origin = *st.account_origin_equity.get_or_insert(raw);
    st.risk_equity = c.total_capital + raw - origin;
    st.risk_equity
}

/// 数据时效和小时特征由共同入口验证后再进入此状态机。
pub(super) fn evaluate(
    s: &mut Strategy,
    c: &StrategyConfig,
    f: &MarketFrame,
    x: &Feature,
) -> Decision {
    let cfg = c.eth_persistent.as_ref().unwrap();
    let band = cfg.band.as_ref().unwrap();
    let mut d = super::hold(s, f);
    let p = f.pool.price;
    let eq = managed_equity(s, c, f.portfolio.equity(p));
    s.observe_lp(!f.portfolio.positions.is_empty());
    let st = s.eth_persistent.get_or_insert_with(Default::default);
    if s.phase == Phase::Paused && st.pause_anchor_ms != s.pause_since {
        st.pause_anchor_ms = s.pause_since;
        st.reentry_not_before_ms = s.pause_since + c.cooldown_hours * HOUR;
        st.pending_hedge_target = None;
        if let Some(bs) = st.band.as_mut() {
            bs.entered_ms = 0;
        }
        s.healthy_hours = 0;
    }
    if st.last_observed_ms > f.now_ms
        || (st.last_observed_ms > 0
            && f.now_ms - st.last_observed_ms > c.max_data_age_seconds * 1000)
    {
        s.healthy_hours = 0;
        s.layers.clear();
        // 观察缺口不改变持仓时钟、全局净值峰值或未成交目标。
    }
    st.last_observed_ms = f.now_ms;
    s.peak_equity = s.peak_equity.max(eq).max(c.total_capital);
    let dd = 1. - eq / s.peak_equity;
    if dd >= c.max_drawdown {
        s.phase = Phase::Halted;
    }
    let guard = cfg.guard(c);
    let danger = guard.danger(x, p).or_else(|| {
        if (p / f.hedge_price - 1.).abs() * 10000. > c.max_basis_bps {
            Some("pool_perp_basis_limit")
        } else {
            None
        }
    });
    if x.time > s.last_hour {
        if s.last_hour > 0 && x.time - s.last_hour != HOUR {
            s.healthy_hours = 0;
        }
        s.last_hour = x.time;
        s.healthy_hours = if guard.recovered(x, p) {
            s.healthy_hours.saturating_add(1)
        } else {
            0
        };
    }
    if danger.is_some() {
        s.healthy_hours = 0;
    }
    let bs = st.band.get_or_insert_with(Default::default);
    bs.net_exposure_usd = (f.portfolio.base() - f.portfolio.short_base) * p;
    let has_lp = !f.portfolio.positions.is_empty();
    if has_lp && bs.entered_ms == 0 {
        // 旧/人工导入记录无法证明入场时间，保守地从首次观察计时。
        bs.entered_ms = f.now_ms;
        if bs.episode_peak == 0. {
            bs.episode_peak = eq;
            bs.entry_drawdown = dd;
        }
    }
    bs.episode_peak = bs.episode_peak.max(eq);
    bs.pause_threshold = band
        .pause_drawdown
        .max(bs.entry_drawdown + band.reentry_loss_budget)
        .min(0.0475);
    let soft = has_lp && 1. - eq / bs.episode_peak >= band.episode_drawdown;
    let global = has_lp && dd >= bs.pause_threshold;
    let mut outside = false;
    let bucket = f.now_ms / (c.decision_bar_seconds * 1000);
    if has_lp && bucket > s.last_decision_bar {
        if s.last_decision_bar > 0 && bucket > s.last_decision_bar + 1 {
            s.layers.clear();
        }
        s.last_decision_bar = bucket;
        for pos in &f.portfolio.positions {
            let side = if p < pos.lower {
                -1
            } else if p > pos.upper {
                1
            } else {
                0
            };
            let layer = s.layers.entry(pos.layer.clone()).or_default();
            layer.outside_count = if side == 0 {
                0
            } else if side == layer.outside_side {
                layer.outside_count.saturating_add(1)
            } else {
                1
            };
            layer.outside_side = side;
            outside |= layer.outside_count >= c.breakout_confirm_bars;
        }
    }
    if s.phase == Phase::Halted || danger.is_some() || soft || global {
        if s.phase != Phase::Halted {
            if s.phase != Phase::Paused || has_lp {
                s.pause_since = f.now_ms;
                st.pause_anchor_ms = s.pause_since;
                st.reentry_not_before_ms = f.now_ms + c.cooldown_hours * HOUR;
            }
            s.phase = Phase::Paused;
        }
        s.healthy_hours = 0;
        s.fraction = 0.;
        s.layers.clear();
        st.pending_hedge_target = None;
        d.lp = LpIntent::ExitToQuote;
        d.target_short_base = f.portfolio.base();
        d.emergency = true;
        d.reasons.push(
            if s.phase == Phase::Halted {
                "portfolio_drawdown_limit"
            } else if let Some(reason) = danger {
                reason
            } else if global {
                "global_drawdown_pause"
            } else {
                "early_drawdown_pause"
            }
            .into(),
        );
    } else if has_lp {
        s.phase = Phase::Active;
        if outside && f.now_ms.saturating_sub(bs.entered_ms) >= band.minimum_holding_hours * HOUR {
            d.lp = LpIntent::Recenter {
                layers: f
                    .portfolio
                    .positions
                    .iter()
                    .map(|p| p.layer.clone())
                    .collect(),
            };
            d.reasons.push("range_recenter".into());
            s.layers.clear();
            bs.entered_ms = 0; // 成交后首次确认LP存在再计48小时，不把交易等待算成持仓。
            bs.episode_peak = eq;
            bs.entry_drawdown = dd;
            st.pending_hedge_target = None;
            st.last_hedge_check_ms = f.now_ms;
        } else {
            if f.now_ms.saturating_sub(st.last_hedge_check_ms) >= cfg.hedge_interval_hours * HOUR
                || st.pending_hedge_target.is_some()
            {
                if st.pending_hedge_target.is_none() {
                    st.last_hedge_check_ms = f.now_ms;
                }
                // 未成交期间按真实库存重算，价格变化后不继续追旧目标。
                let target = band.target(&f.portfolio, p);
                if (target - f.portfolio.short_base).abs() * p >= c.hedge_deadband_usd {
                    st.pending_hedge_target = Some(target);
                    d.target_short_base = target;
                } else {
                    st.pending_hedge_target = None;
                }
            }
        }
    } else {
        if s.phase == Phase::Warmup && s.entry_history != EntryHistory::Initial {
            s.phase = Phase::Paused;
            s.pause_since = f.now_ms;
            st.pause_anchor_ms = f.now_ms;
            st.reentry_not_before_ms = f.now_ms + c.cooldown_hours * HOUR;
        }
        if s.entry_history == EntryHistory::Initial
            || (f.now_ms >= st.reentry_not_before_ms
                && s.healthy_hours >= c.resume_healthy_hours
                && guard.recovered(x, p))
        {
            s.phase = Phase::Active;
            s.fraction = 1.;
            bs.entered_ms = 0; // 成交后首次确认LP存在再计48小时，不把交易等待算成持仓。
            bs.episode_peak = eq;
            bs.entry_drawdown = dd;
            st.pending_hedge_target = None;
            st.last_hedge_check_ms = f.now_ms;
            st.reentry_not_before_ms = 0;
            s.last_decision_bar = bucket;
            d.lp = LpIntent::Deploy { fraction: 1. };
        } else {
            s.phase = Phase::Paused;
            d.lp = LpIntent::ExitToQuote;
            d.target_short_base = f.portfolio.base();
            d.emergency = true;
            d.reasons.push(format!(
                "resume_wait: cooldown_remaining_seconds={}, healthy_hours={}/{}",
                st.reentry_not_before_ms.saturating_sub(f.now_ms) / 1000,
                s.healthy_hours,
                c.resume_healthy_hours
            ));
        }
    }
    st.report = Some(Report {
        observed_ms: f.now_ms,
        completed_hour_ms: x.time,
        return_1h: x.r1,
        return_24h: x.r24,
        return_72h: x.r72,
        vol_6h: x.vol6,
        pause_vol: guard.vol,
        healthy_hours: s.healthy_hours,
        required_hours: c.resume_healthy_hours,
        cooldown_remaining_seconds: if s.phase == Phase::Paused {
            st.reentry_not_before_ms.saturating_sub(f.now_ms) / 1000
        } else {
            0
        },
    });
    d.state = format!("{:?}", s.phase);
    d
}

/// 多笔链上交易之间再次核对200U风控预算，避免等待期间触发退出后仍继续mint。
pub fn entry_equity_safe(s: &Strategy, c: &StrategyConfig, raw: f64) -> bool {
    let Some(cfg) = config(c) else { return true };
    let Some(bs) = s.eth_persistent.as_ref().and_then(|v| v.band.as_ref()) else {
        return false;
    };
    let Some(origin) = bs.account_origin_equity else {
        return false;
    };
    let eq = c.total_capital + raw - origin;
    let trip = cfg
        .pause_drawdown
        .max(bs.entry_drawdown + cfg.reentry_loss_budget)
        .min(0.0475);
    raw.is_finite()
        && s.phase != Phase::Halted
        && eq > s.peak_equity * (1. - c.max_drawdown)
        && eq > s.peak_equity * (1. - trip)
        && eq > bs.episode_peak * (1. - cfg.episode_drawdown)
}

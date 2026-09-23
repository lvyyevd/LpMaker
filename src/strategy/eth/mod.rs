//! ETH 长持 LP：按真实 ETH 库存做部分对冲，严重下跌/高波动退出。
//! 只输出意图；RPC、签名、nonce、成交确认仍由既有执行器负责。
//! 此模块不用于 Solana，也不把假设的 40% APR 写成实盘收入。
use super::{EntryHistory, Phase, Strategy};
use crate::{
    config::{Config, StrategyConfig},
    domain::{Candle, Decision, LpIntent, MarketFrame},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
const HOUR: u64 = 3_600_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentConfig {
    pub drop_24h: f64,
    pub drop_72h: f64,
    /// 最近 6 根已完成小时对数收益的总体标准差，不是年化波动率。
    pub max_hourly_vol: f64,
    pub hedge_interval_hours: u64,
}
impl PersistentConfig {
    pub fn validate(&self, c: &Config) -> Result<()> {
        let s = &c.strategy;
        ensure!(
            c.hyperliquid.hedge_coin == "ETH" && c.liquidity.kind == "uniswap_v3",
            "persistent profile only supports EVM ETH Uniswap v3"
        );
        ensure!(
            c.hyperliquid.cross_margin,
            "persistent ETH requires dedicated cross margin; unattended isolated margin is not modeled"
        );
        ensure!(
            s.max_drawdown <= 0.05,
            "persistent ETH drawdown cap cannot exceed 5%"
        );
        ensure!(
            s.layers.len() == 1 && s.inside_hedge_ratio >= 0.35,
            "persistent ETH requires one LP layer and at least 35% inventory hedge"
        );
        ensure!(
            [self.drop_24h, self.drop_72h, self.max_hourly_vol]
                .iter()
                .all(|x| x.is_finite() && *x > 0. && *x < 1.)
                && self.drop_24h <= self.drop_72h
                && (1..=24).contains(&self.hedge_interval_hours)
                && (1..=168).contains(&s.cooldown_hours),
            "invalid persistent ETH risk parameters"
        );
        Ok(())
    }
    pub fn guard(&self, c: &StrategyConfig) -> Guard {
        Guard {
            drop1: c.fast_drop_1h,
            drop24: self.drop_24h,
            drop72: self.drop_72h,
            vol: self.max_hourly_vol,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentState {
    pub last_observed_ms: u64,
    pub last_hedge_check_ms: u64,
    /// Maker 未成交时保留目标，下一轮继续核对真实仓位，不假定已经成交。
    pub pending_hedge_target: Option<f64>,
    pub reentry_not_before_ms: u64,
    /// 区分本策略的区间等待和执行器因未完成流程设置的新冷却。
    pub pause_anchor_ms: u64,
    pub report: Option<Report>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub observed_ms: u64,
    pub completed_hour_ms: u64,
    pub return_1h: f64,
    pub return_24h: f64,
    pub return_72h: f64,
    pub vol_6h: f64,
    pub pause_vol: f64,
    pub healthy_hours: u32,
    pub required_hours: u32,
    pub cooldown_remaining_seconds: u64,
}
#[derive(Clone, Debug)]
pub struct Feature {
    pub time: u64,
    pub close: f64,
    pub r1: f64,
    pub r6: f64,
    pub r24: f64,
    pub r72: f64,
    pub vol6: f64,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Guard {
    pub drop1: f64,
    pub drop24: f64,
    pub drop72: f64,
    pub vol: f64,
}
impl Guard {
    pub fn danger(&self, f: &Feature, p: f64) -> Option<&'static str> {
        if f.r1 < -self.drop1 || p / f.close - 1. < -self.drop1 {
            Some("fast_drop")
        } else if f.r24 < -self.drop24 || f.r72 < -self.drop72 {
            Some("downtrend")
        } else if f.vol6 > self.vol {
            Some("volatility")
        } else {
            None
        }
    }
    pub fn recovered(&self, f: &Feature, p: f64) -> bool {
        self.danger(f, p).is_none() && f.r6 >= -0.01 && f.r1 >= -0.005 && f.vol6 < self.vol * 0.8
    }
}

/// 调用者提供连续小时数据；批量回放与实时观测共用相同公式。
pub fn features(a: &[Candle]) -> Vec<Feature> {
    (72..a.len())
        .map(|i| {
            let ret = |n: usize| a[i].close / a[i - n].close - 1.;
            let r: Vec<f64> = (i - 5..=i)
                .map(|j| (a[j].close / a[j - 1].close).ln())
                .collect();
            let mean = r.iter().sum::<f64>() / 6.;
            Feature {
                time: a[i].close_ms,
                close: a[i].close,
                r1: ret(1),
                r6: ret(6),
                r24: ret(24),
                r72: ret(72),
                vol6: (r.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / 6.).sqrt(),
            }
        })
        .collect()
}
pub fn latest_feature(a: &[Candle], now: u64) -> Result<Feature> {
    let a: Vec<_> = a.iter().filter(|x| x.close_ms < now).cloned().collect();
    ensure!(a.len() >= 73, "need 73 completed hourly candles");
    let a = &a[a.len() - 73..];
    for x in a {
        ensure!(
            [x.open, x.close, x.low, x.high]
                .iter()
                .all(|v| v.is_finite() && *v > 0.)
                && x.high >= x.open.max(x.close)
                && x.low <= x.open.min(x.close)
                && x.close_ms.checked_sub(x.open_ms) == Some(HOUR - 1),
            "invalid hourly candle"
        );
    }
    ensure!(
        a.windows(2).all(|w| w[1].open_ms == w[0].open_ms + HOUR),
        "hourly gap/duplicate/out-of-order"
    );
    ensure!(
        now.saturating_sub(a[72].close_ms) <= HOUR + 60_000,
        "stale candle history"
    );
    Ok(features(a).remove(0))
}

fn hold(s: &Strategy, f: &MarketFrame) -> Decision {
    Decision {
        state: format!("{:?}", s.phase),
        reasons: vec![],
        lp: LpIntent::Hold,
        target_short_base: f.portfolio.short_base,
        emergency: false,
    }
}
pub fn evaluate(s: &mut Strategy, c: &StrategyConfig, f: &MarketFrame) -> Decision {
    match latest_feature(&f.candles, f.now_ms) {
        Ok(feature) => evaluate_feature(s, c, f, &feature),
        Err(e) => {
            s.healthy_hours = 0;
            let mut d = hold(s, f);
            d.reasons.push(format!(
                "stale_or_invalid_data: persistent hourly history: {e}"
            ));
            // 历史 K 线故障不能屏蔽已可验证的账户回撤退出。
            let equity = f.portfolio.equity(f.pool.price);
            if [f.pool.price, f.hedge_price, equity]
                .iter()
                .all(|v| v.is_finite())
                && f.pool.price > 0.
                && f.hedge_price > 0.
                && f.pool.time_ms <= f.now_ms
                && f.hedge_time_ms <= f.now_ms
                && f.now_ms - f.pool.time_ms <= c.max_data_age_seconds * 1000
                && f.now_ms - f.hedge_time_ms <= c.max_data_age_seconds * 1000
            {
                s.peak_equity = s.peak_equity.max(equity).max(c.total_capital);
                if s.phase == Phase::Halted || equity <= s.peak_equity * (1. - c.max_drawdown) {
                    s.phase = Phase::Halted;
                    d.state = "Halted".into();
                    d.lp = LpIntent::ExitToQuote;
                    d.target_short_base = f.portfolio.base();
                    d.emergency = true;
                    d.reasons = vec!["portfolio_drawdown_limit".into()];
                }
            }
            d
        }
    }
}

/// 已验证的逐分钟回放可复用此入口；拒绝未来/过期特征，不读未来 K 线。
pub fn evaluate_feature(
    s: &mut Strategy,
    c: &StrategyConfig,
    f: &MarketFrame,
    x: &Feature,
) -> Decision {
    let cfg = c.eth_persistent.as_ref().expect("explicit ETH profile");
    let mut d = hold(s, f);
    let p = f.pool.price;
    let eq = f.portfolio.equity(p);
    if [
        p,
        f.hedge_price,
        eq,
        f.portfolio.base(),
        f.portfolio.short_base,
        x.close,
        x.r1,
        x.r6,
        x.r24,
        x.r72,
        x.vol6,
    ]
    .iter()
    .any(|v| !v.is_finite())
        || p <= 0.
        || f.hedge_price <= 0.
        || x.close <= 0.
        || f.pool.time_ms > f.now_ms
        || f.hedge_time_ms > f.now_ms
        || x.time >= f.now_ms
        || f.now_ms.saturating_sub(x.time) > HOUR + 60_000
        || f.now_ms.saturating_sub(f.pool.time_ms) > c.max_data_age_seconds * 1000
        || f.now_ms.saturating_sub(f.hedge_time_ms) > c.max_data_age_seconds * 1000
    {
        s.healthy_hours = 0;
        d.reasons
            .push("stale_or_invalid_data: persistent observation unavailable".into());
        return d;
    }
    s.observe_lp(!f.portfolio.positions.is_empty());
    let st = s.eth_persistent.get_or_insert_with(Default::default);
    if s.phase == Phase::Paused && st.pause_anchor_ms != s.pause_since {
        st.pause_anchor_ms = s.pause_since;
        st.reentry_not_before_ms = s.pause_since + c.cooldown_hours * HOUR;
        st.pending_hedge_target = None;
        s.healthy_hours = 0;
    }
    if st.last_observed_ms > f.now_ms
        || (st.last_observed_ms > 0
            && f.now_ms - st.last_observed_ms > c.max_data_age_seconds * 1000)
    {
        // 重启/断线不会补算未观察到的健康小时，已有冷却、峰值和仓位保持。
        s.healthy_hours = 0;
        s.layers.clear();
    }
    st.last_observed_ms = f.now_ms;
    s.peak_equity = s.peak_equity.max(eq).max(c.total_capital);
    if eq <= s.peak_equity * (1. - c.max_drawdown) {
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
    let mut outside = false;
    if !f.portfolio.positions.is_empty()
        && f.now_ms / c.decision_bar_seconds / 1000 > s.last_decision_bar
    {
        let bucket = f.now_ms / (c.decision_bar_seconds * 1000);
        if s.last_decision_bar > 0 && bucket > s.last_decision_bar + 1 {
            s.layers.clear();
        }
        s.last_decision_bar = bucket;
        for pos in &f.portfolio.positions {
            let direction = if p < pos.lower * (1. - c.breakout_buffer) {
                -1
            } else if p > pos.upper * (1. + c.breakout_buffer) {
                1
            } else {
                0
            };
            let layer = s.layers.entry(pos.layer.clone()).or_default();
            layer.outside_count = if direction == 0 {
                0
            } else if direction == layer.outside_side {
                layer.outside_count.saturating_add(1)
            } else {
                1
            };
            layer.outside_side = direction;
            outside |= layer.outside_count >= c.breakout_confirm_bars;
        }
    }
    if s.phase == Phase::Halted || danger.is_some() || outside {
        if s.phase != Phase::Halted {
            if s.phase != Phase::Paused || !f.portfolio.positions.is_empty() {
                s.pause_since = f.now_ms;
                st.pause_anchor_ms = s.pause_since;
                st.reentry_not_before_ms = f.now_ms
                    + if outside && danger.is_none() {
                        c.decision_bar_seconds * 1000
                    } else {
                        c.cooldown_hours * HOUR
                    };
            }
            s.phase = Phase::Paused;
        }
        if !outside || danger.is_some() {
            s.healthy_hours = 0;
        }
        s.fraction = 0.;
        s.layers.clear();
        st.pending_hedge_target = None;
        d.lp = LpIntent::ExitToQuote;
        d.emergency = true;
        d.target_short_base = f.portfolio.base();
        d.reasons.push(
            if s.phase == Phase::Halted {
                "portfolio_drawdown_limit"
            } else {
                danger.unwrap_or("range_recenter")
            }
            .into(),
        );
    } else if f.portfolio.positions.is_empty() {
        // 老检查点无法证明从未入场时，仍须等待。首次入场也必须先通过所有风险检查。
        if s.phase == Phase::Warmup && s.entry_history != EntryHistory::Initial {
            s.phase = Phase::Paused;
            s.pause_since = f.now_ms;
            st.pause_anchor_ms = s.pause_since;
            st.reentry_not_before_ms = f.now_ms + c.cooldown_hours * HOUR;
        }
        let deadline = if st.reentry_not_before_ms > 0 {
            st.reentry_not_before_ms
        } else {
            s.pause_since + c.cooldown_hours * HOUR
        };
        if s.entry_history == EntryHistory::Initial
            || (f.now_ms >= deadline && s.healthy_hours >= c.resume_healthy_hours)
        {
            s.phase = Phase::Active;
            s.fraction = 1.;
            st.last_hedge_check_ms = f.now_ms;
            st.pending_hedge_target = None;
            s.last_decision_bar = f.now_ms / (c.decision_bar_seconds * 1000);
            d.lp = LpIntent::Deploy { fraction: 1. };
        } else {
            s.phase = Phase::Paused;
            d.lp = LpIntent::ExitToQuote;
            d.target_short_base = f.portfolio.base();
            d.emergency = true;
            d.reasons.push(format!(
                "resume_wait: cooldown_remaining_seconds={}, healthy_hours={}/{}",
                deadline.saturating_sub(f.now_ms) / 1000,
                s.healthy_hours,
                c.resume_healthy_hours
            ));
        }
    } else {
        s.phase = Phase::Active;
        if f.now_ms.saturating_sub(st.last_hedge_check_ms) >= cfg.hedge_interval_hours * HOUR {
            st.last_hedge_check_ms = f.now_ms;
            st.pending_hedge_target = Some(f.portfolio.base() * c.inside_hedge_ratio);
        }
        if let Some(target) = st.pending_hedge_target {
            if (target - f.portfolio.short_base).abs() * p >= c.hedge_deadband_usd {
                d.target_short_base = target;
            } else {
                st.pending_hedge_target = None;
            }
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
        cooldown_remaining_seconds: st.reentry_not_before_ms.saturating_sub(f.now_ms) / 1000,
    });
    d.state = format!("{:?}", s.phase);
    d
}

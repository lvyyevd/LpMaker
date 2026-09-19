//! SOL 专用择时：闭合小时线决定趋势/波动率，实时价格只用于急跌和下沿保护。
//! 与 EVM 的 Strategy::evaluate 分开；状态沿用已持久化的通用字段，旧配置不启用此模块。
use crate::{
    config::StrategyConfig,
    domain::{Candle, Decision, LpIntent, MarketFrame},
    strategy::{EntryHistory, Phase, Strategy, indicators},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ema_entry_hours: usize,
    pub ema_exit_hours: usize,
    pub momentum_hours: usize,
    pub min_momentum: f64,
    pub max_hourly_vol: f64,
    /// Entry must be calmer than the exit threshold, to avoid repeated in/out churn.
    #[serde(default = "one")]
    pub entry_vol_fraction: f64,
    #[serde(default = "one")]
    pub max_momentum: f64,
    pub max_bar_range: f64,
    pub exit_buffer: f64,
    pub entry_confirm_hours: u32,
}
fn one() -> f64 {
    1.
}
/// 运维摘要保留原始原因码，同时给出可直接理解的解释。
pub fn reason_zh(reason: &str) -> &str {
    match reason {
        "portfolio_drawdown_limit" => "达到组合最大回撤，熔断保持",
        "pool_perp_basis_limit" => "池子与合约价差超限",
        "sol_high_volatility" => "波动率或小时振幅过大，退出 LP",
        "sol_downtrend" => "下降趋势成立，退出 LP",
        "sol_fast_drop" | "sol_intrahour_fast_drop" => "急跌保护，退出 LP",
        "sol_intrahour_large_move" => "小时内价格剧烈变化，退出 LP",
        "sol_outside_range_exit_and_wait" => "价格越界，退出后等待冷却及趋势恢复",
        "sol_wait_for_trend_and_cooldown" => "等待趋势确认与冷却完成",
        "sol_indicator_gap" => "指标历史不足或存在缺口",
        _ => reason,
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (2..=144).contains(&self.ema_entry_hours)
                && (2..=144).contains(&self.ema_exit_hours)
                && (2..=168).contains(&self.momentum_hours)
                && (1..=72).contains(&self.entry_confirm_hours),
            "invalid SOL regime windows"
        );
        ensure!(
            (0.0..=0.1).contains(&self.min_momentum)
                && (self.min_momentum..=1.).contains(&self.max_momentum)
                && (0.1..=1.).contains(&self.entry_vol_fraction)
                && (0.001..=0.03).contains(&self.max_hourly_vol)
                && (0.005..=0.1).contains(&self.max_bar_range)
                && (0.0..=0.02).contains(&self.exit_buffer),
            "invalid SOL regime risk limits"
        );
        Ok(())
    }
    pub fn history_hours(&self) -> usize {
        (self.ema_entry_hours.max(self.ema_exit_hours) * 3)
            .max(self.momentum_hours + 1)
            .max(25)
    }
}
#[derive(Clone, Debug)]
pub struct Signals {
    pub entry: bool,
    pub exit: bool,
    pub last_close: f64,
    pub last_hour: u64,
    pub reason: String,
}
fn ema(rows: &[&Candle], n: usize) -> f64 {
    let a = 2. / (n as f64 + 1.);
    rows.iter()
        .skip(1)
        .fold(rows[0].close, |v, r| a * r.close + (1. - a) * v)
}
pub fn signals(c: &Config, s: &StrategyConfig, bars: &[Candle], now: u64) -> Result<Signals> {
    // 通用指标验证 OHLC、时间间隔和数据新鲜度；不读取尚未闭合的 K 线。
    let m = indicators::calculate(bars, s, now)?;
    let rows: Vec<_> = bars.iter().filter(|b| b.close_ms < now).collect();
    let need = c.history_hours();
    ensure!(rows.len() >= need, "SOL regime history insufficient");
    let rows = &rows[rows.len() - need..];
    for row in rows {
        ensure!(
            [row.open, row.high, row.low, row.close]
                .iter()
                .all(|v| v.is_finite() && *v > 0.)
                && row.high >= row.open.max(row.close)
                && row.low <= row.open.min(row.close),
            "invalid SOL regime OHLC"
        );
    }
    for w in rows.windows(2) {
        ensure!(
            w[1].open_ms == w[0].open_ms + 3_600_000,
            "SOL regime hourly gap"
        );
    }
    let n = rows.len();
    let last = rows[n - 1];
    let entry_ema = ema(rows, c.ema_entry_hours);
    let exit_ema = ema(rows, c.ema_exit_hours);
    let prev_exit_ema = ema(&rows[..n - 1], c.ema_exit_hours);
    let returns: Vec<_> = rows[n - 25..]
        .windows(2)
        .map(|w| (w[1].close / w[0].close).ln())
        .collect();
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let vol =
        (returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64).sqrt();
    let momentum = last.close / rows[n - 1 - c.momentum_hours].close - 1.;
    let high_vol = vol > c.max_hourly_vol
        || last.high / last.low - 1. > c.max_bar_range
        || m.vol_ratio > s.vol_pause_ratio;
    let down =
        m.downtrend || (last.close < exit_ema * (1. - c.exit_buffer) && exit_ema < prev_exit_ema);
    let fast = m.return_1h < -s.fast_drop_1h;
    Ok(Signals {
        entry: !high_vol
            && !down
            && !fast
            && last.close > entry_ema
            && momentum >= c.min_momentum
            && momentum <= c.max_momentum
            && vol <= c.max_hourly_vol * c.entry_vol_fraction
            && exit_ema >= prev_exit_ema,
        exit: high_vol || down || fast,
        last_close: last.close,
        last_hour: last.close_ms,
        reason: if high_vol {
            "sol_high_volatility"
        } else if down {
            "sol_downtrend"
        } else if fast {
            "sol_fast_drop"
        } else {
            "sol_trend_not_confirmed"
        }
        .into(),
    })
}
pub fn evaluate(c: &Config, s: &StrategyConfig, state: &mut Strategy, f: &MarketFrame) -> Decision {
    let signal = signals(c, s, &f.candles, f.now_ms);
    evaluate_with_signals(c, s, state, f, signal.as_ref().ok())
}
/// 回测可缓存只依赖已闭合 K 线的信号；运行器调用同一决策函数。
pub fn evaluate_with_signals(
    c: &Config,
    s: &StrategyConfig,
    state: &mut Strategy,
    f: &MarketFrame,
    sig: Option<&Signals>,
) -> Decision {
    state.observe_lp(!f.portfolio.positions.is_empty());
    let p = f.pool.price;
    let mut d = Decision {
        state: format!("{:?}", state.phase),
        reasons: vec![],
        lp: LpIntent::Hold,
        target_short_base: f.portfolio.short_base,
        emergency: false,
    };
    if !p.is_finite()
        || p <= 0.
        || !f.hedge_price.is_finite()
        || f.hedge_price <= 0.
        || !f.portfolio.equity(p).is_finite()
        || !f.portfolio.base().is_finite()
        || !f.portfolio.short_base.is_finite()
        || f.pool.time_ms > f.now_ms + 2000
        || f.hedge_time_ms > f.now_ms + 2000
        || f.now_ms.saturating_sub(f.pool.time_ms) > s.max_data_age_seconds * 1000
        || f.now_ms.saturating_sub(f.hedge_time_ms) > s.max_data_age_seconds * 1000
    {
        state.healthy_hours = 0;
        d.reasons
            .push("stale_or_invalid_data: no transactions".into());
        return d;
    }
    state.peak_equity = state
        .peak_equity
        .max(f.portfolio.equity(p))
        .max(s.total_capital);
    if f.portfolio.equity(p) < state.peak_equity * (1. - s.max_drawdown) {
        state.phase = Phase::Halted;
    }
    let basis = (p / f.hedge_price - 1.).abs() * 10000. > s.max_basis_bps;
    let fast_now = sig.is_some_and(|v| p / v.last_close - 1. < -s.fast_drop_1h);
    let large_now = sig.is_some_and(|v| (p / v.last_close - 1.).abs() > c.max_bar_range);
    let unsafe_now = sig.is_none_or(|v| v.exit) || fast_now || large_now || basis;
    if state.phase == Phase::Halted || unsafe_now {
        if state.phase != Phase::Halted {
            if state.phase != Phase::Paused {
                state.pause_since = f.now_ms;
            }
            state.phase = Phase::Paused;
        }
        state.fraction = 0.;
        state.healthy_hours = 0;
        state.layers.clear();
        d.lp = LpIntent::ExitToQuote;
        d.target_short_base = f.portfolio.base();
        d.emergency = true;
        d.reasons.push(if state.phase == Phase::Halted {
            "portfolio_drawdown_limit".into()
        } else if basis {
            "pool_perp_basis_limit".into()
        } else if fast_now {
            "sol_intrahour_fast_drop".into()
        } else if large_now {
            "sol_intrahour_large_move".into()
        } else {
            sig.map(|x| x.reason.clone())
                .unwrap_or("sol_indicator_gap".into())
        });
    } else if let Some(sig) = sig {
        if sig.last_hour > state.last_hour {
            if state.last_hour > 0 && sig.last_hour - state.last_hour != 3_600_000 {
                state.healthy_hours = 0;
            }
            state.healthy_hours = if sig.entry {
                state.healthy_hours + 1
            } else {
                0
            };
            state.last_hour = sig.last_hour;
        }
        if f.portfolio.positions.is_empty() {
            let cooled = state.entry_history == EntryHistory::Initial
                || f.now_ms.saturating_sub(state.pause_since) >= s.cooldown_hours * 3_600_000;
            if sig.entry && state.healthy_hours >= c.entry_confirm_hours && cooled {
                state.phase = Phase::Active;
                state.fraction = 1.;
                state.last_scale = f.now_ms;
                d.lp = LpIntent::Deploy { fraction: 1. };
            } else {
                state.phase = Phase::Paused;
                d.lp = LpIntent::ExitToQuote;
                d.emergency = true;
                d.reasons.push("sol_wait_for_trend_and_cooldown".into());
            }
        } else {
            state.phase = Phase::Active;
            // 越界后退出并等待下一次趋势确认，而非立即追价重建；减少单边下跌反复接盘。
            let outside = f
                .portfolio
                .positions
                .iter()
                .any(|x| p < x.lower || p > x.upper);
            if outside {
                state.phase = Phase::Paused;
                state.pause_since = f.now_ms;
                state.healthy_hours = 0;
                state.fraction = 0.;
                d.lp = LpIntent::ExitToQuote;
                d.emergency = true;
                d.reasons.push("sol_outside_range_exit_and_wait".into());
            }
        }
        d.target_short_base = f.portfolio.wallet_base
            + f.portfolio
                .positions
                .iter()
                .map(|x| {
                    x.base
                        * if p < x.lower || d.lp == LpIntent::ExitToQuote {
                            1.
                        } else {
                            s.inside_hedge_ratio
                        }
                })
                .sum::<f64>();
    }
    d.state = format!("{:?}", state.phase);
    d
}

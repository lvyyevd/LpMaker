use crate::{config::StrategyConfig, domain::Candle};
use anyhow::{Result, ensure};
#[derive(Clone, Debug)]
pub struct Metrics {
    pub return_1h: f64,
    pub vol_ratio: f64,
    pub recent_vol: f64,
    pub baseline_vol: f64,
    pub downtrend: bool,
    pub no_new_low: bool,
    pub ema_fast: f64,
    pub last_close: f64,
    pub last_close_ms: u64,
}
fn ema(xs: &[f64], period: usize) -> (f64, f64) {
    let a = 2.0 / (period as f64 + 1.0);
    let mut v = xs[0];
    let mut prev = v;
    for x in &xs[1..] {
        prev = v;
        v = a * x + (1.0 - a) * v;
    }
    (v, prev)
}
fn stddev(xs: &[f64]) -> f64 {
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64).sqrt()
}
pub fn calculate(candles: &[Candle], c: &StrategyConfig, now: u64) -> Result<Metrics> {
    let rows: Vec<_> = candles.iter().filter(|x| x.close_ms < now).collect();
    let need = (c.vol_long_hours + c.vol_short_hours + 1).max(c.ema_slow_hours * 3);
    ensure!(rows.len() >= need, "need {need} completed hourly candles");
    let rows = &rows[rows.len() - need..];
    for r in rows {
        ensure!(
            [r.open, r.high, r.low, r.close]
                .iter()
                .all(|x| x.is_finite() && *x > 0.0)
                && r.high >= r.open.max(r.close)
                && r.low <= r.open.min(r.close)
                && r.high >= r.low,
            "invalid candle"
        );
        ensure!(
            r.close_ms.checked_sub(r.open_ms) == Some(3_599_999),
            "not hourly candles"
        );
    }
    for w in rows.windows(2) {
        ensure!(
            w[1].open_ms == w[0].open_ms + 3_600_000,
            "hourly gap/duplicate/out-of-order"
        );
    }
    let last = rows.last().unwrap();
    ensure!(
        now.saturating_sub(last.close_ms) <= 3_660_000,
        "stale candle history"
    );
    let closes: Vec<_> = rows.iter().map(|x| x.close).collect();
    let returns: Vec<_> = closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let n = returns.len();
    let split = n - c.vol_short_hours;
    let baseline = stddev(&returns[split - c.vol_long_hours..split]);
    let recent = stddev(&returns[split..]);
    let vol_ratio = recent / baseline.max(1e-6);
    let (fast, fp) = ema(&closes, c.ema_fast_hours);
    let (slow, sp) = ema(&closes, c.ema_slow_hours);
    let downtrend = last.close < fast && fast < slow && fast < fp && slow < sp;
    let lookback = (c.resume_healthy_hours as usize).min(rows.len() - 1).max(2);
    let prev_low = rows[rows.len() - 1 - lookback..rows.len() - 1]
        .iter()
        .map(|r| r.low)
        .fold(f64::INFINITY, f64::min);
    Ok(Metrics {
        return_1h: last.close / closes[closes.len() - 2] - 1.0,
        vol_ratio,
        recent_vol: recent,
        baseline_vol: baseline,
        downtrend,
        no_new_low: last.low >= prev_low,
        ema_fast: fast,
        last_close: last.close,
        last_close_ms: last.close_ms,
    })
}

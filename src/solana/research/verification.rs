//! Finer-price and intrabar sensitivity, with explicit proxy-price provenance.
use super::{
    AccountModel, Bar, Data, HOUR, cached_signals,
    data::{num, read},
    load, simulate, simulate_account,
};
use crate::{domain::Candle, solana::config::Config};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};
/// Verify one frozen config on all available 5m pool observations. Earlier HL
/// 5m candles are outside the API retention window; explicitly model constant
/// within-hour spot/perp basis instead of claiming these are observed prices.
pub fn verify(c: &Config, path: &Path, output: &Path, apr: f64) -> Result<()> {
    verify_inner(c, path, output, apr, None).map(|_| ())
}
pub fn verify_account(
    c: &Config,
    path: &Path,
    output: &Path,
    apr: f64,
    native_sol: f64,
) -> Result<serde_json::Value> {
    verify_inner(c, path, output, apr, Some(AccountModel { native_sol }))
}
fn verify_inner(
    c: &Config,
    path: &Path,
    output: &Path,
    apr: f64,
    account: Option<AccountModel>,
) -> Result<serde_json::Value> {
    ensure!(
        apr.is_finite() && (0.0..=1.).contains(&apr),
        "invalid APR fraction"
    );
    ensure!(
        c.regime.is_some(),
        "verification needs an explicit SOL regime config"
    );
    let hourly = load(path)?;
    let hourly_signals = cached_signals(c, &hourly);
    let pool: Vec<Bar> = serde_json::from_value(read(path, "pool_5m.json")?)?;
    let observed_path = path.join("hyperliquid_5m_observed.json");
    let mut observed = BTreeMap::new();
    if observed_path.exists() {
        for x in read(path, "hyperliquid_5m_observed.json")?
            .as_array()
            .context("observed 5m")?
        {
            observed.insert(
                x["t"].as_u64().context("5m time")?,
                Candle {
                    open_ms: x["t"].as_u64().unwrap(),
                    close_ms: x["T"].as_u64().context("5m close time")?,
                    open: num(&x["o"])?,
                    high: num(&x["h"])?,
                    low: num(&x["l"])?,
                    close: num(&x["c"])?,
                },
            );
        }
    }
    let mut hedge = vec![];
    let mut signals = vec![];
    let mut actual = 0;
    let mut gaps = 0;
    for (i, b) in pool.iter().enumerate() {
        let t = b.timestamp * 1000;
        let hour = t / HOUR * HOUR;
        ensure!(t >= hourly.start && t < hourly.end, "5m outside period");
        ensure!(
            t.is_multiple_of(300000)
                && [b.open, b.high, b.low, b.close]
                    .iter()
                    .all(|x| x.is_finite() && *x > 0.)
                && b.high >= b.open.max(b.close)
                && b.low <= b.open.min(b.close),
            "invalid 5m pool OHLC"
        );
        if i > 0 {
            ensure!(t > pool[i - 1].timestamp * 1000, "duplicate 5m candle");
            if t != pool[i - 1].timestamp * 1000 + 300000 {
                gaps += 1;
            }
        }
        let ix = ((hour - hourly.start) / HOUR) as usize;
        let h = &hourly.hedge[hourly
            .hedge
            .binary_search_by_key(&hour, |x| x.open_ms)
            .map_err(|_| anyhow::anyhow!("hourly basis unavailable"))?];
        let ratio = h.open / hourly.pool[ix].open;
        hedge.push(if let Some(v) = observed.get(&t) {
            ensure!(
                v.close_ms == t + 299999
                    && [v.open, v.high, v.low, v.close]
                        .iter()
                        .all(|x| x.is_finite() && *x > 0.)
                    && v.high >= v.open.max(v.close)
                    && v.low <= v.open.min(v.close),
                "invalid observed 5m hedge OHLC"
            );
            actual += 1;
            v.clone()
        } else {
            Candle {
                open_ms: t,
                close_ms: t + 299999,
                open: b.open * ratio,
                high: b.high * ratio,
                low: b.low * ratio,
                close: b.close * ratio,
            }
        });
        signals.push(hourly_signals[ix].clone());
    }
    let mut fine = Data {
        step_ms: 300000,
        intrabar_stress: false,
        pool,
        hedge,
        funding: hourly.funding,
        start: hourly.start,
        end: hourly.end,
    };
    let replay = |d: &Data, start, rate, costs, curve| {
        if let Some(a) = account {
            simulate_account(c, d, &signals, start, d.end, rate, costs, curve, a)
        } else {
            simulate(c, d, &signals, start, d.end, rate, costs, curve)
        }
    };
    let full = replay(&fine, fine.start, apr, 1., true)?;
    let holdout = replay(&fine, fine.start + 122 * 24 * HOUR, apr, 1., true)?;
    fine.intrabar_stress = true;
    let adverse = replay(&fine, fine.start, apr, 1., true)?;
    let double_cost = replay(&fine, fine.start, apr, 2., false)?;
    let zero_fees = replay(&fine, fine.start, 0., 1., false)?;
    let low_apr = replay(&fine, fine.start, apr * 0.75, 1., false)?;
    let result = json!({"apr":apr,"pool_rows":fine.pool.len(),"gap_count":gaps,"observed_hl_5m_rows":actual,
        "synthetic_hedge_rows":fine.pool.len()-actual,"hedge_assumption":"missing historical HL5m uses pool5m times known hour-open basis; proxy, not measured executions",
        "full":full,"holdout":holdout,"low_first_stress":adverse,"double_cost_low_first":double_cost,"zero_fees_low_first":zero_fees,"apr_30_low_first":low_apr});
    std::fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    println!(
        "5m validation: full={:.4} holdout={:.4} low-first={:.4} doubled-cost={:.4}",
        full.pnl, holdout.pnl, adverse.pnl, double_cost.pnl
    );
    Ok(result)
}

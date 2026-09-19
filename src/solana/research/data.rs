//! Read-only data loader: reject missing/duplicate/invalid hours.
use super::HOUR;
use crate::domain::Candle;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};
#[derive(Clone, Deserialize)]
pub struct Bar {
    pub timestamp: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}
pub struct Data {
    pub step_ms: u64,
    pub intrabar_stress: bool,
    pub pool: Vec<Bar>,
    pub hedge: Vec<Candle>,
    pub funding: BTreeMap<u64, f64>,
    pub start: u64,
    pub end: u64,
}
pub(super) fn read(path: &Path, name: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path.join(name))?)?)
}
pub(super) fn num(v: &Value) -> Result<f64> {
    if let Some(x) = v.as_f64() {
        Ok(x)
    } else {
        Ok(v.as_str().context("number")?.parse()?)
    }
}
pub fn load(path: &Path) -> Result<Data> {
    let meta = read(path, "manifest.json")?;
    let start = meta["start_ms"].as_u64().context("start")?;
    let end = meta["end_ms"].as_u64().context("end")?;
    ensure!(
        end > start && start >= 432 * HOUR,
        "invalid research window"
    );
    let pool: Vec<Bar> = serde_json::from_value(read(path, "pool_1h.json")?)?;
    let hedge = read(path, "hyperliquid_1h.json")?
        .as_array()
        .context("HL bars")?
        .iter()
        .map(|v| {
            Ok(Candle {
                open_ms: v["t"].as_u64().context("time")?,
                close_ms: v["T"].as_u64().context("close time")?,
                open: num(&v["o"])?,
                high: num(&v["h"])?,
                low: num(&v["l"])?,
                close: num(&v["c"])?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut funding = BTreeMap::new();
    for v in read(path, "funding.json")?.as_array().context("funding")? {
        let t = v["time"].as_u64().context("funding time")? / HOUR * HOUR;
        let rate = num(&v["fundingRate"])?;
        ensure!(rate.is_finite() && rate.abs() <= 1., "invalid funding rate");
        ensure!(funding.insert(t, rate).is_none(), "duplicate funding hour");
    }
    ensure!(
        pool.len() as u64 == (end - start) / HOUR,
        "incomplete pool hours"
    );
    for (i, b) in pool.iter().enumerate() {
        ensure!(
            b.timestamp * 1000 == start + i as u64 * HOUR,
            "pool gap/duplicate"
        );
        ensure!(
            [b.open, b.high, b.low, b.close]
                .iter()
                .all(|v| v.is_finite() && *v > 0.)
                && b.high >= b.open.max(b.close)
                && b.low <= b.open.min(b.close),
            "invalid pool OHLC"
        );
        ensure!(funding.contains_key(&(b.timestamp * 1000)), "funding gap");
    }
    for w in hedge.windows(2) {
        ensure!(w[1].open_ms == w[0].open_ms + HOUR, "HL gap");
    }
    for h in &hedge {
        ensure!(
            h.close_ms.checked_sub(h.open_ms) == Some(HOUR - 1)
                && [h.open, h.high, h.low, h.close]
                    .iter()
                    .all(|v| v.is_finite() && *v > 0.)
                && h.high >= h.open.max(h.close)
                && h.low <= h.open.min(h.close),
            "invalid hedge OHLC"
        );
    }
    ensure!(
        hedge.first().context("no HL")?.open_ms <= start - 432 * HOUR
            && hedge.last().unwrap().close_ms == end - 1,
        "HL coverage insufficient"
    );
    Ok(Data {
        step_ms: HOUR,
        intrabar_stress: false,
        pool,
        hedge,
        funding,
        start,
        end,
    })
}

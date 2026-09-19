//! 可复现研究回放：使用真实观测，明确区分“未计个人手续费”的损益和假设手续费情景。
use super::{
    config::Config,
    dlmm::{Paper, strategy_snapshot},
};
use crate::{
    domain::{Candle, MarketFrame},
    strategy::Strategy,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
#[derive(Clone, Deserialize)]
struct PoolBar {
    timestamp: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}
#[derive(Clone)]
struct Row {
    time: u64,
    pool: f64,
    hedge: f64,
    fees: f64,
    fee_complete: bool,
    close_pool: f64,
    close_hedge: f64,
}
#[derive(Clone, Debug, Serialize)]
struct Outcome {
    start_ms: u64,
    end_ms: u64,
    observations: usize,
    pnl_excluding_lp_fees: f64,
    return_excluding_lp_fees_pct: f64,
    max_drawdown_pct: f64,
    lp_active_hours: f64,
    lp_operations: u64,
    hedge_fees: f64,
    swap_costs: f64,
    operation_costs: f64,
    funding_pnl: f64,
    fee_data_missing_bars: usize,
    fee_sensitivity: Vec<Value>,
    break_even_fee_usd: f64,
    phase: String,
}
struct Data {
    rows: Vec<Row>,
    hourly: Vec<Candle>,
    funding: Vec<(u64, f64)>,
    step: u64,
}
fn read(path: &Path, name: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path.join(name))?)?)
}
fn num(v: &Value) -> Result<f64> {
    if let Some(x) = v.as_f64() {
        Ok(x)
    } else {
        Ok(v.as_str().context("numeric string")?.parse()?)
    }
}
fn load(path: &Path, fine: bool) -> Result<Data> {
    let bars: Vec<PoolBar> = serde_json::from_value(read(path, "pool.json")?)?;
    for w in bars.windows(2) {
        ensure!(w[1].timestamp == w[0].timestamp + 300, "pool candle gap");
    }
    for b in &bars {
        ensure!(
            [b.open, b.high, b.low, b.close]
                .iter()
                .all(|x| x.is_finite() && *x > 0.)
                && b.high >= b.open.max(b.close)
                && b.low <= b.open.min(b.close),
            "invalid pool OHLC"
        );
    }
    let hourly = read(path, "hyperliquid_1h.json")?
        .as_array()
        .context("hourly array")?
        .iter()
        .map(|v| {
            Ok(Candle {
                open_ms: v["t"].as_u64().context("open time")?,
                close_ms: v["T"].as_u64().context("close time")?,
                open: num(&v["o"])?,
                high: num(&v["h"])?,
                low: num(&v["l"])?,
                close: num(&v["c"])?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let hl = read(
        path,
        if fine {
            "hyperliquid_5m.json"
        } else {
            "hyperliquid_1h.json"
        },
    )?;
    let mut quotes = BTreeMap::new();
    for v in hl.as_array().context("candles")? {
        quotes.insert(
            v["t"].as_u64().context("time")?,
            (num(&v["o"])?, num(&v["c"])?),
        );
    }
    let fees = read(path, "fees.json")?;
    let mut fee_rows = BTreeMap::new();
    for v in fees.as_array().context("fees")? {
        fee_rows.insert(
            v["timestamp"].as_u64().context("fee time")?,
            (num(&v["fees"])?, num(&v["volume"])?),
        );
    }
    let step = if fine { 300000 } else { 3600000 };
    let mut rows = vec![];
    for chunk in bars.chunks(if fine { 1 } else { 12 }) {
        let b = &chunk[0];
        let time = b.timestamp * 1000;
        let Some(hedge) = quotes.get(&time) else {
            continue;
        };
        ensure!(
            chunk.len() == if fine { 1 } else { 12 },
            "incomplete pool hour"
        );
        let mut fees = 0.;
        let mut complete = true;
        for b in chunk {
            match fee_rows.get(&b.timestamp) {
                Some((f, v)) if *v > 0. || b.volume == 0. => fees += f,
                _ => complete = false,
            };
        }
        rows.push(Row {
            time,
            pool: b.open,
            hedge: hedge.0,
            close_hedge: hedge.1,
            close_pool: chunk.last().unwrap().close,
            fees,
            fee_complete: complete,
        });
    }
    for w in rows.windows(2) {
        ensure!(
            w[1].time == w[0].time + step,
            "paired price data gap; never fill missing prices"
        );
    }
    let funding = read(path, "funding.json")?
        .as_array()
        .context("funding")?
        .iter()
        .map(|v| {
            Ok((
                v["time"].as_u64().context("funding time")?,
                num(&v["fundingRate"])?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(rows.len() > 200, "insufficient overlapping data");
    Ok(Data {
        rows,
        hourly,
        funding,
        step,
    })
}
fn simulate(c: &Config, data: &Data, start: u64, end: u64, fee_rate: f64) -> Result<Outcome> {
    let mut s = c.strategy.clone();
    // 小时数据看不到 15 分钟信号，以整小时取样和等时长确认近似；细粒度回放保留原 15 分钟设置。
    if data.step == 3600000 {
        s.breakout_confirm_bars =
            (s.decision_bar_seconds * s.breakout_confirm_bars as u64).div_ceil(3600) as u32;
        s.decision_bar_seconds = 3600;
    }
    let mut paper = Paper::new(&s);
    paper.leverage = c.hyperliquid.leverage;
    let mut state = Strategy::default();
    let mut peak = s.total_capital;
    let mut dd: f64 = 0.;
    let mut active = 0.;
    let mut total = 0;
    let mut missing = 0;
    let mut scenario = [0.; 3];
    let rows: Vec<_> = data
        .rows
        .iter()
        .filter(|r| r.time >= start && r.time < end)
        .collect();
    ensure!(!rows.is_empty(), "empty backtest period");
    let mut previous = start;
    let mut last_time = start;
    let mut final_price = rows[0].pool;
    let mut final_hedge = rows[0].hedge;
    for r in rows {
        // Funding applies to the hedge already held BEFORE this observation, never to a just-opened order.
        for (t, rate) in data
            .funding
            .iter()
            .filter(|(t, _)| *t > previous && *t <= r.time)
        {
            paper.funding(*rate, r.hedge, *t);
        }
        previous = r.time;
        paper.mark(r.pool, r.hedge);
        let p = paper.portfolio(&s);
        let equity = p.equity(r.pool);
        peak = peak.max(equity);
        dd = dd.max((peak - equity) / peak);
        let f = MarketFrame {
            now_ms: r.time,
            pool: strategy_snapshot(r.pool, r.time),
            hedge_price: r.hedge,
            hedge_time_ms: r.time,
            candles: data
                .hourly
                .iter()
                .filter(|b| b.close_ms < r.time)
                .cloned()
                .collect(),
            portfolio: p,
        };
        let d = state.evaluate(&s, &f);
        if !d.reasons.iter().any(|r| r.starts_with("stale_or_invalid")) {
            // $0.10 is an explicit per-workflow gas/priority-cost assumption; rent is refundable locked capital, not yield.
            paper.apply(
                &d,
                &s,
                r.pool,
                r.hedge,
                r.time,
                state.fraction,
                0.10,
                fee_rate,
            )?;
        }
        state.observe_lp(!paper.positions.is_empty());
        let after = paper.portfolio(&s).equity(r.pool);
        peak = peak.max(after);
        dd = dd.max((peak - after) / peak);
        if !paper.positions.is_empty() {
            active += data.step as f64 / 3600000.;
        }
        // Assumed competing capital IN THE ACTIVE BIN. Not historical TVL and not an observed LP fee allocation.
        let own = paper
            .positions
            .iter()
            .filter(|p| {
                let b = p.position();
                r.pool >= b.lower && r.pool <= b.upper
            })
            .map(|p| {
                let pos = p.position();
                (pos.base * r.pool + pos.quote) / p.bins.len() as f64
            })
            .sum::<f64>();
        if r.fee_complete {
            for (i, capital) in [5000., 25000., 100000.].iter().enumerate() {
                scenario[i] += r.fees * own / (capital + own);
            }
        } else {
            missing += 1;
        }
        total += 1;
        last_time = r.time;
        final_price = r.close_pool;
        final_hedge = r.close_hedge;
    }
    paper.mark(final_price, final_hedge);
    let final_equity = paper.portfolio(&s).equity(final_price);
    peak = peak.max(final_equity);
    dd = dd.max((peak - final_equity) / peak);
    let pnl = final_equity - s.total_capital;
    Ok(Outcome{start_ms:start,end_ms:last_time+data.step,observations:total,pnl_excluding_lp_fees:pnl,return_excluding_lp_fees_pct:pnl/s.total_capital*100.,max_drawdown_pct:dd*100.,lp_active_hours:active,lp_operations:paper.operations,hedge_fees:paper.hedge_fees,swap_costs:paper.swap_costs,operation_costs:paper.operation_costs,funding_pnl:paper.funding_pnl,fee_data_missing_bars:missing,fee_sensitivity:[5000.,25000.,100000.].iter().enumerate().map(|(i,v)|json!({"assumed_other_active_bin_capital_usd":v,"hypothetical_personal_fees_usd":scenario[i],"pnl_plus_hypothetical_fees_usd":pnl+scenario[i],"missing_fee_periods_excluded":true})).collect(),break_even_fee_usd:(-pnl).max(0.),phase:format!("{:?}",state.phase)})
}
pub fn run(c: &Config, path: &Path, output: &Path) -> Result<()> {
    let data = load(path, false)?;
    let fine = load(path, true)?;
    let manifest = read(path, "manifest.json")?;
    let start = manifest["start_ms"].as_u64().context("start")?;
    let end = manifest["end_ms"].as_u64().context("end")?;
    let split = start + 21 * 86400000;
    ensure!(
        data.rows.first().context("rows")?.time == start
            && data.rows.last().unwrap().time + data.step == end,
        "month coverage mismatch"
    );
    let mut candidates = vec![];
    let rent = read(path, "rent.json")?;
    let base_rent = rent["base_position_lamports"]
        .as_f64()
        .context("base position rent")?
        / 1e9;
    let per_extra_bin = rent["extra_bin_lamports"]
        .as_f64()
        .context("extra bin rent")?
        / 1e9;
    let rent_for = |width: f64| {
        let bins = 2. * (width.ln_1p() / 1.0004_f64.ln()).ceil() + 1.;
        base_rent + (bins - 70.).max(0.) * per_extra_bin
    };
    for (wide, narrow) in [(0.012, 0.004), (0.025, 0.008), (0.04, 0.012), (0.08, 0.02)] {
        for ratio in [0., 0.5, 1.] {
            let mut candidate = c.clone();
            candidate.strategy.layers[0].half_width = wide;
            candidate.strategy.layers[1].half_width = narrow;
            candidate.strategy.inside_hedge_ratio = ratio;
            let train = simulate(&candidate, &data, start, split, 0.00075)?;
            let score = train.pnl_excluding_lp_fees
                - 2. * train.max_drawdown_pct / 100. * c.strategy.total_capital;
            let rents = [rent_for(wide), rent_for(narrow)];
            let rent_and_gas =
                (rents.iter().sum::<f64>() + candidate.solana.min_native_sol + 0.004)
                    * data.rows[0].pool;
            let feasible = rents.iter().all(|x| *x <= candidate.solana.max_rent_sol)
                && rent_and_gas <= candidate.strategy.reserve;
            candidates.push((score, candidate, train, feasible, rent_and_gas));
        }
    }
    candidates.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| b.0.total_cmp(&a.0)));
    ensure!(candidates[0].3, "no candidate fits rent reserve");
    let selected = &candidates[0].1;
    let all=candidates.iter().map(|(score,cfg,train,feasible,rent_and_gas)|Ok(json!({"wide":cfg.strategy.layers[0].half_width,"narrow":cfg.strategy.layers[1].half_width,"inside_hedge_ratio":cfg.strategy.inside_hedge_ratio,"train_score":score,"initial_rent_budget_feasible":feasible,"initial_rent_plus_gas_reserve_usd":rent_and_gas,"train_21_days":train,"test_9_days":simulate(cfg,&data,split,end,0.00075)?,"month":simulate(cfg,&data,start,end,0.00075)?}))).collect::<Result<Vec<_>>>()?;
    let result = json!({"data":manifest,"rent_schedule":rent,"selection":"Filter initial rent budget using observed current rent schedule, then first 21 days only: pre-personal-fee PnL minus 2 * dollar maximum drawdown. Last 9 days held out. No optimization on hypothetical fees.","assumptions":["30-day hourly execution approximation; cannot resolve 15-second risk or 15-minute breakout paths","Separate 5-minute overlapping-period sensitivity, NOT a complete one-month 5m test","Discrete equal-value bins model; live uses official SDK Spot with actual bin composition","No historical per-bin liquidity shares: personal fee sensitivity is hypothetical, not realized income","Taker 4.5bps + 3bps slippage on hedge notional; maker-only fee case assumes fills and is not an executable guarantee","Inventory swap cost 34bps, operation gas/priority $0.10; rent locks capital and is not expensed here","No queued maker fills, funding timestamp mark approximated at observation; no liquidation engine","Assumed fee credits do not feed back into sizing/drawdown; missing fee observations excluded, not fabricated","Open positions marked to market at final completed candle close; closing and cash-out costs not charged"],"selected":{"wide":selected.strategy.layers[0].half_width,"narrow":selected.strategy.layers[1].half_width,"inside_hedge_ratio":selected.strategy.inside_hedge_ratio},"candidates":all,"selected_fine_overlap":simulate(selected,&fine,fine.rows[0].time,end,0.00075)?,"selected_maker_fee_optimistic":simulate(selected,&data,start,end,0.00015)?,"cash_baseline_pnl":0.0});
    std::fs::create_dir_all(output.parent().context("output directory")?)?;
    std::fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    let mut chosen = selected.clone();
    chosen.mode = crate::config::Mode::Paper;
    std::fs::write(
        output.with_file_name("selected-paper.toml"),
        toml::to_string_pretty(&chosen)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn training_does_not_read_future_prices_or_future_candles() {
        let c = Config::load("config/solana.toml").unwrap();
        let start = 2000 * 3600000;
        let mut data = Data {
            step: 3600000,
            funding: vec![],
            hourly: vec![],
            rows: vec![],
        };
        for i in 0..220_u64 {
            let time = start - 200 * 3600000 + i * 3600000;
            let price = 100. + i as f64 * 0.02;
            data.hourly.push(Candle {
                open_ms: time,
                close_ms: time + 3599999,
                open: price,
                high: price + 0.1,
                low: price - 0.1,
                close: price,
            });
        }
        for i in 0..20_u64 {
            data.rows.push(Row {
                time: start + i * 3600000,
                pool: 104. + i as f64 * 0.02,
                hedge: 104. + i as f64 * 0.02,
                close_pool: 104.01 + i as f64 * 0.02,
                close_hedge: 104.01 + i as f64 * 0.02,
                fees: 0.,
                fee_complete: false,
            });
        }
        let end = start + 10 * 3600000;
        let first = simulate(&c, &data, start, end, 0.00075).unwrap();
        for r in data.rows.iter_mut().filter(|r| r.time >= end) {
            r.pool = 10000.;
            r.hedge = 1.;
            r.fees = 1e9;
        }
        for r in data.hourly.iter_mut().filter(|r| r.open_ms >= end) {
            r.close = 10000.;
            r.high = 10001.;
        }
        let changed = simulate(&c, &data, start, end, 0.00075).unwrap();
        assert_eq!(first.pnl_excluding_lp_fees, changed.pnl_excluding_lp_fees);
        assert_eq!(first.lp_operations, changed.lp_operations);
        assert_eq!(first.fee_data_missing_bars, 10);
    }
}

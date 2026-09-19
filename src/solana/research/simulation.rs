//! Discrete DLMM inventory, APR cash flow, hedge and execution-cost ledger.
use super::{Bar, Data, HOUR};
use crate::{
    domain::{Decision, LpIntent, MarketFrame},
    solana::{
        config::Config,
        dlmm::{Paper, strategy_snapshot},
        regime,
    },
    strategy::Strategy,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;
const YEAR_HOURS: f64 = 365. * 24.;
#[derive(Clone, Debug, Serialize)]
pub struct Point {
    pub time_ms: u64,
    pub price: f64,
    pub equity: f64,
    pub lp_value: f64,
    pub fees: f64,
    pub phase: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub start_ms: u64,
    pub end_ms: u64,
    pub pnl: f64,
    pub lp_fees: f64,
    pub price_and_hedge_pnl: f64,
    pub hedge_cost: f64,
    pub swap_cost: f64,
    pub operation_cost: f64,
    pub funding: f64,
    pub max_drawdown_pct: f64,
    pub active_hours: f64,
    pub eligible_fee_hours: f64,
    pub operations: u64,
    pub phase: String,
    pub exits: BTreeMap<String, u64>,
    pub equity_curve: Vec<Point>,
}
/// No fee is earned during cash periods or a bar which breaches the range.
/// The smaller endpoint principal is used: no full-budget APR on a partial/reduced position.
pub fn fee_for_bar(paper: &Paper, b: &Bar, apr: f64, hours: f64) -> f64 {
    paper
        .positions
        .iter()
        .map(|position| {
            let p = position.position();
            if b.low < p.lower || b.high > p.upper {
                0.
            } else {
                let start_value = p.base * b.open + p.quote;
                let mut end = position.clone();
                end.mark(b.close);
                let q = end.position();
                start_value.min(q.base * b.close + q.quote).max(0.) * apr * hours / YEAR_HOURS
            }
        })
        .sum()
}
fn frame(c: &Config, paper: &Paper, t: u64, p: f64, h: f64) -> MarketFrame {
    MarketFrame {
        now_ms: t,
        pool: strategy_snapshot(p, t),
        hedge_price: h,
        hedge_time_ms: t,
        candles: vec![],
        portfolio: paper.portfolio(&c.strategy),
    }
}
pub fn cached_signals(c: &Config, data: &Data) -> Vec<Option<regime::Signals>> {
    data.pool
        .iter()
        .map(|b| {
            let t = b.timestamp * 1000;
            let end = data.hedge.partition_point(|x| x.close_ms < t);
            let begin = end.saturating_sub(450);
            regime::signals(
                c.regime.as_ref().unwrap(),
                &c.strategy,
                &data.hedge[begin..end],
                t,
            )
            .ok()
        })
        .collect()
}
#[allow(clippy::too_many_arguments)]
pub fn simulate(
    c: &Config,
    data: &Data,
    signals: &[Option<regime::Signals>],
    start: u64,
    end: u64,
    apr: f64,
    cost_multiplier: f64,
    curve: bool,
) -> Result<Outcome> {
    let mut paper = Paper::new(&c.strategy);
    paper.leverage = c.hyperliquid.leverage;
    let mut state = Strategy::default();
    let mut peak = c.strategy.total_capital;
    let mut dd: f64 = 0.;
    let mut fees = 0.;
    let mut active = 0.;
    let mut eligible = 0.;
    let mut exits = BTreeMap::new();
    let mut points = vec![];
    let mut last = (0., 0.);
    for (i, b) in data
        .pool
        .iter()
        .enumerate()
        .filter(|(_, b)| b.timestamp * 1000 >= start && b.timestamp * 1000 < end)
    {
        let t = b.timestamp * 1000;
        let hi = data
            .hedge
            .binary_search_by_key(&t, |x| x.open_ms)
            .map_err(|_| anyhow::anyhow!("missing paired HL hour"))?;
        let h = &data.hedge[hi];
        paper.mark(b.open, h.open);
        // Settlement occurs a few ms after the hour. Attribute it to pre-decision inventory;
        // funding is never used as a predictive feature and cannot credit a new position retroactively.
        let funding_time = t / HOUR * HOUR;
        paper.funding(data.funding[&funding_time], h.open, funding_time);
        if i > 0 && t > data.pool[i - 1].timestamp * 1000 + data.step_ms {
            state.phase = crate::strategy::Phase::Paused;
            state.pause_since = t;
            state.healthy_hours = 0;
            let exit = Decision {
                state: "DataGap".into(),
                reasons: vec![],
                lp: LpIntent::ExitToQuote,
                target_short_base: 0.,
                emergency: true,
            };
            paper.apply(
                &exit,
                &c.strategy,
                b.open,
                h.open,
                t,
                0.,
                0.1 * cost_multiplier,
                0.00075 * cost_multiplier,
            )?;
        }
        let f = frame(c, &paper, t, b.open, h.open);
        let d = regime::evaluate_with_signals(
            c.regime.as_ref().unwrap(),
            &c.strategy,
            &mut state,
            &f,
            signals[i].as_ref(),
        );
        if d.lp == LpIntent::ExitToQuote && !paper.positions.is_empty() {
            for reason in &d.reasons {
                *exits.entry(reason.clone()).or_insert(0) += 1;
            }
        }
        let old_swaps = paper.swap_costs;
        paper.apply(
            &d,
            &c.strategy,
            b.open,
            h.open,
            t,
            state.fraction,
            0.10 * cost_multiplier,
            0.00075 * cost_multiplier,
        )?;
        if matches!(d.lp, LpIntent::Deploy { .. } | LpIntent::Recenter { .. }) {
            // The real SOL runner fully hedges the newly swapped WSOL during
            // multi-transaction minting, then reduces to the strategic hedge.
            // Include that extra turnover instead of assuming a cost-free mint.
            let inventory = paper.portfolio(&c.strategy);
            let target = crate::engine::inventory_hedge_target(&c.strategy, &inventory, b.open);
            paper.hedge(
                inventory.base(),
                h.open,
                true,
                &c.strategy,
                0.00075 * cost_multiplier,
            );
            paper.hedge(target, h.open, true, &c.strategy, 0.00075 * cost_multiplier);
        }
        // Paper's quote swap baseline is 34 bps. Stress multiplier also covers those costs.
        let extra = (paper.swap_costs - old_swaps) * (cost_multiplier - 1.);
        paper.cash -= extra;
        paper.swap_costs += extra;
        state.observe_lp(!paper.positions.is_empty());
        if data.intrabar_stress && !paper.positions.is_empty() {
            // Feasible low-first intrabar path, not a claim about the unknowable tick sequence.
            // Use contemporaneous basis rather than pairing unrelated spot/perp extrema.
            let low_hedge = b.low * h.open / b.open;
            paper.mark(b.low, low_hedge);
            let f = frame(c, &paper, t + data.step_ms / 3, b.low, low_hedge);
            let low_equity = f.portfolio.equity(b.low);
            peak = peak.max(low_equity);
            dd = dd.max((peak - low_equity) / peak);
            let risk = regime::evaluate_with_signals(
                c.regime.as_ref().unwrap(),
                &c.strategy,
                &mut state,
                &f,
                signals[i].as_ref(),
            );
            if risk.lp == LpIntent::ExitToQuote {
                for reason in &risk.reasons {
                    *exits.entry(format!("intrabar:{reason}")).or_insert(0) += 1;
                }
                let old = paper.swap_costs;
                paper.apply(
                    &risk,
                    &c.strategy,
                    b.low,
                    low_hedge,
                    f.now_ms,
                    0.,
                    0.1 * cost_multiplier,
                    0.00075 * cost_multiplier,
                )?;
                let extra = (paper.swap_costs - old) * (cost_multiplier - 1.);
                paper.cash -= extra;
                paper.swap_costs += extra;
            }
            paper.mark(b.open, h.open);
        }
        let hours = data.step_ms as f64 / HOUR as f64;
        if !paper.positions.is_empty() {
            active += hours;
        }
        let fee = fee_for_bar(&paper, b, apr, hours);
        if fee > 0. {
            eligible += hours;
        }
        // Credit before the NEXT decision, so fees participate in risk and drawdown immediately.
        paper.cash += fee;
        fees += fee;
        paper.mark(b.close, h.close);
        let p = paper.portfolio(&c.strategy);
        let equity = p.equity(b.close);
        peak = peak.max(equity);
        dd = dd.max((peak - equity) / peak);
        if curve && (t + data.step_ms).is_multiple_of(HOUR) {
            points.push(Point {
                time_ms: t + data.step_ms,
                price: b.close,
                equity,
                lp_value: p.positions.iter().map(|x| x.base * b.close + x.quote).sum(),
                fees,
                phase: format!("{:?}", state.phase),
            });
        }
        last = (b.close, h.close);
    }
    ensure!(last.0 > 0., "empty research interval");
    // Charge terminal liquidation: output is realizable cash-model PnL, not an unclosed mark.
    let exit = Decision {
        state: "EndOfTest".into(),
        reasons: vec![],
        lp: LpIntent::ExitToQuote,
        target_short_base: 0.,
        emergency: true,
    };
    let old_swaps = paper.swap_costs;
    paper.apply(
        &exit,
        &c.strategy,
        last.0,
        last.1,
        end,
        0.,
        0.1 * cost_multiplier,
        0.00075 * cost_multiplier,
    )?;
    let extra = (paper.swap_costs - old_swaps) * (cost_multiplier - 1.);
    paper.cash -= extra;
    paper.swap_costs += extra;
    let pnl = paper.portfolio(&c.strategy).equity(last.0) - c.strategy.total_capital;
    dd = dd.max((peak - c.strategy.total_capital - pnl) / peak);
    if curve {
        points.push(Point {
            time_ms: end,
            price: last.0,
            equity: c.strategy.total_capital + pnl,
            lp_value: 0.,
            fees,
            phase: "ClosedForValuation".into(),
        });
    }
    Ok(Outcome {
        start_ms: start,
        end_ms: end,
        pnl,
        lp_fees: fees,
        price_and_hedge_pnl: pnl - fees,
        hedge_cost: paper.hedge_fees,
        swap_cost: paper.swap_costs,
        operation_cost: paper.operation_costs,
        funding: paper.funding_pnl,
        max_drawdown_pct: dd * 100.,
        active_hours: active,
        eligible_fee_hours: eligible,
        operations: paper.operations,
        phase: format!("{:?}", state.phase),
        exits,
        equity_curve: points,
    })
}

//! 模拟执行账本：不签名、不发交易；LP 手续费仍按原有约定不模拟。
use super::inventory_hedge_target;
use crate::{config::Config, domain::*, hyperliquid::num, math};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperOrder {
    pub target: f64,
    pub buy: bool,
    pub price: f64,
    pub submitted: u64,
    pub emergency: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paper {
    pub portfolio: Portfolio,
    pub last_hedge_price: f64,
    pub pending: Option<PaperOrder>,
    pub hedge_fees: f64,
    pub swap_costs: f64,
    pub funding_pnl: f64,
    pub last_funding_ms: u64,
}
impl Paper {
    pub fn new(c: &Config) -> Self {
        Self {
            portfolio: Portfolio {
                positions: vec![],
                wallet_base: 0.0,
                wallet_quote: c.strategy.lp_budget,
                short_base: 0.0,
                hedge_equity: c.strategy.hedge_collateral,
                reserve: c.strategy.reserve,
            },
            last_hedge_price: 0.0,
            pending: None,
            hedge_fees: 0.0,
            swap_costs: 0.0,
            funding_pnl: 0.0,
            last_funding_ms: crate::now_ms(),
        }
    }
    pub fn mark(&mut self, p: f64, hedge: f64, now: u64, c: &Config) {
        if self.last_hedge_price > 0.0 {
            self.portfolio.hedge_equity +=
                self.portfolio.short_base * (self.last_hedge_price - hedge);
        }
        self.last_hedge_price = hedge;
        for pos in &mut self.portfolio.positions {
            (pos.base, pos.quote) = math::amounts(pos.liquidity, pos.lower, pos.upper, p);
        }
        if let Some(o) = self.pending.clone() {
            let crossed = now > o.submitted
                && if o.buy {
                    hedge < o.price
                } else {
                    hedge > o.price
                };
            let expired =
                now.saturating_sub(o.submitted) >= c.hyperliquid.maker_wait_seconds * 1000;
            if crossed {
                self.fill(o.target, o.price, 0.00015);
                self.pending = None;
            } else if expired {
                if o.emergency {
                    let execution = hedge
                        * (1.0
                            + if o.buy { 1.0 } else { -1.0 }
                                * c.hyperliquid.emergency_slippage_bps as f64
                                / 10000.0);
                    self.fill(o.target, execution, 0.00045);
                }
                self.pending = None;
            }
        }
    }
    fn fill(&mut self, target: f64, execution: f64, fee_rate: f64) {
        let delta = target - self.portfolio.short_base;
        let fee = delta.abs() * execution * fee_rate;
        self.portfolio.hedge_equity += delta * (execution - self.last_hedge_price) - fee;
        self.hedge_fees += fee;
        self.portfolio.short_base = target;
    }
    fn swap_to_base(&mut self, target: f64, p: f64, c: &Config) {
        let delta = target - self.portfolio.wallet_base;
        let cost = delta.abs()
            * p
            * (c.liquidity.fee as f64 / 1_000_000.0 + c.liquidity.slippage_bps as f64 / 10000.0);
        self.portfolio.wallet_quote -= delta * p + cost;
        self.portfolio.wallet_base = target;
        self.swap_costs += cost;
    }
    fn remove(&mut self, layers: &[String]) {
        self.portfolio.positions.retain(|pos| {
            if layers.contains(&pos.layer) {
                self.portfolio.wallet_base += pos.base;
                self.portfolio.wallet_quote += pos.quote;
                false
            } else {
                true
            }
        });
    }
    pub fn apply(&mut self, d: &Decision, c: &Config, p: f64, hedge: f64, now: u64) -> Result<()> {
        self.apply_with_widths(d, c, p, hedge, now, None)
    }
    pub fn apply_with_widths(
        &mut self,
        d: &Decision,
        c: &Config,
        p: f64,
        hedge: f64,
        now: u64,
        widths: Option<(f64, f64)>,
    ) -> Result<()> {
        if crate::strategy::eth::band::config(&c.strategy).is_some()
            && matches!(d.lp, LpIntent::Deploy { .. } | LpIntent::Recenter { .. })
        {
            ensure!(
                widths.is_some(),
                "band paper entry requires observed hourly feature"
            );
        }
        match &d.lp {
            LpIntent::Hold => {}
            LpIntent::ExitToQuote => {
                self.pending = None;
                let names = self
                    .portfolio
                    .positions
                    .iter()
                    .map(|p| p.layer.clone())
                    .collect::<Vec<_>>();
                self.remove(&names);
                self.swap_to_base(0.0, p, c);
                // Paper cash exit is synchronous; live exit documents the cross-venue gap.
                if self.portfolio.short_base > 0.0 {
                    self.fill(
                        0.0,
                        hedge * (1.0 + c.hyperliquid.emergency_slippage_bps as f64 / 10000.0),
                        0.00045,
                    );
                }
                return Ok(());
            }
            LpIntent::Deploy { fraction } if !self.portfolio.positions.is_empty() => {
                for index in 0..self.portfolio.positions.len() {
                    let pos = self.portfolio.positions[index].clone();
                    let layer = c
                        .strategy
                        .layers
                        .iter()
                        .find(|l| l.name == pos.layer)
                        .context("unknown paper layer")?;
                    let desired =
                        c.strategy.lp_budget * fraction * layer.weight - (pos.base * p + pos.quote);
                    let available = self.portfolio.wallet_base * p + self.portfolio.wallet_quote;
                    let value = desired.min(available) * 0.995;
                    if value < 10.0 {
                        continue;
                    }
                    ensure!(
                        p > pos.lower && p < pos.upper,
                        "cannot scale an out-of-range paper position"
                    );
                    let added = math::liquidity_for_value(value, pos.lower, pos.upper, p)?;
                    let (b, q) = math::amounts(added, pos.lower, pos.upper, p);
                    self.swap_to_base(b, p, c);
                    ensure!(
                        self.portfolio.wallet_quote >= q,
                        "insufficient paper scaling quote"
                    );
                    self.portfolio.wallet_base -= b;
                    self.portfolio.wallet_quote -= q;
                    let pos = &mut self.portfolio.positions[index];
                    pos.liquidity += added;
                    (pos.base, pos.quote) = math::amounts(pos.liquidity, pos.lower, pos.upper, p);
                }
            }
            LpIntent::Deploy { .. } | LpIntent::Recenter { .. } => {
                let layers: Vec<String> = match &d.lp {
                    LpIntent::Recenter { layers } => layers.clone(),
                    _ => c.strategy.layers.iter().map(|l| l.name.clone()).collect(),
                };
                self.remove(&layers);
                let available = self.portfolio.wallet_base * p + self.portfolio.wallet_quote;
                let weight_sum = c
                    .strategy
                    .layers
                    .iter()
                    .filter(|l| layers.contains(&l.name))
                    .map(|l| l.weight)
                    .sum::<f64>();
                let budget = match d.lp {
                    LpIntent::Deploy { fraction } => c.strategy.lp_budget * fraction,
                    _ => {
                        if widths.is_some() {
                            c.strategy.lp_budget
                        } else {
                            available
                        }
                    }
                }
                .min(available)
                    * 0.995;
                for l in c
                    .strategy
                    .layers
                    .iter()
                    .filter(|l| layers.contains(&l.name))
                {
                    let value = budget * l.weight / weight_sum;
                    let (lo, hi) = if let Some(w) = widths {
                        (p * (1. - w.0), p * (1. + w.1))
                    } else {
                        math::range(p, l.half_width)
                    };
                    let liquidity = math::liquidity_for_value(value, lo, hi, p)?;
                    let (b, q) = math::amounts(liquidity, lo, hi, p);
                    self.swap_to_base(b, p, c);
                    ensure!(
                        self.portfolio.wallet_quote + 1e-6 >= q,
                        "paper insufficient quote"
                    );
                    self.portfolio.wallet_base -= b;
                    self.portfolio.wallet_quote -= q;
                    self.portfolio.positions.push(LpPosition {
                        layer: l.name.clone(),
                        token_id: None,
                        lower: lo,
                        upper: hi,
                        liquidity,
                        raw_liquidity: "paper".into(),
                        unclaimed_base: 0.0,
                        unclaimed_quote: 0.0,
                        base: b,
                        quote: q,
                    });
                }
            }
        }
        let target = if matches!(d.lp, LpIntent::Deploy { .. } | LpIntent::Recenter { .. }) {
            inventory_hedge_target(&c.strategy, &self.portfolio, p)
        } else {
            d.target_short_base
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|o| (o.target - target).abs() * hedge >= c.strategy.hedge_deadband_usd)
        {
            self.pending = None;
        }
        let diff = target - self.portfolio.short_base;
        let entry_band =
            widths.is_some() && matches!(d.lp, LpIntent::Deploy { .. } | LpIntent::Recenter { .. });
        let minimum = if entry_band {
            10.
        } else {
            c.strategy.hedge_deadband_usd
        };
        if diff.abs() * hedge >= minimum && self.pending.is_none() {
            let buy = diff < 0.0;
            self.pending = Some(PaperOrder {
                target,
                buy,
                price: hedge * (if buy { 0.9999 } else { 1.0001 }),
                submitted: now,
                emergency: d.emergency
                    || entry_band
                    || self.portfolio.positions.iter().any(|pos| p < pos.lower),
            });
        }
        Ok(())
    }
    pub fn apply_funding(&mut self, rows: &Value) -> Result<()> {
        for r in rows.as_array().context("funding history")? {
            let time = r["time"].as_u64().context("funding timestamp")?;
            if time > self.last_funding_ms {
                let pnl =
                    self.portfolio.short_base * self.last_hedge_price * num(&r["fundingRate"])?;
                self.portfolio.hedge_equity += pnl;
                self.funding_pnl += pnl;
                self.last_funding_ms = time;
            }
        }
        Ok(())
    }
}

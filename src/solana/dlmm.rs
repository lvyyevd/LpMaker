//! DLMM 专用价格/bin 账本。没有 Uniswap sqrtPrice、tick liquidity 或 NFT 运算。
use crate::{
    config::StrategyConfig,
    domain::{Decision, LpIntent, LpPosition, PoolSnapshot, Portfolio},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Position {
    pub address: String,
    pub revision: String,
    pub lower_bin: i32,
    pub upper_bin: i32,
    pub lower: f64,
    pub upper: f64,
    pub base: f64,
    pub quote: f64,
    pub fee_base: f64,
    pub fee_quote: f64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Wallet {
    pub base: f64,
    pub quote: f64,
    pub native: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub slot: u64,
    pub block_hash: String,
    pub time_ms: u64,
    pub observed_ms: u64,
    pub active_bin: i32,
    pub bin_step: u16,
    pub price: f64,
    pub positions: Vec<Position>,
    pub wallet: Wallet,
    pub grpc: serde_json::Value,
    pub fee_pct: f64,
}
impl Snapshot {
    pub fn validate(&self, now: u64, max_age: u64, require_grpc: bool) -> Result<()> {
        ensure!(
            self.bin_step == 4
                && self.price.is_finite()
                && self.price > 0.
                && (self.price / bin_price(self.active_bin) - 1.).abs() < 1e-8,
            "invalid DLMM price/bin snapshot"
        );
        crate::runtime::fresh(self.time_ms, now, max_age)?;
        ensure!(
            self.positions
                .iter()
                .all(|p| [p.base, p.quote, p.fee_base, p.fee_quote]
                    .iter()
                    .all(|x| x.is_finite() && *x >= 0.)),
            "invalid DLMM balances"
        );
        if require_grpc {
            ensure!(
                self.grpc["connected"] == true,
                "gRPC not connected; no transactions"
            );
            crate::runtime::fresh(
                self.grpc["last_data_ms"].as_u64().unwrap_or(0),
                now,
                max_age,
            )?;
        }
        Ok(())
    }
    pub fn position(&self, p: &Position, layer: &str) -> LpPosition {
        LpPosition {
            layer: layer.into(),
            token_id: Some(p.address.clone()),
            lower: p.lower,
            upper: p.upper,
            liquidity: 0.,
            raw_liquidity: String::new(),
            unclaimed_base: p.fee_base,
            unclaimed_quote: p.fee_quote,
            base: p.base + p.fee_base,
            quote: p.quote + p.fee_quote,
        }
    }
}
pub fn bin_price(bin: i32) -> f64 {
    1.0004_f64.powi(bin) * 1000.
}
pub fn price_bin(price: f64) -> i32 {
    (price / 1000.).ln().div_euclid(1.0004_f64.ln()) as i32
}
pub fn bounds(price: f64, width: f64) -> (i32, i32) {
    let active = price_bin(price);
    let n = (width.ln_1p() / 1.0004_f64.ln()).ceil() as i32;
    (active - n, active + n)
}
/// Legacy strategy frame has V3 metadata fields, but the pure strategy uses only price/time.
/// Empty V3 fields make accidental protocol execution fail; never pass this to an EVM adapter.
pub fn strategy_snapshot(price: f64, time_ms: u64) -> PoolSnapshot {
    PoolSnapshot {
        block: 0,
        block_hash: String::new(),
        time_ms,
        price,
        tick: 0,
        tick_spacing: 0,
        liquidity: String::new(),
        sqrt_price_x96: String::new(),
        base_is_token0: true,
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bin {
    pub id: i32,
    pub base: f64,
    pub quote: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperPosition {
    pub layer: String,
    pub since_ms: u64,
    pub bins: Vec<Bin>,
}
impl PaperPosition {
    pub fn new(layer: &str, value: f64, width: f64, price: f64, now: u64) -> Self {
        let (lo, hi) = bounds(price, width);
        // 研究模型：每 bin 等值分配，active bin 初始按全 quote；SDK 实盘使用 Spot 并读取真实 active-bin 配比。
        // 模型不宣称逐 bin 成交顺序或 LP 手续费精确复现。
        let per_bin = value / (hi - lo + 1) as f64;
        let bins = (lo..=hi)
            .map(|id| {
                if bin_price(id) > price {
                    Bin {
                        id,
                        base: per_bin / price,
                        quote: 0.,
                    }
                } else {
                    Bin {
                        id,
                        base: 0.,
                        quote: per_bin,
                    }
                }
            })
            .collect();
        Self {
            layer: layer.into(),
            since_ms: now,
            bins,
        }
    }
    pub fn mark(&mut self, price: f64) {
        for b in &mut self.bins {
            let p = bin_price(b.id);
            if price > p {
                b.quote += b.base * p;
                b.base = 0.;
            } else if price < p {
                b.base += b.quote / p;
                b.quote = 0.;
            }
        }
    }
    pub fn position(&self) -> LpPosition {
        LpPosition {
            layer: self.layer.clone(),
            token_id: None,
            lower: bin_price(self.bins.first().unwrap().id),
            upper: bin_price(self.bins.last().unwrap().id),
            liquidity: 0.,
            raw_liquidity: String::new(),
            unclaimed_base: 0.,
            unclaimed_quote: 0.,
            base: self.bins.iter().map(|b| b.base).sum(),
            quote: self.bins.iter().map(|b| b.quote).sum(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paper {
    #[serde(default = "default_leverage")]
    pub leverage: u32,
    pub positions: Vec<PaperPosition>,
    pub cash: f64,
    pub hedge_equity: f64,
    pub short: f64,
    pub last_hedge: f64,
    pub funding_pnl: f64,
    pub hedge_fees: f64,
    pub swap_costs: f64,
    pub operation_costs: f64,
    pub operations: u64,
    pub last_funding_ms: u64,
}
fn default_leverage() -> u32 {
    3
}
impl Paper {
    pub fn new(s: &StrategyConfig) -> Self {
        Self {
            leverage: 3,
            positions: vec![],
            cash: s.lp_budget,
            hedge_equity: s.hedge_collateral,
            short: 0.,
            last_hedge: 0.,
            funding_pnl: 0.,
            hedge_fees: 0.,
            swap_costs: 0.,
            operation_costs: 0.,
            operations: 0,
            last_funding_ms: 0,
        }
    }
    pub fn mark(&mut self, price: f64, hedge: f64) {
        if self.last_hedge > 0. {
            self.hedge_equity += self.short * (self.last_hedge - hedge);
        }
        self.last_hedge = hedge;
        for p in &mut self.positions {
            p.mark(price);
        }
    }
    pub fn portfolio(&self, s: &StrategyConfig) -> Portfolio {
        Portfolio {
            positions: self.positions.iter().map(|p| p.position()).collect(),
            wallet_base: 0.,
            wallet_quote: self.cash,
            short_base: self.short,
            hedge_equity: self.hedge_equity,
            reserve: s.reserve,
        }
    }
    pub fn funding(&mut self, rate: f64, price: f64, time: u64) {
        if time > self.last_funding_ms {
            let v = self.short * price * rate;
            self.hedge_equity += v;
            self.funding_pnl += v;
            self.last_funding_ms = time;
        }
    }
    pub fn hedge(
        &mut self,
        target: f64,
        price: f64,
        emergency: bool,
        s: &StrategyConfig,
        fee_rate: f64,
    ) {
        let delta = target - self.short;
        let value = delta.abs() * price;
        if value
            < if emergency || target == 0. {
                0.01
            } else {
                s.hedge_deadband_usd
            }
        {
            return;
        }
        if delta > 0.
            && (value < 10. || target * price / f64::from(self.leverage) > s.hedge_collateral * 0.9)
        {
            return;
        }
        let rounded = (target * 100.0).floor() / 100.0; // SOL 当前 szDecimals=2；仅研究假设，实盘读 exchange meta。
        let fee = (rounded - self.short).abs() * price * fee_rate;
        self.hedge_equity -= fee;
        self.hedge_fees += fee;
        self.short = rounded;
    }
    #[allow(clippy::too_many_arguments)] // Explicit simulation inputs; no shared live execution state.
    pub fn apply(
        &mut self,
        d: &Decision,
        s: &StrategyConfig,
        price: f64,
        hedge: f64,
        now: u64,
        fraction: f64,
        cost: f64,
        fee_rate: f64,
    ) -> Result<()> {
        ensure!(
            [price, hedge].iter().all(|x| x.is_finite() && *x > 0.),
            "invalid simulation price"
        );
        if d.lp == LpIntent::Hold {
            self.hedge(d.target_short_base, hedge, d.emergency, s, fee_rate);
            return Ok(());
        }
        self.hedge(self.portfolio(s).base(), hedge, true, s, fee_rate);
        if d.lp == LpIntent::ExitToQuote && self.positions.is_empty() {
            self.hedge(0., hedge, true, s, fee_rate);
            return Ok(());
        }
        let names: Vec<String> = match &d.lp {
            LpIntent::Recenter { layers } => layers.clone(),
            _ => s.layers.iter().map(|l| l.name.clone()).collect(),
        };
        // Scaling rebuilds only in this research ledger, so charge the operation cost; live increases in-place.
        for p in self.positions.iter().filter(|p| names.contains(&p.layer)) {
            let p = p.position();
            let swap = p.base * price * 0.0034;
            self.swap_costs += swap;
            self.cash += p.base * price + p.quote - swap;
        }
        self.positions.retain(|p| !names.contains(&p.layer));
        self.cash -= cost;
        self.operation_costs += cost;
        self.operations += 1;
        if d.lp == LpIntent::ExitToQuote {
            self.hedge(0., hedge, true, s, fee_rate);
            return Ok(());
        }
        let deploy = match d.lp {
            LpIntent::Deploy { fraction } => fraction,
            _ => fraction,
        };
        for layer in s.layers.iter().filter(|l| names.contains(&l.name)) {
            let value = (s.lp_budget * deploy * layer.weight).min(self.cash.max(0.)) * 0.995;
            if value < 1. {
                continue;
            }
            let swap = value * 0.5 * 0.0034;
            self.cash -= value;
            self.swap_costs += swap;
            self.positions.push(PaperPosition::new(
                &layer.name,
                value - swap,
                layer.half_width,
                price,
                now,
            ));
        }
        let p = self.portfolio(s);
        self.hedge(
            crate::engine::inventory_hedge_target(s, &p, price),
            hedge,
            true,
            s,
            fee_rate,
        );
        Ok(())
    }
}

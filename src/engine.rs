use crate::{
    config::{Config, Mode},
    domain::*,
    evm::{UniswapV3, tx::Executor},
    hyperliquid::{Client, account::collateral, journal, num, orders, position_size},
    math, recovery,
    store::Store,
    strategy::Strategy,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperOrder {
    pub target: f64,
    pub buy: bool,
    pub price: f64,
    pub submitted: u64,
    pub emergency: bool,
}

pub fn inventory_hedge_target(c: &crate::config::StrategyConfig, p: &Portfolio, price: f64) -> f64 {
    p.wallet_base
        + p.positions
            .iter()
            .map(|pos| {
                pos.base
                    * if price <= pos.lower * (1.0 + c.hedge_release_buffer) {
                        1.0
                    } else {
                        c.inside_hedge_ratio
                    }
            })
            .sum::<f64>()
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
                    _ => available,
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
                    let (lo, hi) = math::range(p, l.half_width);
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
        if diff.abs() * hedge >= c.strategy.hedge_deadband_usd && self.pending.is_none() {
            let buy = diff < 0.0;
            self.pending = Some(PaperOrder {
                target,
                buy,
                price: hedge * (if buy { 0.9999 } else { 1.0001 }),
                submitted: now,
                emergency: d.emergency || self.portfolio.positions.iter().any(|pos| p < pos.lower),
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

pub struct Live {
    pub liquidity: Arc<dyn LiquidityExecutor>,
    pub store: Arc<Store>,
    pub hl: Client,
    pub cfg: Config,
}
#[derive(Debug)]
struct HedgeBudgetUnavailable(&'static str);
impl std::fmt::Display for HedgeBudgetUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for HedgeBudgetUnavailable {}
impl Live {
    async fn entry_guard(&self) -> Result<()> {
        crate::runtime::observe(
            self.cfg.runtime.observation_timeout_seconds,
            self.entry_guard_inner(),
        )
        .await
    }
    async fn entry_guard_inner(&self) -> Result<()> {
        let started = crate::now_ms();
        let s = self.liquidity.snapshot().await?;
        let (p, t) = self.hl.market(&self.cfg.hyperliquid.hedge_coin).await?;
        ensure!(
            (s.price / p - 1.0).abs() * 10000.0 <= self.cfg.strategy.max_basis_bps,
            "entry basis limit"
        );
        let c = &self.cfg.strategy;
        let count = (c.vol_long_hours + c.vol_short_hours + 2).max(c.ema_slow_hours * 3 + 2);
        let bars = self
            .hl
            .candles(
                &self.cfg.hyperliquid.hedge_coin,
                "1h",
                started.saturating_sub(count as u64 * 3_600_000),
                started,
            )
            .await?;
        let now = crate::now_ms();
        crate::runtime::fresh(started, now, self.cfg.runtime.observation_timeout_seconds)?;
        crate::runtime::fresh(s.time_ms, now, c.max_data_age_seconds)?;
        crate::runtime::fresh(t, now, c.max_data_age_seconds)?;
        let m = crate::strategy::indicators::calculate(&bars, c, now)?;
        ensure!(
            !m.downtrend
                && m.vol_ratio < c.vol_pause_ratio
                && m.return_1h >= -c.fast_drop_1h
                && s.price / m.last_close - 1.0 >= -c.fast_drop_1h,
            "market became unsafe during LP workflow; hold hedge and reconcile workflow"
        );
        Ok(())
    }
    async fn scale_positions(&self, positions: &[LpPosition], fraction: f64) -> Result<()> {
        for pos in positions {
            self.entry_guard().await?;
            let s = self.liquidity.snapshot().await?;
            let layer = self
                .cfg
                .strategy
                .layers
                .iter()
                .find(|l| l.name == pos.layer)
                .context("unknown layer")?;
            let (wallet_b, wallet_q) = self.liquidity.wallet_balances().await?;
            let desired = self.cfg.strategy.lp_budget * fraction * layer.weight
                - (pos.base * s.price + pos.quote);
            let value = desired.min(wallet_b * s.price + wallet_q) * 0.995;
            if value < 10.0 {
                continue;
            }
            ensure!(
                s.price > pos.lower && s.price < pos.upper,
                "cannot scale an out-of-range layer"
            );
            let l = math::liquidity_for_value(value, pos.lower, pos.upper, s.price)?;
            let (needed_b, _) = math::amounts(l, pos.lower, pos.upper, s.price);
            let delta = needed_b - wallet_b;
            if delta.abs() * s.price > 1.0 {
                self.liquidity
                    .swap_inventory(
                        delta < 0.0,
                        if delta < 0.0 { -delta } else { delta * s.price },
                    )
                    .await?;
            }
            self.hedge(self.portfolio().await?.base(), true).await?;
            self.entry_guard().await?;
            self.liquidity.increase_position(pos, value * 0.998).await?;
        }
        Ok(())
    }
    pub async fn portfolio(&self) -> Result<Portfolio> {
        let positions = self.liquidity.current_positions().await?;
        if !positions.is_empty() {
            recovery::record_lp_history(&self.store, json!({"source":"reconciled_inventory"}))?;
        }
        let (base, quote) = self.liquidity.wallet_balances().await?;
        let account = self.hl.account().await?;
        self.store.write(
            "account_snapshot.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"account":account}),
        )?;
        let signed = position_size(&account, &self.cfg.hyperliquid.hedge_coin)?;
        ensure!(
            signed <= 0.0,
            "unexpected long perpetual position; use a dedicated hedge account"
        );
        for ap in account["assetPositions"]
            .as_array()
            .context("asset positions")?
        {
            ensure!(
                ap["position"]["coin"] == self.cfg.hyperliquid.hedge_coin
                    || num(&ap["position"]["szi"])? == 0.0,
                "other live positions in hedge account are not part of this strategy"
            );
        }
        let portfolio = Portfolio {
            positions,
            wallet_base: base,
            wallet_quote: quote,
            short_base: -signed,
            hedge_equity: collateral(&account)?.equity_usdc,
            reserve: self.cfg.strategy.reserve,
        };
        let previous = self.store.read::<Value>("live_inventory.json")?;
        let observation = json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,
            "account":account,"portfolio":portfolio});
        if previous
            .as_ref()
            .is_some_and(|old| old["portfolio"] != observation["portfolio"])
        {
            self.store.event(
                "inventory_corrected",
                json!({"previous":previous,"actual":observation}),
            )?;
        }
        self.store.write("live_inventory.json", &observation)?;
        Ok(portfolio)
    }
    pub async fn sync_orders(&self, recover_managed: bool) -> Result<()> {
        // Persist raw observations even when an unknown/manual order blocks recovery.
        let open = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":open}),
        )?;
        journal::refresh(&self.hl, &self.store).await?;
        let open = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":open}),
        )?;
        let ids =
            journal::managed_open_orders(&self.store, &open, &self.cfg.hyperliquid.hedge_coin)?;
        ensure!(
            recover_managed || ids.is_empty(),
            "unexpected outstanding strategy order; restart reconciliation required"
        );
        for id in ids {
            self.store
                .event("startup_cancel_stale_hedge", json!({"cloid":id}))?;
            self.cancel_if_open(&self.cfg.hyperliquid.hedge_coin, &id)
                .await?;
        }
        let after = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":after}),
        )?;
        ensure!(
            after.as_array().is_some_and(|a| a.is_empty()),
            "open orders remain after reconciliation"
        );
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        journal::compact(&self.store)?;
        Ok(())
    }
    pub async fn hedge(&self, target: f64, emergency: bool) -> Result<()> {
        ensure!(target.is_finite() && target >= 0.0, "invalid hedge target");
        let coin = &self.cfg.hyperliquid.hedge_coin;
        let account = self.hl.account().await?;
        let signed = position_size(&account, coin)?;
        ensure!(signed <= 0.0, "unexpected long position");
        let current = -signed;
        let (bid, ask, time) = self.hl.book(coin).await?;
        let mid = (bid + ask) / 2.0;
        let target = if target * mid < 0.01 { 0.0 } else { target };
        let delta = target - current;
        crate::runtime::fresh(
            time,
            crate::now_ms(),
            self.cfg.strategy.max_data_age_seconds,
        )?;
        if delta.abs() * mid
            < if target == 0.0 || emergency {
                0.01
            } else {
                self.cfg.strategy.hedge_deadband_usd
            }
        {
            return self.hedge_residual(target, current, mid, "within_hedge_deadband");
        }
        if delta > 0.0 {
            if target * mid / self.cfg.hyperliquid.leverage as f64
                > self.cfg.strategy.hedge_collateral * 0.9
            {
                return Err(HedgeBudgetUnavailable("hedge margin budget exceeded").into());
            }
            if delta * mid / self.cfg.hyperliquid.leverage as f64
                > collateral(&account)?.available_short_usdc
            {
                return Err(HedgeBudgetUnavailable("insufficient free hedge collateral").into());
            }
        }
        let (_, asset) = self.hl.asset(coin).await?;
        let size = orders::quantity(delta.abs(), asset.sz_decimals)?;
        let buy = delta < 0.0;
        let cloid = orders::cloid();
        let px = orders::price(if buy { bid } else { ask }, asset.sz_decimals, !buy)?;
        if !orders::tradeable(&px, &size, buy)? {
            return self.hedge_residual(
                target,
                current,
                mid,
                "below_opening_minimum_or_lot_precision",
            );
        }
        let result = self
            .hl
            .managed_order(
                coin,
                buy,
                &px,
                &size,
                "Alo",
                &cloid,
                target,
                time,
                self.cfg.strategy.max_data_age_seconds,
            )
            .await;
        if let Err(e) = result {
            if self.store.pending()?.is_some() || !e.is::<crate::hyperliquid::ExchangeRejected>() {
                return Err(e);
            }
            self.store.event("maker_rejected", format!("{e:#}"))?;
        } else {
            tokio::time::sleep(Duration::from_secs(if emergency {
                self.cfg.hyperliquid.maker_wait_seconds.min(2)
            } else {
                self.cfg.hyperliquid.maker_wait_seconds
            }))
            .await;
            self.cancel_if_open(coin, &cloid).await?;
        }
        // The original order is now confirmed rejected, canceled or filled.
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        // Read exchange inventory after cancel acknowledgement; cancellation may race a fill.
        let current = -position_size(&self.hl.perp_account().await?, coin)?;
        let mut final_current = current;
        let remaining = target - current;
        if emergency && remaining.abs() * mid >= 0.01 {
            let (bid, ask, t) = self.hl.book(coin).await?;
            crate::runtime::fresh(t, crate::now_ms(), self.cfg.strategy.max_data_age_seconds)?;
            let buy = remaining < 0.0;
            let slip = self.cfg.hyperliquid.emergency_slippage_bps as f64 / 10000.0;
            let px = orders::price(
                if buy {
                    ask * (1.0 + slip)
                } else {
                    bid * (1.0 - slip)
                },
                asset.sz_decimals,
                buy,
            )?;
            let sz = orders::quantity(remaining.abs(), asset.sz_decimals)?;
            if !orders::tradeable(&px, &sz, buy)? {
                return self.hedge_residual(
                    target,
                    current,
                    (bid + ask) / 2.0,
                    "partial_fill_dust_deferred",
                );
            }
            if sz != "0" {
                let id = orders::cloid();
                self.hl
                    .managed_order(
                        coin,
                        buy,
                        &px,
                        &sz,
                        "Ioc",
                        &id,
                        target,
                        t,
                        self.cfg.strategy.max_data_age_seconds,
                    )
                    .await?;
            }
            let actual = -position_size(&self.hl.perp_account().await?, coin)?;
            final_current = actual;
            ensure!(
                (target - actual).abs() * mid < self.cfg.strategy.hedge_deadband_usd.min(10.0),
                "emergency IOC only partially filled; inventory reconciliation required"
            );
        }
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        self.hedge_residual(target, final_current, mid, "confirmed_execution_residual")
    }
    fn hedge_residual(&self, target: f64, actual: f64, price: f64, reason: &str) -> Result<()> {
        if (target - actual).abs() * price < 0.01 {
            return self
                .store
                .write("hedge_residual.json", &Option::<Value>::None);
        }
        let row = json!({"observed_ms":crate::now_ms(),"coin":self.cfg.hyperliquid.hedge_coin,
            "target":target,"actual_short":actual,"residual_base":target-actual,
            "residual_usd":(target-actual).abs()*price,"reason":reason,
            "action":"recompute from actual inventory next cycle; do not increase size to meet minimum"});
        self.store.write("hedge_residual.json", &row)?;
        self.store.event("hedge_residual_deferred", &row)?;
        tracing::warn!(residual=%row, "hedge dust deferred; exposure remains visible");
        Ok(())
    }
    pub async fn cancel_if_open(&self, coin: &str, cloid: &str) -> Result<()> {
        let status = self.hl.order_status(json!(cloid)).await?;
        journal::observe(&self.store, cloid, &status)?;
        if status["status"] == "order" && status["order"]["status"] == "open" {
            let canceled = self.hl.cancel(coin, cloid).await;
            if let Err(error) = canceled
                && (self.store.pending()?.is_some()
                    || !error.is::<crate::hyperliquid::ExchangeRejected>())
            {
                return Err(error);
            }
            // An acknowledged rejection can race a fill: the subsequent status is authoritative.
            let after = self.hl.order_status(json!(cloid)).await?;
            journal::observe(&self.store, cloid, &after)?;
            ensure!(
                after["order"]["status"]
                    .as_str()
                    .is_some_and(journal::terminal_status),
                "cancel/fill not confirmed"
            );
        }
        Ok(())
    }
    /// Before buying any base inventory, reserve capacity to hedge the whole new LP budget.
    /// This is a preflight only: execution still rechecks margin before each hedge order.
    pub(crate) async fn preflight_entry(&self, d: &Decision, portfolio: &Portfolio) -> Result<()> {
        let LpIntent::Deploy { fraction } = d.lp else {
            return Ok(());
        };
        if !portfolio.positions.is_empty() {
            return Ok(());
        }
        ensure!(
            fraction.is_finite() && fraction > 0.0 && fraction <= 1.0,
            "invalid deployment fraction"
        );
        let required =
            self.cfg.strategy.lp_budget * fraction / f64::from(self.cfg.hyperliquid.leverage) / 0.9;
        let account = self.hl.account().await?;
        let funds = collateral(&account)?;
        let equity = funds.equity_usdc;
        let available = funds.available_short_usdc;
        ensure!(
            equity.is_finite() && available.is_finite(),
            "invalid hedge collateral observation"
        );
        if required > self.cfg.strategy.hedge_collateral
            || required > equity
            || required > available
        {
            tracing::warn!(
                account_mode=%funds.account_mode,
                collateral_source=%funds.source,
                required_usdc = required,
                available_usdc = available,
                equity_usdc = equity,
                configured_usdc = self.cfg.strategy.hedge_collateral,
                "LP entry waiting for hedge collateral; no inventory purchased"
            );
            return Err(HedgeBudgetUnavailable("entry_waiting_hedge_collateral").into());
        }
        Ok(())
    }
    pub async fn apply(&self, d: &Decision) -> Result<()> {
        if d.lp == LpIntent::Hold {
            return self
                .hedge(
                    d.target_short_base,
                    d.emergency
                        || d.target_short_base > 0.0
                            && d.target_short_base >= self.portfolio().await?.base() * 0.99,
                )
                .await;
        }
        let portfolio = self.portfolio().await?;
        self.preflight_entry(d, &portfolio).await?;
        self.store.write(
            "workflow.json",
            &Some(json!({"decision":d,"started_ms":crate::now_ms()})),
        )?;
        // Inventory is temporarily fully hedged during a multi-venue LP transition.
        if let Err(error) = self.hedge(portfolio.base(), true).await {
            if d.lp != LpIntent::ExitToQuote
                || self.store.pending()?.is_some()
                || !(error.is::<crate::hyperliquid::ExchangeRejected>()
                    || error.is::<HedgeBudgetUnavailable>())
            {
                return Err(error);
            }
            // An acknowledged insufficient-margin/rejected hedge must not prevent selling risk.
            self.store
                .event("exit_without_full_hedge", format!("{error:#}"))?;
        }
        if let LpIntent::Deploy { fraction } = d.lp
            && !portfolio.positions.is_empty()
        {
            self.scale_positions(&portfolio.positions, fraction).await?;
            let after = self.portfolio().await?;
            let price = self.liquidity.snapshot().await?.price;
            self.hedge(
                inventory_hedge_target(&self.cfg.strategy, &after, price),
                true,
            )
            .await?;
            self.store.write("workflow.json", &Option::<Value>::None)?;
            return Ok(());
        }
        let layers = match &d.lp {
            LpIntent::Recenter { layers } => layers.clone(),
            _ => portfolio
                .positions
                .iter()
                .map(|p| p.layer.clone())
                .collect(),
        };
        for p in portfolio
            .positions
            .iter()
            .filter(|p| layers.contains(&p.layer))
        {
            self.liquidity.remove_position(p).await?;
        }
        if d.lp == LpIntent::ExitToQuote {
            let (base, _) = self.liquidity.wallet_balances().await?;
            if base > 1e-8 {
                self.liquidity.swap_inventory(true, base).await?;
            }
            // Sale first, then reduce-only short close. Never silently treat cross-chain legs as atomic.
            let remaining = self.liquidity.wallet_balances().await?.0;
            self.hedge(remaining, true).await?;
        } else {
            let snap = self.liquidity.snapshot().await?;
            let (base, quote) = self.liquidity.wallet_balances().await?;
            let available = base * snap.price + quote;
            let names = match &d.lp {
                LpIntent::Recenter { layers } => layers.clone(),
                _ => self
                    .cfg
                    .strategy
                    .layers
                    .iter()
                    .map(|l| l.name.clone())
                    .collect(),
            };
            let fraction = match d.lp {
                LpIntent::Deploy { fraction } => fraction,
                _ => 1.0,
            };
            let total_weight = self
                .cfg
                .strategy
                .layers
                .iter()
                .filter(|l| names.contains(&l.name))
                .map(|l| l.weight)
                .sum::<f64>();
            let budget =
                (self.cfg.strategy.lp_budget * fraction * total_weight).min(available) * 0.995;
            for l in self
                .cfg
                .strategy
                .layers
                .iter()
                .filter(|l| names.contains(&l.name))
            {
                self.entry_guard().await?;
                let s = self.liquidity.snapshot().await?;
                let value = budget * l.weight / total_weight;
                // Geometric ranges require equal base/quote value at their centre.
                let target_base = value / 2.0 / s.price;
                let (current, _) = self.liquidity.wallet_balances().await?;
                let delta = target_base - current;
                if delta.abs() * s.price > 1.0 {
                    self.liquidity
                        .swap_inventory(
                            delta < 0.0,
                            if delta < 0.0 { -delta } else { delta * s.price },
                        )
                        .await?;
                }
                let inventory = self.portfolio().await?;
                self.hedge(inventory.base(), true).await?;
                self.entry_guard().await?;
                // 0.2% cushion covers tick rounding and small price movement; excess stays in the wallet.
                self.liquidity
                    .mint_layer(&l.name, value * 0.998, l.half_width)
                    .await?;
            }
            let inventory = self.portfolio().await?;
            let price = self.liquidity.snapshot().await?.price;
            let target = inventory_hedge_target(&self.cfg.strategy, &inventory, price);
            self.hedge(target, true).await?;
        }
        self.store.write("workflow.json", &Option::<Value>::None)?;
        Ok(())
    }
}

pub async fn run(
    c: Config,
    store: Arc<Store>,
    once: bool,
    execute: bool,
    first_entry: bool,
) -> Result<()> {
    recovery::start(&store)?;
    crate::runtime::status(&store, "starting", json!({"pid":std::process::id()}))?;
    let mut supervisor = crate::monitor::Supervisor::start(c.clone(), store.clone());
    let task = supervisor.task.as_mut().context("monitor task missing")?;
    let result = tokio::select! {
        result = crate::runtime::retry_reads(&store, Duration::from_secs(c.runtime.read_retry_seconds), once, || async {
            recovery::start(&store)?;
            crate::runtime::status(&store, "recovering", json!({"action":"reconcile persisted state"}))?;
            run_strategy(c.clone(), store.clone(), once, execute, first_entry).await
        }) => result,
        result = task => {
            supervisor.task.take();
            match result { Ok(Err(e)) => Err(e.context("monitor failed; strategy stopped")),
                _ => Err(anyhow::anyhow!("monitor unexpectedly stopped; strategy stopped")) }
        },
        _ = crate::runtime::shutdown() => {
            store.event("shutdown", "positions retained; any prepared transaction remains pending for reconciliation")?;
            Ok(())
        }
    };
    let stopped = supervisor.stop().await;
    if let Err(error) = &result {
        recovery::stage(&store, "blocked", json!({"error":format!("{error:#}")}))?;
    }
    crate::runtime::status(
        &store,
        if result.is_ok() { "stopped" } else { "blocked" },
        json!({"error":result.as_ref().err().map(|e| format!("{e:#}"))}),
    )?;
    result.and(stopped)
}
async fn run_strategy(
    c: Config,
    store: Arc<Store>,
    once: bool,
    execute: bool,
    first_entry: bool,
) -> Result<()> {
    let fingerprint = serde_json::to_string(
        &json!({"mode":c.mode,"liquidity":c.liquidity,"strategy":c.strategy,"hyperliquid":c.hyperliquid}),
    )?;
    if let Some(old) = store.read::<String>("config.json")? {
        ensure!(
            transport_independent_fingerprint(&old)?
                == transport_independent_fingerprint(&fingerprint)?,
            "config changed for existing state; use a new state directory or explicitly migrate"
        )
    }
    store.write("config.json", &fingerprint)?;
    let venue = UniswapV3::new(c.liquidity.clone())?;
    venue.validate().await?;
    let mut hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let (mut strategy, mut paper) = recovery::load(&store, &c)?;
    recovery::invalidate_observation_streaks(&mut strategy);
    recovery::stage(
        &store,
        "checkpoint_loaded",
        json!({"phase":strategy.phase,"mode":c.mode,
        "note":"observation streaks reset across process downtime; balances and risk phase retained"}),
    )?;
    let live = if c.mode == Mode::Live {
        ensure!(execute, "live runner requires --execute");
        if store.pending()?.is_some() {
            recovery::stage(
                &store,
                "pending_operation",
                json!({"action":"query original hash/client ID; never resend"}),
            )?;
            reconcile(&c, store.clone()).await?;
        }
        hl.enable_signing().await?;
        let evm = Executor::new(venue.clone(), store.clone())?;
        evm.nonce.refresh().await?.available()?;
        evm.verify_inventory().await?;
        recovery::stage(&store, "lp_inventory_verified", json!({"nfts":evm.ids()?}))?;
        let l = Live {
            liquidity: Arc::new(evm),
            store: store.clone(),
            hl: hl.clone(),
            cfg: c.clone(),
        };
        l.portfolio().await?;
        l.sync_orders(true).await?;
        let actual = l.portfolio().await?; // Include fills racing recovery cancellation.
        strategy.observe_lp(!actual.positions.is_empty());
        recovery::stage(&store, "exchange_reconciled", json!({"portfolio":actual}))?;
        let layers = actual
            .positions
            .iter()
            .map(|p| p.layer.clone())
            .collect::<Vec<_>>();
        if recovery::pause_incomplete_inventory(&mut strategy, &layers, &c) {
            recovery::stage(
                &store,
                "phase_corrected",
                json!({"phase":"Paused","reason":"saved phase expects missing LP layers; no blind redeploy"}),
            )?;
        }
        recovery::save(&store, &c, &strategy, &paper)?;
        let asset_state = hl.active_asset(&c.hyperliquid.hedge_coin).await?;
        if asset_state["leverage"]["value"] != c.hyperliquid.leverage
            || asset_state["leverage"]["type"]
                != if c.hyperliquid.cross_margin {
                    "cross"
                } else {
                    "isolated"
                }
        {
            hl.leverage(
                &c.hyperliquid.hedge_coin,
                c.hyperliquid.leverage,
                c.hyperliquid.cross_margin,
            )
            .await?;
        }
        if store.read::<Value>("workflow.json")?.is_some() {
            store.event(
                "recovery",
                "unfinished LP workflow: flatten reconciled inventory, do not replay old mint",
            )?;
            l.apply(&Decision {
                state: "Paused".into(),
                reasons: vec!["restart_recovery".into()],
                lp: LpIntent::ExitToQuote,
                target_short_base: 0.0,
                emergency: true,
            })
            .await?;
            recovery::finish_workflow_recovery(&mut strategy);
            recovery::save(&store, &c, &strategy, &paper)?;
        }
        if first_entry {
            let actual = l.portfolio().await?;
            let confirmed = recovery::confirm_first_entry(&store, &mut strategy, &actual)?;
            tracing::info!(confirmed, entry_history=?strategy.entry_history, "first-entry migration checked after account reconciliation");
        }
        Some(l)
    } else {
        ensure!(
            store.pending()?.is_none(),
            "paper state has a live unresolved operation; refusing to ignore it"
        );
        strategy.observe_lp(!paper.portfolio.positions.is_empty());
        if first_entry {
            ensure!(
                paper.pending.is_none(),
                "first entry requires no pending paper hedge order"
            );
            recovery::confirm_first_entry(&store, &mut strategy, &paper.portfolio)?;
        }
        let layers = paper
            .portfolio
            .positions
            .iter()
            .map(|p| p.layer.clone())
            .collect::<Vec<_>>();
        if recovery::pause_incomplete_inventory(&mut strategy, &layers, &c) {
            recovery::stage(
                &store,
                "phase_corrected",
                json!({"phase":"Paused","reason":"incomplete legacy paper LP state"}),
            )?;
        }
        None
    };
    recovery::save(&store, &c, &strategy, &paper)?;
    recovery::stage(
        &store,
        "ready",
        json!({"phase":strategy.phase,"mode":c.mode,"entry_history":strategy.entry_history}),
    )?;
    let mut history = vec![];
    let mut fetched_hour = 0;
    let _nonce_worker = live.as_ref().map(|l| {
        let liquidity=l.liquidity.clone();let seconds=c.liquidity.nonce_refresh_seconds;
        NonceWorker(tokio::spawn(async move {
            let mut timer=tokio::time::interval(Duration::from_secs(seconds));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                if let Err(e)=liquidity.refresh_execution_state().await {
                    tracing::warn!(error=%format!("{e:#}"),"periodic nonce refresh failed; every send still requires a fresh successful check");
                }
            }
        }))
    });
    loop {
        if let Some(l) = &live {
            l.sync_orders(false).await?;
        }
        let now = crate::now_ms();
        let hour = now / 3_600_000;
        let observed: Result<_> =
            crate::runtime::observe(c.runtime.observation_timeout_seconds, async {
                if history.is_empty() || hour != fetched_hour {
                    let count = (c.strategy.vol_long_hours + c.strategy.vol_short_hours + 2)
                        .max(c.strategy.ema_slow_hours * 3 + 2);
                    history = hl
                        .candles(
                            &c.hyperliquid.hedge_coin,
                            "1h",
                            now.saturating_sub(count as u64 * 3_600_000),
                            now,
                        )
                        .await?;
                    fetched_hour = hour;
                    if live.is_none() && paper.last_hedge_price > 0.0 {
                        let funding = hl
                            .funding_history(
                                &c.hyperliquid.hedge_coin,
                                paper.last_funding_ms + 1,
                                now,
                            )
                            .await?;
                        paper.apply_funding(&funding)?;
                    }
                }
                let pool = venue.snapshot().await?;
                let (hp, ht) = hl.market(&c.hyperliquid.hedge_coin).await?;
                let portfolio = if let Some(l) = &live {
                    l.portfolio().await?
                } else {
                    paper.mark(pool.price, hp, now, &c);
                    paper.portfolio.clone()
                };
                Ok((pool, hp, ht, portfolio))
            })
            .await;
        let (pool, hp, ht, portfolio) = observed?;
        let now = crate::now_ms();
        crate::runtime::fresh(pool.time_ms, now, c.strategy.max_data_age_seconds)?;
        crate::runtime::fresh(ht, now, c.strategy.max_data_age_seconds)?;
        let basis = if live.is_some() {
            let account = store
                .read::<Value>("account_snapshot.json")?
                .context("missing account observation")?;
            if collateral(&account["account"])?.account_mode == "unifiedAccount" {
                "unified_usdc_v1"
            } else {
                "native_perps_v1"
            }
        } else {
            "paper_v1"
        };
        let (baseline, baseline_comparable) =
            recovery::equity_baseline(&store, portfolio.equity(pool.price), basis)?;
        store.write(
            "portfolio.json",
            &crate::monitor::PortfolioObservation {
                observed_ms: now,
                mark_price: pool.price,
                portfolio: portfolio.clone(),
                baseline_equity: baseline,
                baseline_comparable: Some(baseline_comparable),
            },
        )?;
        let frame = MarketFrame {
            now_ms: now,
            pool,
            hedge_price: hp,
            hedge_time_ms: ht,
            candles: history.clone(),
            portfolio,
        };
        let previous = strategy.clone();
        let mut d = strategy.evaluate(&c.strategy, &frame);
        let mut entry_deferred = false;
        if let Some(l) = &live
            && let Err(error) = l.preflight_entry(&d, &frame.portfolio).await
        {
            if !error.is::<HedgeBudgetUnavailable>() {
                return Err(error);
            }
            defer_entry(&mut strategy, &previous, &mut d, &frame.portfolio);
            entry_deferred = d.lp == LpIntent::Hold;
        }
        store.event("decision",json!({"decision":d,"pool":frame.pool,"equity":frame.portfolio.equity(frame.pool.price),"net_base":frame.portfolio.base()-frame.portfolio.short_base}))?;
        tracing::info!(phase=%d.state,entry_history=?strategy.entry_history,action=?d.lp,price=frame.pool.price,equity=frame.portfolio.equity(frame.pool.price),reasons=?d.reasons,"strategy decision");
        if live.is_some() {
            recovery::save(&store, &c, &strategy, &paper)?;
        }
        if !entry_deferred
            && !d
                .reasons
                .iter()
                .any(|r| r.starts_with("stale_or_invalid_data"))
        {
            if let Some(l) = &live {
                l.apply(&d).await?;
                l.sync_orders(false).await?;
                strategy.observe_lp(!l.portfolio().await?.positions.is_empty());
            } else {
                paper.apply(&d, &c, frame.pool.price, hp, now)?;
                strategy.observe_lp(!paper.portfolio.positions.is_empty());
                recovery::save(&store, &c, &strategy, &paper)?;
                store.event("paper_execution", json!({"decision":d,"portfolio":paper.portfolio,"pending_hedge":paper.pending,"hedge_fees":paper.hedge_fees,"swap_costs":paper.swap_costs}))?;
            }
        }
        recovery::save(&store, &c, &strategy, &paper)?;
        crate::runtime::status(
            &store,
            "running",
            json!({"phase":strategy.phase,"pool_time_ms":frame.pool.time_ms,
            "hedge_time_ms":ht,"residual":store.read::<Value>("hedge_residual.json")?}),
        )?;
        if once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(c.poll_seconds)).await;
    }
}

/// An unexecuted deployment must not advance Active/Recovering or consume its entry allowance.
pub(crate) fn defer_entry(
    strategy: &mut Strategy,
    previous: &Strategy,
    d: &mut Decision,
    portfolio: &Portfolio,
) {
    strategy.phase = previous.phase.clone();
    strategy.fraction = previous.fraction;
    strategy.last_scale = previous.last_scale;
    d.lp = LpIntent::Hold;
    d.state = format!("{:?}", strategy.phase);
    d.target_short_base = portfolio.short_base;
    d.reasons.push(
        "entry_waiting_hedge_collateral: deployment deferred before any swap or LP transaction"
            .into(),
    );
    if portfolio.wallet_base.abs() > 1e-8 || portfolio.short_base.abs() > 1e-8 {
        // Insufficient collateral blocks entry, not disposal of pre-existing exposure.
        strategy.phase = crate::strategy::Phase::Paused;
        d.state = "Paused".into();
        d.lp = LpIntent::ExitToQuote;
        d.emergency = true;
    }
}

struct NonceWorker(tokio::task::JoinHandle<()>);
impl Drop for NonceWorker {
    fn drop(&mut self) {
        self.0.abort();
        tracing::info!("periodic nonce worker stopped");
    }
}
/// Transport, observability and fee-quote buffers do not reset positions or accounting.
/// Economic budgets (including the gas ceiling) and signer identity still require migration.
pub fn transport_independent_fingerprint(serialized: &str) -> Result<Value> {
    let mut value: Value = serde_json::from_str(serialized)?;
    for (section, keys) in [
        (
            "liquidity",
            vec![
                "rpc_url",
                "archive_rpc_url",
                "ws_url",
                "nonce_refresh_seconds",
                "pending_warn_seconds",
                "owner",
                "max_quote_age_seconds",
                "gas_fee_buffer_bps",
                "gas_limit_buffer_bps",
            ],
        ),
        ("hyperliquid", vec!["http_url", "ws_url"]),
    ] {
        if let Some(map) = value[section].as_object_mut() {
            for key in keys {
                map.remove(key);
            }
        }
    }
    Ok(value)
}

pub async fn reconcile(c: &Config, store: Arc<Store>) -> Result<Value> {
    let pending = match store.pending()? {
        Some(v) => v,
        None => return Ok(json!({"status":"no_pending_operation"})),
    };
    if pending["venue"] == "evm" {
        let evm = Executor::new(UniswapV3::new(c.liquidity.clone())?, store.clone())?;
        ensure!(
            pending["owner"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&evm.owner().to_string())),
            "pending operation belongs to another signer"
        );
        return evm
            .wait_receipt(
                pending["hash"].as_str().context("pending hash")?,
                &pending["operation"],
            )
            .await;
    }
    ensure!(
        pending["venue"] == "hyperliquid",
        "unknown pending venue; operation retained"
    );
    let hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    ensure!(
        pending["user"]
            .as_str()
            .is_some_and(|u| hl.user().is_ok_and(|actual| actual.eq_ignore_ascii_case(u))),
        "pending operation belongs to another Hyperliquid account"
    );
    let action = &pending["request"]["action"];
    if pending["dispatch_state"] == "prepared" {
        journal::prepare(
            &store,
            action,
            hl.user()?,
            pending["request"]["nonce"]
                .as_u64()
                .context("pending nonce")?,
        )?;
        journal::not_submitted(&store, action)?;
        let result = json!({"status":"not_submitted","reason":"durable pre-dispatch state; no network write began"});
        store.finish(&result)?;
        return Ok(result);
    }
    if action["type"] == "order" {
        journal::prepare(
            &store,
            action,
            hl.user()?,
            pending["request"]["nonce"]
                .as_u64()
                .context("pending request nonce")?,
        )?;
        let orders = action["orders"].as_array().context("pending orders")?;
        let mut statuses = vec![];
        for o in orders {
            let id = o["c"].as_str().context("pending order has no client ID")?;
            let status = hl.order_status(json!(id)).await?;
            journal::observe(&store, id, &status)?;
            statuses.push(status);
        }
        store.finish(&statuses)?;
        return Ok(
            json!({"statuses":statuses,"note":"known strategy orders and actual positions are reconciled at startup; no order replay"}),
        );
    }
    // Non-order actions are reconciled by their observable effect, never blindly resent.
    match action["type"].as_str() {
        Some("cancel") | Some("cancelByCloid") | Some("scheduleCancel") => {
            let open = hl.open_orders().await?;
            ensure!(
                open.as_array().is_some_and(|a| a.is_empty()),
                "open orders remain; inspect before clearing cancellation"
            );
            store.finish(&open)?;
            Ok(open)
        }
        Some("updateLeverage") => {
            let (asset, _) = hl.asset(&c.hyperliquid.hedge_coin).await?;
            ensure!(action["asset"] == asset, "different leverage asset");
            let state = hl.active_asset(&c.hyperliquid.hedge_coin).await?;
            ensure!(
                state["leverage"]["value"] == action["leverage"]
                    && state["leverage"]["type"]
                        == if action["isCross"] == true {
                            "cross"
                        } else {
                            "isolated"
                        },
                "cannot verify leverage change; inspect account configuration"
            );
            store.finish(&state)?;
            Ok(state)
        }
        _ => bail!(
            "pending non-order action requires checking its effect: {}",
            action["type"]
        ),
    }
}

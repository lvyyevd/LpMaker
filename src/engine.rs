use crate::{
    config::{Config, Mode},
    domain::*,
    evm::{UniswapV3, tx::Executor},
    hyperliquid::{Client, journal, num, orders, position_size},
    math, recovery,
    store::Store,
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
impl Live {
    async fn entry_guard(&self) -> Result<()> {
        let now = crate::now_ms();
        let s = self.liquidity.snapshot().await?;
        let (p, t) = self.hl.market(&self.cfg.hyperliquid.hedge_coin).await?;
        ensure!(
            now.saturating_sub(s.time_ms) <= self.cfg.strategy.max_data_age_seconds * 1000
                && now.saturating_sub(t) <= self.cfg.strategy.max_data_age_seconds * 1000,
            "entry prices stale"
        );
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
                now.saturating_sub(count as u64 * 3_600_000),
                now,
            )
            .await?;
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
            hedge_equity: num(&account["marginSummary"]["accountValue"])?,
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
        Ok(())
    }
    pub async fn hedge(&self, target: f64, emergency: bool) -> Result<()> {
        let coin = &self.cfg.hyperliquid.hedge_coin;
        let account = self.hl.account().await?;
        let signed = position_size(&account, coin)?;
        ensure!(signed <= 0.0, "unexpected long position");
        let current = -signed;
        let (bid, ask, time) = self.hl.book(coin).await?;
        let mid = (bid + ask) / 2.0;
        let target = if target * mid < 0.01 { 0.0 } else { target };
        let delta = target - current;
        ensure!(
            crate::now_ms().saturating_sub(time) <= self.cfg.strategy.max_data_age_seconds * 1000,
            "stale hedge book"
        );
        if delta.abs() * mid
            < if target == 0.0 {
                0.01
            } else {
                self.cfg.strategy.hedge_deadband_usd
            }
        {
            return Ok(());
        }
        if delta > 0.0 {
            ensure!(
                target * mid / self.cfg.hyperliquid.leverage as f64
                    <= self.cfg.strategy.hedge_collateral * 0.9,
                "hedge margin budget exceeded"
            );
            ensure!(
                delta * mid / self.cfg.hyperliquid.leverage as f64
                    <= num(&account["withdrawable"])?,
                "insufficient free hedge collateral"
            );
        }
        let (_, asset) = self.hl.asset(coin).await?;
        let size = orders::quantity(delta.abs(), asset.sz_decimals)?;
        if size == "0" {
            return Ok(());
        }
        let buy = delta < 0.0;
        let cloid = orders::cloid();
        let px = orders::price(if buy { bid } else { ask }, asset.sz_decimals, !buy)?;
        self.store.write(
            "hedge_order.json",
            &Some(json!({"coin":coin,"cloid":cloid,"target":target,"buy":buy,"price":px,"size":size,"tif":"Alo","submitted_ms":crate::now_ms()})),
        )?;
        let result = self
            .hl
            .order(coin, buy, &px, &size, "Alo", buy, &cloid)
            .await;
        if let Err(e) = result {
            if self.store.pending()?.is_some() {
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
        // Read exchange inventory after cancel acknowledgement; cancellation may race a fill.
        let current = -position_size(&self.hl.account().await?, coin)?;
        let remaining = target - current;
        if emergency && remaining.abs() * mid >= 0.01 {
            let (bid, ask, t) = self.hl.book(coin).await?;
            ensure!(
                crate::now_ms().saturating_sub(t) <= self.cfg.strategy.max_data_age_seconds * 1000,
                "stale emergency quote"
            );
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
            if sz != "0" {
                let id = orders::cloid();
                self.store.write(
                    "hedge_order.json",
                    &Some(json!({"coin":coin,"cloid":id,"target":target,
                    "buy":buy,"price":px,"size":sz,"tif":"Ioc","submitted_ms":crate::now_ms()})),
                )?;
                self.hl.order(coin, buy, &px, &sz, "Ioc", buy, &id).await?;
            }
            let actual = -position_size(&self.hl.account().await?, coin)?;
            ensure!(
                (target - actual).abs() * mid < self.cfg.strategy.hedge_deadband_usd.min(10.0),
                "emergency IOC only partially filled; inventory reconciliation required"
            );
        }
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        Ok(())
    }
    pub async fn cancel_if_open(&self, coin: &str, cloid: &str) -> Result<()> {
        let status = self.hl.order_status(json!(cloid)).await?;
        journal::observe(&self.store, cloid, &status)?;
        if status["status"] == "order" && status["order"]["status"] == "open" {
            let canceled = self.hl.cancel(coin, cloid).await;
            if self.store.pending()?.is_some() {
                canceled?;
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
        self.store.write(
            "workflow.json",
            &Some(json!({"decision":d,"started_ms":crate::now_ms()})),
        )?;
        let portfolio = self.portfolio().await?;
        // Inventory is temporarily fully hedged during a multi-venue LP transition.
        if let Err(error) = self.hedge(portfolio.base(), true).await {
            if d.lp != LpIntent::ExitToQuote || self.store.pending()?.is_some() {
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

pub async fn run(c: Config, store: Arc<Store>, once: bool, execute: bool) -> Result<()> {
    recovery::start(&store)?;
    let mut supervisor = crate::monitor::Supervisor::start(c.clone(), store.clone());
    let task = supervisor.task.as_mut().context("monitor task missing")?;
    let result = tokio::select! {
        result = run_strategy(c, store.clone(), once, execute) => result,
        result = task => {
            supervisor.task.take();
            match result { Ok(Err(e)) => Err(e.context("monitor failed; strategy stopped")),
                _ => Err(anyhow::anyhow!("monitor unexpectedly stopped; strategy stopped")) }
        },
        _ = tokio::signal::ctrl_c() => {
            store.event("shutdown", "positions retained; any prepared transaction remains pending for reconciliation")?;
            Ok(())
        }
    };
    let stopped = supervisor.stop().await;
    if let Err(error) = &result
        && store
            .read::<Value>("startup_reconciliation.json")?
            .is_some_and(|r| r["status"] != "ready")
    {
        recovery::stage(&store, "blocked", json!({"error":format!("{error:#}")}))?;
    }
    result.and(stopped)
}
async fn run_strategy(c: Config, store: Arc<Store>, once: bool, execute: bool) -> Result<()> {
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
        hl.enable_signing()?;
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
        hl.leverage(
            &c.hyperliquid.hedge_coin,
            c.hyperliquid.leverage,
            c.hyperliquid.cross_margin,
        )
        .await?;
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
        Some(l)
    } else {
        ensure!(
            store.pending()?.is_none(),
            "paper state has a live unresolved operation; refusing to ignore it"
        );
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
        json!({"phase":strategy.phase,"mode":c.mode}),
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
        let observed: Result<_> = async {
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
                        .funding_history(&c.hyperliquid.hedge_coin, paper.last_funding_ms + 1, now)
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
        }
        .await;
        let (pool, hp, ht, portfolio) = match observed {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(error=%format!("{e:#}"), "strategy market/account refresh failed; no trades until next valid snapshot");
                store.event("data_unavailable", format!("{e:#}"))?;
                if once {
                    return Err(e);
                }
                tokio::select! {_=tokio::time::sleep(Duration::from_secs(c.poll_seconds))=>continue,
                _=tokio::signal::ctrl_c()=>return Ok(())}
            }
        };
        let baseline = match store.read::<f64>("equity_baseline.json")? {
            Some(v) => v,
            None => {
                let v = portfolio.equity(pool.price);
                store.write("equity_baseline.json", &v)?;
                v
            }
        };
        store.write(
            "portfolio.json",
            &crate::monitor::PortfolioObservation {
                observed_ms: now,
                mark_price: pool.price,
                portfolio: portfolio.clone(),
                baseline_equity: baseline,
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
        let d = strategy.evaluate(&c.strategy, &frame);
        store.event("decision",json!({"decision":d,"pool":frame.pool,"equity":frame.portfolio.equity(frame.pool.price),"net_base":frame.portfolio.base()-frame.portfolio.short_base}))?;
        tracing::info!(phase=%d.state,action=?d.lp,price=frame.pool.price,equity=frame.portfolio.equity(frame.pool.price),reasons=?d.reasons,"strategy decision");
        if live.is_some() {
            recovery::save(&store, &c, &strategy, &paper)?;
        }
        if !d
            .reasons
            .iter()
            .any(|r| r.starts_with("stale_or_invalid_data"))
        {
            if let Some(l) = &live {
                l.apply(&d).await?;
                l.sync_orders(false).await?;
                l.portfolio().await?;
            } else {
                paper.apply(&d, &c, frame.pool.price, hp, now)?;
                recovery::save(&store, &c, &strategy, &paper)?;
                store.event("paper_execution", json!({"decision":d,"portfolio":paper.portfolio,"pending_hedge":paper.pending,"hedge_fees":paper.hedge_fees,"swap_costs":paper.swap_costs}))?;
            }
        }
        recovery::save(&store, &c, &strategy, &paper)?;
        if once {
            return Ok(());
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(c.poll_seconds))=>{},_=tokio::signal::ctrl_c()=>{store.event("shutdown","positions retained; no unsolicited flatten on process exit")?;return Ok(())}}
    }
}

struct NonceWorker(tokio::task::JoinHandle<()>);
impl Drop for NonceWorker {
    fn drop(&mut self) {
        self.0.abort();
        tracing::info!("periodic nonce worker stopped");
    }
}
/// Transport/observability changes do not reset positions or accounting. Economic
/// parameters and signer-account identity still require explicit state migration.
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
            let account = hl.account().await?;
            let (asset, _) = hl.asset(&c.hyperliquid.hedge_coin).await?;
            ensure!(action["asset"] == asset, "different leverage asset");
            let pos = account["assetPositions"]
                .as_array()
                .context("positions")?
                .iter()
                .find(|p| p["position"]["coin"] == c.hyperliquid.hedge_coin);
            ensure!(
                pos.is_some_and(|p| p["position"]["leverage"]["value"] == action["leverage"]
                    && p["position"]["leverage"]["type"]
                        == if action["isCross"] == true {
                            "cross"
                        } else {
                            "isolated"
                        }),
                "cannot verify leverage change; inspect account configuration"
            );
            store.finish(&account)?;
            Ok(account)
        }
        _ => bail!(
            "pending non-order action requires checking its effect: {}",
            action["type"]
        ),
    }
}

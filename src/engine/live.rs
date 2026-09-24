//! 实盘编排：先核对实际库存，再串行执行 LP 与对冲动作；平台细节由适配器负责。
use crate::{
    config::Config,
    domain::*,
    hyperliquid::{Client, account::collateral, num, position_size},
    math, recovery,
    store::Store,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn inventory_hedge_target(c: &crate::config::StrategyConfig, p: &Portfolio, price: f64) -> f64 {
    if let Some(band) = crate::strategy::eth::band::config(c) {
        return band.target(p, price);
    }
    if c.eth_persistent.is_some() {
        return p.base() * c.inside_hedge_ratio;
    }
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
/// 确定的入场风险变化：只在没有未知交易时允许执行器对账退出并进入正常冷却。
#[derive(Debug)]
pub(crate) struct BandEntryRisk;
impl std::fmt::Display for BandEntryRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "band entry risk changed during workflow")
    }
}
impl std::error::Error for BandEntryRisk {}
pub struct Live {
    pub liquidity: Arc<dyn LiquidityExecutor>,
    pub store: Arc<Store>,
    pub hl: Client,
    pub cfg: Config,
}
pub(super) use crate::hyperliquid::hedge::HedgeBudgetUnavailable;
impl Live {
    async fn entry_guard(&self) -> Result<Option<crate::strategy::eth::Feature>> {
        crate::runtime::observe(
            self.cfg.runtime.observation_timeout_seconds,
            self.entry_guard_inner(),
        )
        .await
    }
    async fn entry_guard_inner(&self) -> Result<Option<crate::strategy::eth::Feature>> {
        let started = crate::now_ms();
        let s = self.liquidity.snapshot().await?;
        let (p, t) = self.hl.market(&self.cfg.hyperliquid.hedge_coin).await?;
        ensure!(
            (s.price / p - 1.0).abs() * 10000.0 <= self.cfg.strategy.max_basis_bps,
            "entry basis limit"
        );
        let c = &self.cfg.strategy;
        let count = (c.vol_long_hours + c.vol_short_hours + 2)
            .max(c.ema_slow_hours * 3 + 2)
            .max(if c.eth_persistent.is_some() { 74 } else { 0 });
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
        if let Some(profile) = &c.eth_persistent {
            let f = crate::strategy::eth::latest_feature(&bars, now)?;
            if profile.band.is_some() {
                let checkpoint = self
                    .store
                    .read::<recovery::Checkpoint>("checkpoint.json")?
                    .context("band entry requires persisted risk basis")?;
                let actual = self.portfolio().await?;
                crate::runtime::fresh(
                    started,
                    crate::now_ms(),
                    self.cfg.runtime.observation_timeout_seconds,
                )?;
                if profile.guard(c).danger(&f, s.price).is_some()
                    || !crate::strategy::eth::band::entry_equity_safe(
                        &checkpoint.strategy,
                        c,
                        actual.equity(s.price),
                    )
                {
                    return Err(BandEntryRisk.into());
                }
            }
            ensure!(
                profile.guard(c).danger(&f, s.price).is_none(),
                "persistent LP market became unsafe during workflow; preserve hedge and reconcile"
            );
            return Ok(Some(f));
        }
        let m = crate::strategy::indicators::calculate(&bars, c, now)?;
        ensure!(
            !m.downtrend
                && m.vol_ratio < c.vol_pause_ratio
                && m.return_1h >= -c.fast_drop_1h
                && s.price / m.last_close - 1.0 >= -c.fast_drop_1h,
            "market became unsafe during LP workflow; hold hedge and reconcile workflow"
        );
        Ok(None)
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
    fn hedge_controller(&self) -> crate::hyperliquid::hedge::Controller<'_> {
        crate::hyperliquid::hedge::Controller {
            hl: &self.hl,
            store: &self.store,
            strategy: &self.cfg.strategy,
        }
    }
    pub async fn sync_orders(&self, recover_managed: bool) -> Result<()> {
        self.hedge_controller().sync_orders(recover_managed).await
    }
    pub async fn hedge(&self, target: f64, emergency: bool) -> Result<()> {
        self.hedge_controller().hedge(target, emergency).await
    }
    pub async fn cancel_if_open(&self, coin: &str, cloid: &str) -> Result<()> {
        self.hedge_controller().cancel_if_open(coin, cloid).await
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
        let band = crate::strategy::eth::band::config(&self.cfg.strategy);
        // 新候选复用库存与原空单。旧策略仍保持过渡期全额保护。
        if band.is_some() {
            self.sync_orders(true).await?;
        }
        if band.is_none()
            && let Err(error) = self.hedge(portfolio.base(), true).await
        {
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
        if let Some(band) = band
            && matches!(d.lp, LpIntent::Recenter { .. })
        {
            self.store.event("band_roll_wait",json!({"seconds":band.roll_delay_seconds,"note":"旧LP已退出，保留实际库存与空单；重启按workflow对账退出，不重复mint"}))?;
            tokio::time::sleep(std::time::Duration::from_secs(band.roll_delay_seconds)).await;
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
                let feature = self.entry_guard().await?;
                let widths =
                    band.map(|b| b.widths(feature.as_ref().expect("validated ETH feature")));
                let s = self.liquidity.snapshot().await?;
                let value = budget * l.weight / total_weight;
                // 让协议适配器处理 tick 对齐所需配比；Robinhood 保留旧等值逻辑。
                let target_base = if let Some(w) = widths {
                    self.liquidity.bounded_base_requirement(value, w, &s)?
                } else {
                    self.liquidity
                        .mint_base_requirement(value, l.half_width, &s)?
                };
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
                if band.is_none() {
                    let inventory = self.portfolio().await?;
                    self.hedge(inventory.base(), true).await?;
                }
                self.entry_guard().await?;
                // 0.2% cushion covers tick rounding and small price movement; excess stays in the wallet.
                if let Some(w) = widths {
                    tracing::info!(
                        lower_width = w.0,
                        upper_width = w.1,
                        budget = value,
                        "Robinhood 净敞口策略准备单区间建仓"
                    );
                    self.liquidity
                        .mint_bounded_layer(&l.name, value * 0.998, w)
                        .await?;
                } else {
                    self.liquidity
                        .mint_layer(&l.name, value * 0.998, l.half_width)
                        .await?;
                }
            }
            let inventory = self.portfolio().await?;
            let price = self.liquidity.snapshot().await?.price;
            let target = inventory_hedge_target(&self.cfg.strategy, &inventory, price);
            self.hedge(target, true).await?;
        }
        self.store.write("workflow.json", &Option::<Value>::None)?;
        Ok(())
    }

    /// 普通持有循环复用本轮刚核对的库存，避免仅为判断保护性对冲再完整读链。
    /// 实际下单仍由对冲控制器重新检查交易所持仓、挂单和资金。
    pub async fn hold_observed(
        &self,
        d: &Decision,
        portfolio: &Portfolio,
        observed_ms: u64,
    ) -> Result<()> {
        ensure!(
            d.lp == LpIntent::Hold,
            "observed hold cannot execute LP mutations"
        );
        crate::runtime::fresh(
            observed_ms,
            crate::now_ms(),
            self.cfg.strategy.max_data_age_seconds,
        )?;
        self.hedge(
            d.target_short_base,
            d.emergency
                || d.target_short_base > 0.0 && d.target_short_base >= portfolio.base() * 0.99,
        )
        .await
    }
}

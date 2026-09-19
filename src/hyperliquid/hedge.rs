//! 链无关的 Hyperliquid 对冲执行器。EVM 和 Solana 共用同一套挂单、撤单核对和紧急 IOC 规则。
use crate::{
    config::StrategyConfig,
    domain::HedgeVenue,
    hyperliquid::{Client, account::collateral, journal, orders, position_size},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
#[derive(Debug)]
pub(crate) struct HedgeBudgetUnavailable(pub &'static str);
impl std::fmt::Display for HedgeBudgetUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for HedgeBudgetUnavailable {}
pub struct Controller<'a> {
    pub hl: &'a Client,
    pub store: &'a Arc<Store>,
    pub strategy: &'a StrategyConfig,
}
impl Controller<'_> {
    pub async fn sync_orders(&self, recover_managed: bool) -> Result<()> {
        // Persist raw observations even when an unknown/manual order blocks recovery.
        let open = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":open}),
        )?;
        journal::refresh(self.hl, self.store).await?;
        let open = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":open}),
        )?;
        let ids = journal::managed_open_orders(self.store, &open, &self.hl.cfg.hedge_coin)?;
        ensure!(
            recover_managed || ids.is_empty(),
            "unexpected outstanding strategy order; restart reconciliation required"
        );
        for id in ids {
            self.store
                .event("startup_cancel_stale_hedge", json!({"cloid":id}))?;
            self.cancel_if_open(&self.hl.cfg.hedge_coin, &id).await?;
        }
        let after = self.hl.open_orders().await?;
        self.store.write(
            "open_orders.json",
            &json!({"observed_ms":crate::now_ms(),"user":self.hl.user()?,"orders":after}),
        )?;
        if !after.as_array().context("open orders response")?.is_empty() {
            // 已核对终态却仍出现在滞后的列表中：退避重读。手工/身份不符的单仍硬阻断。
            journal::managed_open_orders(self.store, &after, &self.hl.cfg.hedge_coin)?;
            return Err(crate::runtime::ReadUnavailable(
                "open orders remain after reconciliation; retry reads before further trading"
                    .into(),
            )
            .into());
        }
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        journal::compact(self.store)?;
        Ok(())
    }
    pub async fn hedge(&self, target: f64, emergency: bool) -> Result<()> {
        ensure!(target.is_finite() && target >= 0.0, "invalid hedge target");
        let coin = &self.hl.cfg.hedge_coin;
        let account = self.hl.account().await?;
        let signed = position_size(&account, coin)?;
        ensure!(signed <= 0.0, "unexpected long position");
        let current = -signed;
        let (bid, ask, time) = self.hl.book(coin).await?;
        let mid = (bid + ask) / 2.0;
        let target = if target * mid < 0.01 { 0.0 } else { target };
        let delta = target - current;
        crate::runtime::fresh(time, crate::now_ms(), self.strategy.max_data_age_seconds)?;
        if delta.abs() * mid
            < if target == 0.0 || emergency {
                0.01
            } else {
                self.strategy.hedge_deadband_usd
            }
        {
            return self.hedge_residual(target, current, mid, "within_hedge_deadband");
        }
        if delta > 0.0 {
            if target * mid / self.hl.cfg.leverage as f64 > self.strategy.hedge_collateral * 0.9 {
                return Err(HedgeBudgetUnavailable("hedge margin budget exceeded").into());
            }
            if delta * mid / self.hl.cfg.leverage as f64
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
                self.strategy.max_data_age_seconds,
            )
            .await;
        if let Err(e) = result {
            if self.store.pending()?.is_some() || !e.is::<crate::hyperliquid::ExchangeRejected>() {
                return Err(e);
            }
            self.store.event("maker_rejected", format!("{e:#}"))?;
        } else {
            tokio::time::sleep(Duration::from_secs(if emergency {
                self.hl.cfg.maker_wait_seconds.min(2)
            } else {
                self.hl.cfg.maker_wait_seconds
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
            crate::runtime::fresh(t, crate::now_ms(), self.strategy.max_data_age_seconds)?;
            let buy = remaining < 0.0;
            let slip = self.hl.cfg.emergency_slippage_bps as f64 / 10000.0;
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
                        self.strategy.max_data_age_seconds,
                    )
                    .await?;
            }
            let actual = -position_size(&self.hl.perp_account().await?, coin)?;
            final_current = actual;
            ensure!(
                (target - actual).abs() * mid < self.strategy.hedge_deadband_usd.min(10.0),
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
        let row = json!({"observed_ms":crate::now_ms(),"coin":self.hl.cfg.hedge_coin,
            "target":target,"actual_short":actual,"residual_base":target-actual,
            "residual_usd":(target-actual).abs()*price,"reason":reason,
            "action":"recompute from actual inventory next cycle; do not increase size to meet minimum"});
        self.store.write("hedge_residual.json", &row)?;
        self.store.event("hedge_residual_deferred", &row)?;
        if reason == "within_hedge_deadband" {
            tracing::debug!(residual=%row, "对冲余量位于允许偏差内，暂不调整");
        } else {
            tracing::warn!(residual=%row, "对冲仍有未完成余量，后续按实际持仓重新核对");
        }
        Ok(())
    }
    pub async fn cancel_if_open(&self, coin: &str, cloid: &str) -> Result<()> {
        let status = super::order_recovery::resolve(self.hl, self.store, cloid, false).await?;
        if status["status"] == "order" && status["order"]["status"] == "open" {
            let canceled = self.hl.cancel(coin, cloid).await;
            if let Err(error) = canceled
                && (self.store.pending()?.is_some()
                    || !error.is::<crate::hyperliquid::ExchangeRejected>())
            {
                return Err(error);
            }
            // An acknowledged rejection can race a fill: the subsequent status is authoritative.
            let after = super::order_recovery::resolve(self.hl, self.store, cloid, true).await?;
            ensure!(
                after["order"]["status"]
                    .as_str()
                    .is_some_and(journal::terminal_status),
                "cancel/fill not confirmed"
            );
        }
        Ok(())
    }
}

pub async fn reconcile(c: &crate::config::HyperliquidConfig, store: Arc<Store>) -> Result<Value> {
    let pending = match store.pending()? {
        Some(v) => v,
        None => return Ok(json!({"status":"no_pending_operation"})),
    };
    ensure!(
        pending["venue"] == "hyperliquid",
        "unknown pending venue; operation retained"
    );
    let hl = Client::new(c.clone(), store.clone())?;
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
            let status = super::order_recovery::resolve(&hl, &store, id, false).await?;
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
            let (asset, _) = hl.asset(&c.hedge_coin).await?;
            ensure!(action["asset"] == asset, "different leverage asset");
            let state = hl.active_asset(&c.hedge_coin).await?;
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

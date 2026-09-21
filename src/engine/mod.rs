//! 运行入口与恢复循环。保持原检查点字段、配置指纹和交易顺序，重启沿用旧状态。
pub mod entry_rearm;
mod live;
mod paper;
pub mod reset;
use crate::{
    config::{Config, Mode},
    domain::*,
    hyperliquid::{Client, account::collateral},
    liquidity::uniswap_v3::tx::Executor,
    recovery,
    store::Store,
    strategy::Strategy,
};
use anyhow::{Context, Result, ensure};
use live::HedgeBudgetUnavailable;
pub use live::{Live, inventory_hedge_target};
pub use paper::{Paper, PaperOrder};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub async fn run(
    c: Config,
    store: Arc<Store>,
    once: bool,
    execute: bool,
    first_entry: bool,
) -> Result<()> {
    ensure!(
        store.read::<Value>("manual_reset.json")?.is_none(),
        "手工退出/清理尚未完成；保留记录，重新执行 reset-flat --execute，勿直接运行策略"
    );
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
    let venue = crate::liquidity::connect(c.liquidity.clone())?;
    venue.validate().await?;
    let mut hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let (mut strategy, mut paper) = recovery::load(&store, &c)?;
    recovery::prepare_observation_revalidation(&mut strategy);
    recovery::stage(
        &store,
        "checkpoint_loaded",
        json!({"phase":strategy.phase,"mode":c.mode,
        "healthy_hours":strategy.healthy_hours,
        "note":"healthy-hour progress frozen pending fresh market/candle verification; breakout streaks reset; balances and risk phase retained"}),
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
        let rpc=venue.rpc.clone();
        NonceWorker(tokio::spawn(async move {
            let mut timer=tokio::time::interval(Duration::from_secs(seconds));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                if !rpc.cooldown_remaining().is_zero() {
                    tracing::debug!("nonce 定时查询等待 RPC 退避；发送交易前仍需重新核对");
                    continue;
                }
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
        let mut d = entry_rearm::evaluate(&store, &c, &mut strategy, &frame)?;
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
        store.event("decision",json!({"decision":d,"pool":frame.pool,"equity":frame.portfolio.equity(frame.pool.price),"net_base":frame.portfolio.base()-frame.portfolio.short_base,"recovery_progress":strategy.recovery_progress}))?;
        tracing::info!(phase=%d.state,entry_history=?strategy.entry_history,action=?d.lp,price=frame.pool.price,equity=frame.portfolio.equity(frame.pool.price),reasons=?d.reasons,"strategy decision");
        if let Some(r) = &strategy.recovery_progress.report {
            tracing::debug!(report=?r, last_reset=?strategy.recovery_progress.last_reset, "recovery conditions evaluated");
        }
        if live.is_some() {
            entry_rearm::consume(&store, &c, &d)?;
            recovery::save(&store, &c, &strategy, &paper)?;
        }
        if !entry_deferred
            && !d
                .reasons
                .iter()
                .any(|r| r.starts_with("stale_or_invalid_data"))
        {
            if let Some(l) = &live {
                if d.lp == LpIntent::Hold {
                    l.hold_observed(&d, &frame.portfolio, frame.pool.time_ms)
                        .await?;
                    l.sync_orders(false).await?;
                    // Hold 只操作合约对冲，不改变 LP；下一轮继续核对链上库存。
                    strategy.observe_lp(!frame.portfolio.positions.is_empty());
                } else {
                    l.apply(&d).await?;
                    l.sync_orders(false).await?;
                    strategy.observe_lp(!l.portfolio().await?.positions.is_empty());
                }
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
                "rpc_min_interval_ms",
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
        let evm = Executor::new(
            crate::liquidity::connect(c.liquidity.clone())?,
            store.clone(),
        )?;
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
    crate::hyperliquid::hedge::reconcile(&c.hyperliquid, store).await
}

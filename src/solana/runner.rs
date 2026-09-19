//! Solana 独立运行器：链上动作串行，重启先核对签名、position 公钥和 HL 订单。
use super::{
    bridge::Bridge,
    config::Config,
    dlmm::{self, Paper, Snapshot},
    journal::{self, Positions},
};
use crate::{
    config::Mode,
    domain::{Decision, HedgeVenue, LpIntent, MarketFrame, Portfolio},
    hyperliquid::{Client, account::collateral, hedge::Controller, num, position_size},
    store::Store,
    strategy::{Phase, Strategy},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema: u32,
    pub strategy: Strategy,
    pub paper: Paper,
}
pub fn bind(store: &Store, c: &Config) -> Result<()> {
    ensure!(
        store.read::<Value>("checkpoint.json")?.is_none()
            && store.read::<Value>("config.json")?.is_none(),
        "EVM state directory cannot be used by Solana"
    );
    if let Some(old) = store.read::<Value>("solana_identity.json")? {
        ensure!(
            old == c.fingerprint(),
            "Solana economic identity changed; retain old state, use a separate directory or migrate explicitly"
        );
    }
    store.write("solana_identity.json", &c.fingerprint())
}
async fn snapshot(b: &Bridge) -> Result<Snapshot> {
    Ok(serde_json::from_value(
        b.call(json!({"method":"observe"})).await?,
    )?)
}
fn positions(c: &Config, store: &Store, s: &Snapshot) -> Result<Vec<crate::domain::LpPosition>> {
    let tracked = store
        .read::<Positions>("solana_positions.json")?
        .unwrap_or_default();
    ensure!(
        s.positions
            .iter()
            .all(|p| tracked.values().any(|t| t.address == p.address)),
        "untracked DLMM position; use dedicated wallet or reconcile mappings"
    );
    ensure!(
        tracked
            .values()
            .all(|t| s.positions.iter().any(|p| p.address == t.address)),
        "tracked DLMM position missing; do not automatically re-enter"
    );
    tracked
        .iter()
        .map(|(layer, r)| {
            ensure!(
                c.strategy.layers.iter().any(|l| l.name == *layer),
                "unknown tracked layer"
            );
            Ok(s.position(
                s.positions.iter().find(|p| p.address == r.address).unwrap(),
                layer,
            ))
        })
        .collect()
}
async fn portfolio(c: &Config, store: &Store, s: &Snapshot, hl: &Client) -> Result<Portfolio> {
    let account = hl.account().await?;
    let signed = position_size(&account, "SOL")?;
    ensure!(
        signed <= 0.,
        "unexpected SOL long; dedicated hedge account required"
    );
    for p in account["assetPositions"].as_array().context("positions")? {
        ensure!(
            p["position"]["coin"] == "SOL" || num(&p["position"]["szi"])? == 0.,
            "another strategy uses this Hyperliquid account"
        );
    }
    store.write(
        "account_snapshot.json",
        &json!({"observed_ms":crate::now_ms(),"account":account,"user":hl.user()?}),
    )?;
    Ok(Portfolio {
        positions: positions(c, store, s)?,
        wallet_base: s.wallet.base,
        wallet_quote: s.wallet.quote,
        short_base: -signed,
        hedge_equity: collateral(&account)?.equity_usdc,
        reserve: c.strategy.reserve,
    })
}
async fn entry_guard(c: &Config, b: &Bridge, hl: &Client) -> Result<Snapshot> {
    let s = snapshot(b).await?;
    let now = crate::now_ms();
    s.validate(
        crate::now_ms(),
        c.strategy.max_data_age_seconds,
        c.solana.require_grpc,
    )?;
    let (p, t) = hl.market("SOL").await?;
    crate::runtime::fresh(t, now, c.strategy.max_data_age_seconds)?;
    ensure!(
        (s.price / p - 1.).abs() * 10000. <= c.strategy.max_basis_bps,
        "entry basis limit"
    );
    let count = (c.strategy.vol_long_hours + c.strategy.vol_short_hours + 2)
        .max(c.strategy.ema_slow_hours * 3 + 2)
        .max(c.regime.as_ref().map_or(0, |r| r.history_hours() + 2));
    let bars = hl
        .candles("SOL", "1h", now.saturating_sub(count as u64 * 3600000), now)
        .await?;
    let m = crate::strategy::indicators::calculate(&bars, &c.strategy, now)?;
    ensure!(
        !m.downtrend
            && m.vol_ratio < c.strategy.vol_pause_ratio
            && m.return_1h >= -c.strategy.fast_drop_1h
            && s.price / m.last_close - 1. >= -c.strategy.fast_drop_1h,
        "market unsafe during DLMM workflow"
    );
    if let Some(regime) = &c.regime {
        let signal = super::regime::signals(regime, &c.strategy, &bars, now)?;
        ensure!(
            signal.entry
                && !signal.exit
                && (s.price / signal.last_close - 1.).abs() <= regime.max_bar_range,
            "SOL regime changed during DLMM workflow"
        );
    }
    Ok(s)
}
async fn apply(
    c: &Config,
    store: &Arc<Store>,
    b: &Bridge,
    hl: &Client,
    d: &Decision,
    fraction: f64,
) -> Result<()> {
    let controller = Controller {
        hl,
        store,
        strategy: &c.strategy,
    };
    let s = snapshot(b).await?;
    s.validate(
        crate::now_ms(),
        c.strategy.max_data_age_seconds,
        c.solana.require_grpc,
    )?;
    let initial = portfolio(c, store, &s, hl).await?;
    if d.lp == LpIntent::Hold {
        return controller.hedge(d.target_short_base, d.emergency).await;
    }
    if d.lp == LpIntent::ExitToQuote
        && initial.positions.is_empty()
        && initial.wallet_base * s.price < 0.01
    {
        return controller.hedge(0., true).await;
    }
    if matches!(d.lp, LpIntent::Deploy { .. } | LpIntent::Recenter { .. }) {
        let funds = collateral(&hl.account().await?)?;
        let needed = c.strategy.lp_budget * fraction / c.hyperliquid.leverage as f64 / 0.9;
        ensure!(
            needed <= c.strategy.hedge_collateral
                && needed <= funds.equity_usdc
                && needed <= funds.available_short_usdc,
            "insufficient collateral for full inventory transition"
        );
        entry_guard(c, b, hl).await?;
        if initial.positions.is_empty() {
            let mut rent = 0.;
            for layer in &c.strategy.layers {
                let v=b.call(json!({"method":"allocation","value":c.strategy.lp_budget*fraction*layer.weight,"width":layer.half_width})).await?;
                let required = v["position_rent_sol"]
                    .as_f64()
                    .context("position rent quote")?;
                ensure!(
                    required <= c.solana.max_rent_sol,
                    "position rent exceeds per-workflow ceiling; no swap sent"
                );
                rent += required;
            }
            let reserve_sol = rent + c.solana.min_native_sol + 0.004;
            ensure!(
                reserve_sol <= s.wallet.native && reserve_sol * s.price <= c.strategy.reserve,
                "rent/gas reserve insufficient; no inventory purchased"
            );
        }
    }
    store.write(
        "solana_workflow.json",
        &json!({"decision":d,"started_ms":crate::now_ms()}),
    )?;
    // If a confirmed budget failure prevents an exit hedge, still allow disposing spot risk; unknown results block all mutations.
    if let Err(e) = controller.hedge(initial.base(), true).await {
        if d.lp != LpIntent::ExitToQuote
            || store.pending()?.is_some()
            || !(e.is::<crate::hyperliquid::ExchangeRejected>()
                || e.is::<crate::hyperliquid::hedge::HedgeBudgetUnavailable>())
        {
            return Err(e);
        }
        store.event(
            "solana_exit_without_full_hedge",
            json!({"reason":"confirmed hedge capacity/rejection"}),
        )?;
    }
    let names: Vec<String> = match &d.lp {
        LpIntent::Recenter { layers } => layers.clone(),
        _ => c.strategy.layers.iter().map(|l| l.name.clone()).collect(),
    };
    let scaling = matches!(d.lp, LpIntent::Deploy { .. }) && !initial.positions.is_empty();
    if !scaling {
        for p in initial
            .positions
            .iter()
            .filter(|p| names.contains(&p.layer))
        {
            journal::execute(
                b,
                store,
                json!({"kind":"remove","position":p.token_id}),
                Some(&p.layer),
            )
            .await?;
        }
    }
    if d.lp == LpIntent::ExitToQuote {
        let s = snapshot(b).await?;
        if s.wallet.base * s.price >= 0.01 {
            journal::execute(
                b,
                store,
                json!({"kind":"swap","sell_base":true,"amount":s.wallet.base}),
                None,
            )
            .await?;
        }
        controller
            .hedge(snapshot(b).await?.wallet.base, true)
            .await?;
    } else {
        let fraction = match d.lp {
            LpIntent::Deploy { fraction } => fraction,
            _ => fraction,
        }; // Recovery recenter must retain the recovery fraction.
        for l in c.strategy.layers.iter().filter(|l| names.contains(&l.name)) {
            let s = entry_guard(c, b, hl).await?;
            let p = portfolio(c, store, &s, hl).await?;
            let existing = p.positions.iter().find(|p| p.layer == l.name);
            let value = (c.strategy.lp_budget * fraction * l.weight
                - existing.map(|p| p.base * s.price + p.quote).unwrap_or(0.))
            .min(s.wallet.base * s.price + s.wallet.quote)
                * 0.995;
            if value < 1. {
                continue;
            }
            let bounds = existing
                .and_then(|p| {
                    s.positions
                        .iter()
                        .find(|x| Some(&x.address) == p.token_id.as_ref())
                })
                .map(|p| json!({"minBinId":p.lower_bin,"maxBinId":p.upper_bin}));
            let amounts=b.call(json!({"method":"allocation","value":value,"width":l.half_width,"bounds":bounds})).await?;
            let required = amounts["base"].as_f64().context("DLMM base requirement")?;
            let delta = required - s.wallet.base;
            if delta.abs() * s.price >= 0.01 {
                journal::execute(b,store,json!({"kind":"swap","sell_base":delta<0.,"amount":if delta<0.{-delta}else{delta*s.price*(1.+c.solana.slippage_bps as f64/10000.)}}),None).await?;
            }
            let s = snapshot(b).await?;
            controller
                .hedge(portfolio(c, store, &s, hl).await?.base(), true)
                .await?;
            entry_guard(c, b, hl).await?;
            journal::execute(b,store,json!({"kind":if existing.is_some(){"increase"}else{"mint"},"position":existing.and_then(|p|p.token_id.clone()),"value":value*0.998,"width":l.half_width}),Some(&l.name)).await?;
        }
        let s = snapshot(b).await?;
        let p = portfolio(c, store, &s, hl).await?;
        controller
            .hedge(
                crate::engine::inventory_hedge_target(&c.strategy, &p, s.price),
                true,
            )
            .await?;
    }
    store.write("solana_workflow.json", &Option::<Value>::None)?;
    Ok(())
}
pub async fn monitor(c: Config, seconds: u64) -> Result<()> {
    let b = Bridge::start(&c, false).await?;
    let store = Arc::new(Store::open(&c.state_dir)?);
    bind(&store, &c)?;
    let hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let start = tokio::time::Instant::now();
    loop {
        let s = snapshot(&b).await?;
        let (price, _) = hl.market("SOL").await?;
        tracing::info!(
            "【Solana Meteora｜只读】SOL {:.4} USDC｜Hyperliquid {:.4} USD｜active bin {}｜gRPC {}｜仓位 {} 个",
            s.price,
            price,
            s.active_bin,
            s.grpc["connected"],
            s.positions.len()
        );
        store.write("solana_monitor.json", &s)?;
        if seconds > 0 && start.elapsed().as_secs() >= seconds {
            return Ok(());
        }
        tokio::select! {_=crate::runtime::shutdown()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(c.report_seconds))=>{}}
    }
}
pub async fn run(c: Config, once: bool, execute: bool) -> Result<()> {
    let live = c.mode == Mode::Live;
    ensure!(!live || execute, "live requires --execute");
    let store = Arc::new(Store::open(&c.state_dir)?);
    bind(&store, &c)?;
    // Native official Hyperliquid WS retains existing reconnect/heartbeat semantics.
    let (stop, rx) = tokio::sync::watch::channel(false);
    let (tx, mut events) = tokio::sync::mpsc::channel(1024);
    let url = c.hyperliquid.ws_url.clone();
    let subs = crate::hyperliquid::ws::subscriptions(
        &c.hyperliquid.coins,
        c.hyperliquid
            .vault
            .as_deref()
            .or(c.hyperliquid.account.as_deref()),
    );
    let ws = c.websocket.clone();
    let mut socket = tokio::spawn(async move {
        crate::hyperliquid::ws::listen_with_config(&url, subs, ws, tx, rx).await
    });
    let event_store = store.clone();
    let consumer = tokio::spawn(async move {
        while let Some(e) = events.recv().await {
            if matches!(
                e.channel.as_str(),
                "userFills" | "orderUpdates" | "userFundings"
            ) {
                event_store.event("hyperliquid_account_event", &e)?;
            }
            if matches!(e.channel.as_str(), "connected" | "disconnected") {
                event_store.write("solana_hl_ws.json", &e)?;
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let result = tokio::select! {r=inner(&c,store.clone(),once,live)=>r,_=crate::runtime::shutdown()=>Ok(()),_= &mut socket=>Err(anyhow::anyhow!("Hyperliquid WS supervisor stopped"))};
    let _ = stop.send(true);
    if !socket.is_finished() {
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut socket).await;
    }
    socket.abort();
    consumer.abort();
    let _ = consumer.await;
    crate::runtime::status(
        &store,
        if result.is_ok() { "stopped" } else { "blocked" },
        json!({"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    result
}
async fn inner(c: &Config, store: Arc<Store>, once: bool, live: bool) -> Result<()> {
    let b = Bridge::start(c, live).await?;
    let mut hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let mut cp = store
        .read::<Checkpoint>("solana_checkpoint.json")?
        .unwrap_or(Checkpoint {
            schema: 1,
            strategy: Strategy::default(),
            paper: Paper::new(&c.strategy),
        });
    ensure!(cp.schema == 1, "unknown Solana checkpoint schema");
    cp.paper.leverage = c.hyperliquid.leverage;
    crate::recovery::invalidate_observation_streaks(&mut cp.strategy);
    if live {
        journal::reconcile(&b, &store).await?;
        crate::hyperliquid::hedge::reconcile(&c.hyperliquid, store.clone()).await?;
        hl.enable_signing().await?;
        let controller = Controller {
            hl: &hl,
            store: &store,
            strategy: &c.strategy,
        };
        controller.sync_orders(true).await?;
        let lev = hl.active_asset("SOL").await?;
        if lev["leverage"]["value"] != c.hyperliquid.leverage
            || lev["leverage"]["type"] != "isolated"
        {
            hl.leverage("SOL", c.hyperliquid.leverage, false).await?;
        }
        let s = snapshot(&b).await?;
        let p = portfolio(c, &store, &s, &hl).await?;
        cp.strategy.observe_lp(!p.positions.is_empty());
        if store.read::<Value>("solana_workflow.json")?.is_some() {
            apply(
                c,
                &store,
                &b,
                &hl,
                &Decision {
                    state: "Paused".into(),
                    reasons: vec!["incomplete_workflow_recovery".into()],
                    lp: LpIntent::ExitToQuote,
                    target_short_base: p.base(),
                    emergency: true,
                },
                cp.strategy.fraction,
            )
            .await?;
            crate::recovery::finish_workflow_recovery(&mut cp.strategy);
        }
        if matches!(cp.strategy.phase, Phase::Active | Phase::Recovering)
            && p.positions.len() != c.strategy.layers.len()
        {
            cp.strategy.phase = Phase::Paused;
            cp.strategy.pause_since = crate::now_ms();
            cp.strategy.healthy_hours = 0;
        }
    } else {
        ensure!(
            store.pending()?.is_none() && store.read::<Value>("solana_pending.json")?.is_none(),
            "paper cannot ignore a pending live operation"
        );
    }
    store.write("solana_checkpoint.json", &cp)?;
    let mut bars = vec![];
    let mut hour = 0;
    let mut last_report = 0;

    loop {
        let now = crate::now_ms();
        let observation: Result<_> = async {
            let s = snapshot(&b).await?;
            s.validate(
                crate::now_ms(),
                c.strategy.max_data_age_seconds,
                c.solana.require_grpc,
            )?;
            let (hp, ht) = hl.market("SOL").await?;
            if hour != now / 3600000 {
                let count = (c.strategy.vol_long_hours + c.strategy.vol_short_hours + 2)
                    .max(c.strategy.ema_slow_hours * 3 + 2)
                    .max(c.regime.as_ref().map_or(0, |r| r.history_hours() + 2));
                bars = hl
                    .candles("SOL", "1h", now.saturating_sub(count as u64 * 3600000), now)
                    .await?;
                hour = now / 3600000;
            }
            Ok((s, hp, ht))
        }
        .await;
        let (s, hp, ht) = match observation {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error=%e,"Solana 观察失败，等待重新核对，不发送交易");
                if once {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let p = if live {
            Controller {
                hl: &hl,
                store: &store,
                strategy: &c.strategy,
            }
            .sync_orders(false)
            .await?;
            portfolio(c, &store, &s, &hl).await?
        } else {
            cp.paper.mark(s.price, hp);
            cp.paper.portfolio(&c.strategy)
        };
        let f = MarketFrame {
            now_ms: crate::now_ms(),
            pool: dlmm::strategy_snapshot(s.price, s.time_ms),
            hedge_price: hp,
            hedge_time_ms: ht,
            candles: bars.clone(),
            portfolio: p.clone(),
        };
        let previous_phase = cp.strategy.phase.clone();
        let d = if let Some(regime) = &c.regime {
            super::regime::evaluate(regime, &c.strategy, &mut cp.strategy, &f)
        } else {
            cp.strategy.evaluate(&c.strategy, &f)
        };
        if c.regime.is_some()
            && (previous_phase != cp.strategy.phase
                || (d.lp != LpIntent::Hold && !p.positions.is_empty())
                || matches!(d.lp, LpIntent::Deploy { .. }))
        {
            tracing::info!(phase=?cp.strategy.phase, action=?d.lp, target_short_sol=d.target_short_base,
                reasons=%d.reasons.iter().map(|r|super::regime::reason_zh(r)).collect::<Vec<_>>().join("；"),"Solana 趋势策略决策");
        }
        store.event(
            "solana_decision",
            json!({"decision":d,"price":s.price,"portfolio":p}),
        )?;
        if live {
            store.write("solana_checkpoint.json", &cp)?;
        }
        if !d
            .reasons
            .iter()
            .any(|r| r.starts_with("stale_or_invalid_data"))
        {
            if live {
                apply(c, &store, &b, &hl, &d, cp.strategy.fraction).await?;
            } else {
                cp.paper.apply(
                    &d,
                    &c.strategy,
                    s.price,
                    hp,
                    now,
                    cp.strategy.fraction,
                    0.10,
                    0.00075,
                )?;
                cp.strategy.observe_lp(!cp.paper.positions.is_empty());
            }
        }
        store.write("solana_checkpoint.json", &cp)?;
        if now.saturating_sub(last_report) >= c.report_seconds * 1000 {
            let s = if live { snapshot(&b).await? } else { s.clone() };
            let p = if live {
                portfolio(c, &store, &s, &hl).await?
            } else {
                cp.paper.portfolio(&c.strategy)
            };
            let fees = p
                .positions
                .iter()
                .map(|p| p.unclaimed_base * s.price + p.unclaimed_quote)
                .sum::<f64>();
            let principal = p
                .positions
                .iter()
                .map(|p| {
                    p.base * s.price + p.quote - p.unclaimed_base * s.price - p.unclaimed_quote
                })
                .sum::<f64>();
            let performance = if live {
                super::performance::observe(c, &store, &s)?
            } else {
                json!({})
            };
            let apr = performance["fee_apr_1h"]["apr_pct"].as_f64();
            tracing::info!(
                "【Solana Meteora｜{}】SOL {:.4} USDC｜阶段 {:?}｜LP {} 层｜本金 {:.4} USDC｜待领费用 {:.6} USDC\nHyperliquid SOL 空单 {:.4}｜权益 {:.4} USDC｜组合净值 {:.4} USD\n近1小时手续费APR：{}（不足1小时为观察期折年化；非净利润）｜gRPC {}｜动作 {:?}",
                if live {
                    "实盘"
                } else {
                    "模拟，未计LP费用及资金费"
                },
                s.price,
                cp.strategy.phase,
                p.positions.len(),
                principal,
                fees,
                p.short_base,
                p.hedge_equity,
                p.equity(s.price),
                apr.map(|x| format!("{x:.2}%"))
                    .unwrap_or_else(|| "数据积累中/模拟不估算".into()),
                s.grpc["connected"],
                d.lp
            );
            let tracked = store
                .read::<Positions>("solana_positions.json")?
                .unwrap_or_default();
            for pos in &p.positions {
                let since = if live {
                    tracked.get(&pos.layer).map(|r| r.since_ms)
                } else {
                    cp.paper
                        .positions
                        .iter()
                        .find(|p| p.layer == pos.layer)
                        .map(|p| p.since_ms)
                };
                tracing::info!(
                    "{}｜区间 {:.4} ～ {:.4} USDC｜持仓 {} 分钟",
                    pos.layer,
                    pos.lower,
                    pos.upper,
                    now.saturating_sub(since.unwrap_or(now)) / 60000
                );
            }
            store.write("solana_monitor.json",&json!({"time_ms":now,"snapshot":s,"portfolio":p,"decision":d,"fee_apr_observation":apr,"performance":performance}))?;
            last_report = now;
        }
        crate::runtime::status(
            &store,
            "running",
            json!({"phase":cp.strategy.phase,"pool_time_ms":s.time_ms,"hedge_time_ms":ht}),
        )?;
        if once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(c.poll_seconds)).await;
    }
}

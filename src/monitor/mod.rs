pub mod denomination;
pub mod display;
pub mod performance;
// 保留旧离线统计工具的兼容入口；实时监控不再创建成交量统计或补数任务。
pub mod volume;
use crate::{
    config::{Config, Mode},
    domain::{HedgeVenue, LiquidityVenue, LpPosition, Portfolio},
    engine::Paper,
    hyperliquid::{Client, ws},
    liquidity::uniswap_v3::UniswapV3,
    store::Store,
    stream::{self, Event, Protocol},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, MissedTickBehavior},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioObservation {
    pub observed_ms: u64,
    pub mark_price: f64,
    pub portfolio: Portfolio,
    pub baseline_equity: f64,
    #[serde(default)]
    pub baseline_comparable: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize)]
struct Health {
    connected: bool,
    last_data_ms: Option<u64>,
    generation: u64,
}
impl Health {
    fn update(&mut self, e: &Event) {
        match e.channel.as_str() {
            "connected" => {
                self.connected = true;
                self.last_data_ms = None;
                self.generation = e.data["generation"].as_u64().unwrap_or(0);
            }
            "disconnected" => {
                self.connected = false;
                self.last_data_ms = None;
            }
            "pong" | "subscriptionResponse" => {}
            _ => self.last_data_ms = Some(e.received_ms),
        }
    }
}
pub fn range_position(price: f64, lower: f64, upper: f64) -> Value {
    if !price.is_finite() || upper <= lower {
        return json!({"status":"invalid"});
    }
    json!({"status":if price < lower {"below"} else if price >= upper {"above"} else {"in_range"},
        "fraction":(price-lower)/(upper-lower), "distance_to_lower_pct":(price/lower-1.0)*100.0,
        "distance_to_upper_pct":(upper/price-1.0)*100.0})
}
fn positions_report(positions: &[LpPosition], price: f64, paper: bool) -> Vec<Value> {
    positions.iter().map(|p| {
        let (base,quote) = crate::math::amounts(p.liquidity,p.lower,p.upper,price);
        json!({"layer":p.layer,"token_id":p.token_id,"lower":p.lower,"upper":p.upper,
            "price_position":range_position(price,p.lower,p.upper),"principal_value_usdg":base*price+quote,
            "unclaimed_base":p.unclaimed_base,"unclaimed_quote":p.unclaimed_quote,
            "unclaimed_fees_usdg":if paper { None } else { Some(p.unclaimed_base*price+p.unclaimed_quote) }})
    }).collect()
}
fn interval(seconds: u64) -> tokio::time::Interval {
    let mut t = tokio::time::interval(Duration::from_secs(seconds));
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}
enum Refresh {
    Hyperliquid(Result<Value>),
    Liquidity(Result<Value>),
}

/// Observe only: no signer is constructed, and no exchange/chain mutation is sent.
/// A separate timer prints cached snapshots even while a slow refresh is in flight.
pub async fn run(c: Config, store: Arc<Store>, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let venue = crate::liquidity::connect(c.liquidity.clone())?;
    let _feed_guard =
        crate::liquidity::uniswap_v3::observations::FeedGuard(venue.observations.clone());
    let (hl_tx, mut hl_rx) = mpsc::channel(4096);
    let (evm_tx, mut evm_rx) = mpsc::channel(4096);
    let mut streams = JoinSet::new();
    let hcfg = c.clone();
    let hstop = shutdown.clone();
    streams.spawn(async move {
        let user = hcfg
            .hyperliquid
            .vault
            .as_deref()
            .or(hcfg.hyperliquid.account.as_deref());
        ws::listen_with_config(
            &hcfg.hyperliquid.ws_url,
            ws::subscriptions(&hcfg.hyperliquid.coins, user),
            hcfg.websocket,
            hl_tx,
            hstop,
        )
        .await
    });
    let ecfg = c.clone();
    let estop = shutdown.clone();
    streams.spawn(async move {
        stream::listen(
            &ecfg.liquidity.ws_url,
            Protocol::Ethereum,
            vec![
                json!(["newHeads"]),
                json!(["logs",{"address":ecfg.liquidity.pool}]),
                json!(["logs",{"address":ecfg.liquidity.position_manager,"topics":[[
                    format!("{:#x}",alloy::primitives::keccak256("Transfer(address,address,uint256)")),
                    format!("{:#x}",alloy::primitives::keccak256("IncreaseLiquidity(uint256,uint128,uint256,uint256)")),
                    format!("{:#x}",alloy::primitives::keccak256("DecreaseLiquidity(uint256,uint128,uint256,uint256)")),
                    format!("{:#x}",alloy::primitives::keccak256("Collect(uint256,address,uint256,uint256)"))
                ]]}]),
            ],
            ecfg.websocket,
            evm_tx,
            estop,
        )
        .await
    });
    let mut jobs = JoinSet::new();
    let mut performance = match store
        .read::<performance::History>(performance::FILE)
        .and_then(|h| {
            let h = h.unwrap_or_default();
            h.validate()?;
            Ok(h)
        }) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error=%e,"LP APR 历史不可用，将重新积累样本；持仓状态保留");
            Default::default()
        }
    };
    let history_path = std::path::PathBuf::from(&c.state_dir);
    let matching_chain = store
        .read::<Value>("execution_identity.json")?
        .is_some_and(|v| v["chain_id"] == c.liquidity.chain_id);
    let holding_starts = match tokio::task::spawn_blocking(move || {
        if matching_chain {
            performance::Starts::load(&history_path)
        } else {
            Ok(Default::default())
        }
    })
    .await?
    {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error=%e,"旧建仓时间不可用，将按首次观察时间显示");
            Default::default()
        }
    };
    let mut htimer = interval(c.monitoring.hyperliquid_interval_seconds);
    let mut etimer = interval(c.monitoring.robinhood_interval_seconds);
    let mut hrefresh = interval(c.monitoring.hyperliquid_refresh_seconds);
    let mut erefresh = interval(c.monitoring.robinhood_refresh_seconds);
    let mut hh = Health::default();
    let mut eh = Health::default();
    let mut prices = BTreeMap::<String, Value>::new();
    let mut account = Value::Null;
    let mut chain = Value::Null;
    let mut hbusy = false;
    let mut ebusy = false;
    let mut herr: Option<String> = None;
    let mut eerr: Option<String> = None;
    let mut account_ws = Value::Null;
    let mut rpc_reported = venue.rpc.request_count();
    let mut rpc_report_time = Instant::now();
    tracing::info!(mode=?c.mode, hl_seconds=c.monitoring.hyperliquid_interval_seconds, lp_seconds=c.monitoring.robinhood_interval_seconds, "monitor started");
    let result:Result<()> = async {
        loop {
            tokio::select! {
                _=shutdown.changed()=>break,
                joined=streams.join_next(), if !streams.is_empty()=>{
                    match joined { Some(Ok(Ok(()))) if *shutdown.borrow()=>break,
                        Some(Ok(Err(e)))=>return Err(e.context("stream supervisor stopped")),
                        _=>anyhow::bail!("stream task ended unexpectedly"), }
                },
                e=hl_rx.recv()=>{if let Some(e)=e {
                    hh.update(&e);
                    match e.channel.as_str() {
                        "connected"|"disconnected"=>{prices.clear();account=Value::Null;account_ws=Value::Null;},
                        "allMids"=>{for coin in &c.hyperliquid.coins { if !e.data["mids"][coin].is_null() {
                            prices.insert(coin.clone(),json!({"price":e.data["mids"][coin],"received_ms":e.received_ms,"source":"native_ws"}));
                        }}},
                        "clearinghouseState"|"spotState"|"activeAssetData"|"openOrders"=>{
                            // Raw WS messages cannot overwrite a mode-aware REST collateral snapshot.
                            if !account_ws.is_object() {account_ws=json!({});}
                            let key=if e.channel=="activeAssetData" {format!("activeAssetData:{}",e.data["coin"].as_str().unwrap_or("unknown"))} else {e.channel.clone()};
                            account_ws[key]=json!({"observed_ms":e.received_ms,"source":"native_ws","data":e.data});
                        },
                        "orderUpdates"|"userFills"|"userFundings"=>tracing::info!(channel=%e.channel,data=%e.data,"Hyperliquid account event"),
                        _=>{},
                    }
                }},
                e=evm_rx.recv()=>{if let Some(e)=e {
                    eh.update(&e);
                    if let Err(error)=venue.observations.on_event(&e,&c.liquidity) {
                        tracing::warn!(error=%error,"WSS 观察无效；保留 RPC 核对，不使用异常推送");
                    }
                }},
                _=htimer.tick()=>{
                    let paper = if c.mode==Mode::Paper {store.read::<Paper>("paper.json")?} else {None};
                    let residual = if c.mode==Mode::Live {store.read::<Value>("hedge_residual.json")?} else {None};
                    let report=json!({"time_ms":crate::now_ms(),"mode":c.mode,"max_data_age_seconds":c.strategy.max_data_age_seconds,"ws":hh,"prices":prices,"account":account,"collateral":account["state"]["lpMakerCollateral"],"account_ws":account_ws,
                        "account_configured":hl.user().is_ok(),"account_age_ms":account["observed_ms"].as_u64().map(|t|crate::now_ms().saturating_sub(t)),"ws_data_age_ms":hh.last_data_ms.map(|t|crate::now_ms().saturating_sub(t)),"refresh_pending":hbusy,"last_refresh_error":herr,
                        "hedge_residual":residual,
                        "paper_position":paper.map(|p|json!({"coin":c.hyperliquid.hedge_coin,"short_base":p.portfolio.short_base,"equity":p.portfolio.hedge_equity,"pending_order":p.pending}))});
                    tracing::info!("\n{}",display::hyperliquid(&report));
                    tracing::debug!(report=%report,"Hyperliquid status raw snapshot");
                    if store.is_writable() {store.write("monitor_hyperliquid.json",&report)?;}
                },
                _=hrefresh.tick()=>{
                    if !hbusy {
                        hbusy=true;let client=hl.clone();let timeout=c.monitoring.refresh_timeout_seconds;
                        jobs.spawn(async move { Refresh::Hyperliquid(match tokio::time::timeout(Duration::from_secs(timeout),async {
                            if client.user().is_err() {return Ok(json!({"status":"account_not_configured"}));}
                            let state=client.account().await?;
                            Ok(json!({"observed_ms":crate::now_ms(),"source":"rest_reconciliation","state":state}))
                        }).await { Ok(v)=>v,Err(e)=>Err(e.into()) }) });
                    }
                },
                _=etimer.tick()=>{
                    let rpc_total=venue.rpc.request_count();
                    let rpc_requests=json!({"started":rpc_total.saturating_sub(rpc_reported),"interval_seconds":rpc_report_time.elapsed().as_secs_f64(),"total":rpc_total});
                    rpc_reported=rpc_total;rpc_report_time=Instant::now();
                    let mut report=json!({"market":crate::liquidity::chains::labels(&c.liquidity),"time_ms":crate::now_ms(),"mode":c.mode,"max_data_age_seconds":c.strategy.max_data_age_seconds,"ws":eh,"ws_data_age_ms":eh.last_data_ms.map(|t|crate::now_ms().saturating_sub(t)),"snapshot":chain,
                        "rpc_requests":rpc_requests,
                        "rpc_cooldown_seconds":venue.rpc.cooldown_remaining().as_secs_f64().ceil() as u64,
                        "history_rpc_cooldown_seconds":venue.archive_rpc.cooldown_remaining().as_secs_f64().ceil() as u64,
                        "runtime":store.read::<Value>("runtime_health.json")?,
                        "snapshot_age_ms":chain["observed_ms"].as_u64().map(|t|crate::now_ms().saturating_sub(t)),"last_unconfirmed_swap":venue.observations.latest_swap(),"refresh_pending":ebusy,"last_refresh_error":eerr,"volume_enabled":false});
                    denomination::normalize(&mut report, c.liquidity.chain_id == 4663);
                    tracing::info!("\n{}",display::liquidity(&report));
                    tracing::debug!(report=%report,"LP status raw snapshot");
                    if store.is_writable() {store.write(crate::liquidity::chains::monitor_file(&c.liquidity),&report)?;}
                },
                _=erefresh.tick()=>{
                    if !ebusy && venue.rpc.cooldown_remaining().is_zero() {
                        ebusy=true;let cfg=c.clone();let v=venue.clone();let s=store.clone();
                        let anchor=performance.positions.values().filter_map(|h|h.samples.back())
                            .filter(|sample|crate::now_ms().saturating_sub(sample.time_ms)<=c.strategy.max_data_age_seconds*1000)
                            .max_by_key(|sample|sample.block).map(|sample|(performance.identity.clone(),sample.block,sample.block_hash.clone()));
                        jobs.spawn(async move { Refresh::Liquidity(match tokio::time::timeout(Duration::from_secs(cfg.monitoring.refresh_timeout_seconds),
                            refresh_chain(&cfg,&v,&s,anchor)).await {Ok(v)=>v,Err(e)=>Err(e.into())}) });
                    }
                },
                r=jobs.join_next(), if !jobs.is_empty()=>{
                    match r.context("refresh task missing")?? {
                        Refresh::Hyperliquid(r)=>{hbusy=false;match r {Ok(v)=>{account=v;herr=None;},Err(e)=>{herr=Some(format!("{e:#}"));tracing::warn!(error=%e,"Hyperliquid account refresh failed; previous snapshot retained with its timestamp");}}},
                        Refresh::Liquidity(r)=>{ebusy=false;match r {Ok(mut v)=>{
                            let mut next=performance.clone();
                            match next.observe(&mut v,crate::now_ms(),c.strategy.max_data_age_seconds*1000,&holding_starts) {
                                Ok(())=>{if store.is_writable() && c.mode==Mode::Live && v["positions_observed"]==true {store.write(performance::FILE,&next)?;}performance=next;},
                                Err(e)=>{v["performance_error"]=json!(format!("{e:#}"));tracing::warn!(error=%e,"LP APR 计算暂停，等待有效快照");},
                            }
                            chain=v;eerr=None;
                        },Err(e)=>{eerr=Some(format!("{e:#}"));tracing::warn!(error=%e,"LP refresh failed; previous snapshot retained with its timestamp");}}},
                    }
                }
            }
            if *shutdown.borrow() {break;}
        }
        Ok(())
    }.await;
    jobs.abort_all();
    streams.abort_all();
    while jobs.join_next().await.is_some() {}
    while streams.join_next().await.is_some() {}
    tracing::info!("monitor stopped; sockets and refresh tasks joined");
    result
}
async fn refresh_chain(
    c: &Config,
    venue: &UniswapV3,
    store: &Store,
    previous: Option<(Value, u64, String)>,
) -> Result<Value> {
    let epoch = venue.observations.epoch();
    let identity = store.read::<Value>("execution_identity.json")?;
    let owner = c.liquidity.owner.as_deref().or_else(|| {
        identity
            .as_ref()
            .filter(|v| v["chain_id"] == c.liquidity.chain_id)
            .and_then(|v| v["owner"].as_str())
    });
    let registered = store
        .read::<BTreeMap<String, String>>("nfts.json")?
        .unwrap_or_default();
    let registered_ids: Vec<_> = registered
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let recent = if c.mode == Mode::Live {
        owner
            .map(|owner| -> Result<_> {
                let owner = owner.parse()?;
                // 至少每分钟完整枚举一次，避免仅靠 NFT 推送漏掉外部转入/转出。
                if !venue.observations.inventory_recent(owner, &registered_ids) {
                    return Ok(None);
                }
                Ok(venue.observations.positions(
                    owner,
                    &registered_ids,
                    Duration::from_secs(c.monitoring.robinhood_refresh_seconds.min(15)),
                ))
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    let snapshot = if let Some(v) = &recent {
        v.snapshot.clone()
    } else {
        venue.snapshot().await?
    };
    let observed_ms = recent.as_ref().map(|v| v.observed_ms);
    let phase = store.read::<Value>("strategy.json")?;
    let mut positions_observed = true;
    let mut accounting_revisions = BTreeMap::new();
    let mut accounting_identity = Value::Null;
    let (positions, pnl) = if c.mode == Mode::Paper {
        if let Some(mut p) = store.read::<Paper>("paper.json")? {
            for pos in &mut p.portfolio.positions {
                (pos.base, pos.quote) =
                    crate::math::amounts(pos.liquidity, pos.lower, pos.upper, snapshot.price);
            }
            let pnl = json!({"basis":"paper; LP fees and gas excluded","equity_usd":p.portfolio.equity(snapshot.price),
                "pnl_usd":p.portfolio.equity(snapshot.price)-c.strategy.total_capital,"hedge_fees_usd":p.hedge_fees,
                "swap_costs_usd":p.swap_costs,"funding_pnl_usd":p.funding_pnl,"lp_fees_usd":Value::Null});
            (p.portfolio.positions, pnl)
        } else {
            positions_observed = false;
            (
                vec![],
                json!({"status":"paper_strategy_not_started","pnl_usd":Value::Null}),
            )
        }
    } else {
        let positions = if let Some(owner) = owner {
            accounting_identity = json!({"chain_id":c.liquidity.chain_id,"pool":c.liquidity.pool.to_ascii_lowercase(),
                "manager":c.liquidity.position_manager.to_ascii_lowercase(),"owner":owner.to_ascii_lowercase(),
                "base_decimals":c.liquidity.base_decimals,"quote_decimals":c.liquidity.quote_decimals});
            let observed = if let Some(v) = recent {
                v.rows
            } else {
                let ids = venue
                    .token_ids_at(owner.parse()?, &format!("0x{:x}", snapshot.block))
                    .await?;
                let ids = ids
                    .into_iter()
                    .map(|id| {
                        let layer = registered
                            .iter()
                            .find(|(_, v)| **v == id)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| "unregistered".into());
                        (layer, id)
                    })
                    .collect::<Vec<_>>();
                venue.position_observations(owner, &ids, &snapshot).await?
            };
            observed
                .into_iter()
                .map(|(p, revision)| {
                    if let Some(id) = &p.token_id {
                        accounting_revisions.insert(id.clone(), revision);
                    }
                    p
                })
                .collect()
        } else {
            positions_observed = false;
            vec![]
        };
        let pnl = if let Some(obs) = store.read::<PortfolioObservation>("portfolio.json")? {
            json!({"basis":"live equity change, includes unclaimed LP fees; excludes gas and is NOT adjusted for deposits/withdrawals",
                "observed_ms":obs.observed_ms,"equity_usd":obs.portfolio.equity(obs.mark_price),
                "baseline_comparable":obs.baseline_comparable,
                "status":if obs.baseline_comparable==Some(false) {"accounting_basis_changed; corrected collateral is not profit"} else {"observed"},
                "equity_change_usd":if obs.baseline_comparable==Some(false) {Value::Null} else {json!(obs.portfolio.equity(obs.mark_price)-obs.baseline_equity)}})
        } else {
            json!({"status":"no_strategy_equity_baseline","equity_change_usd":Value::Null})
        };
        (positions, pnl)
    };
    let mut reports = positions_report(&positions, snapshot.price, c.mode == Mode::Paper);
    let mut reorg = false;
    if c.mode == Mode::Live && positions_observed {
        if let Some((_, block, hash)) =
            previous.filter(|(identity, _, _)| *identity == accounting_identity)
        {
            reorg = block > snapshot.block || venue.canonical_hash(block).await? != hash;
        }
        let canonical = venue.canonical_hash(snapshot.block).await?;
        anyhow::ensure!(
            canonical == snapshot.block_hash,
            "LP observation block changed during sampling"
        );
    }
    for p in &mut reports {
        if let Some(revision) = p["token_id"]
            .as_str()
            .and_then(|id| accounting_revisions.get(id))
        {
            p["accounting_revision"] = json!(revision);
        }
    }
    venue.observations.ensure_epoch(epoch)?;
    Ok(
        json!({"observed_ms":observed_ms.unwrap_or_else(crate::now_ms),"observation_source":if observed_ms.is_some(){"shared_confirmed"}else{"confirmed_read"},"mode":c.mode,"pool":snapshot,"accounting_identity":accounting_identity,"accounting_reorg":reorg,
        "positions_observed":positions_observed,"positions":reports,"returns":pnl,"strategy":phase}),
    )
}

/// RAII cancellation also covers errors leaving the strategy loop.
pub struct Supervisor {
    stop: watch::Sender<bool>,
    pub task: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl Supervisor {
    pub fn start(c: Config, store: Arc<Store>) -> Self {
        let (stop, rx) = watch::channel(false);
        Self {
            stop,
            task: Some(tokio::spawn(run(c, store, rx))),
        }
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
                Ok(r) => r??,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        Ok(())
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub async fn standalone(c: Config, store: Arc<Store>, seconds: u64) -> Result<()> {
    let mut supervisor = Supervisor::start(c, store);
    let end = Instant::now()
        + Duration::from_secs(if seconds == 0 {
            86400 * 365 * 100
        } else {
            seconds
        });
    let task = supervisor.task.as_mut().context("missing monitor task")?;
    tokio::select! {
        r=task=>{r??;return Ok(());},
        _=crate::runtime::shutdown()=>{},
        _=tokio::time::sleep_until(end)=>{},
    }
    supervisor.stop().await
}

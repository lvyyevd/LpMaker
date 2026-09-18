pub mod volume;
use crate::{
    config::{Config, Mode},
    domain::{HedgeVenue, LiquidityVenue, LpPosition, Portfolio},
    engine::Paper,
    evm::UniswapV3,
    hyperliquid::{Client, ws},
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
    Robinhood(Result<Value>),
    Volume(Result<(Value, volume::Volume)>),
}

/// Observe only: no signer is constructed, and no exchange/chain mutation is sent.
/// A separate timer prints cached snapshots even while a slow refresh is in flight.
pub async fn run(c: Config, store: Arc<Store>, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    let venue = UniswapV3::new(c.liquidity.clone())?;
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
            ],
            ecfg.websocket,
            evm_tx,
            estop,
        )
        .await
    });
    let mut jobs = JoinSet::new();
    let mut htimer = interval(c.monitoring.hyperliquid_interval_seconds);
    let mut etimer = interval(c.monitoring.robinhood_interval_seconds);
    let mut hh = Health::default();
    let mut eh = Health::default();
    let mut prices = BTreeMap::<String, Value>::new();
    let mut account = Value::Null;
    let mut chain = Value::Null;
    let mut hbusy = false;
    let mut ebusy = false;
    let mut vbusy = false;
    let mut volume_report = Value::Null;
    let mut volume_error: Option<String> = None;
    let mut herr: Option<String> = None;
    let mut eerr: Option<String> = None;
    let mut volume = match store.read::<volume::Volume>("monitor_volume.json") {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error=%e,"rebuild invalid monitoring cache from confirmed chain logs");
            volume::Volume::default()
        }
    };
    let mut last_swap = Value::Null;
    let mut account_ws = Value::Null;
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
                        "clearinghouseState"|"spotState"|"activeAssetData"=>{
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
                    if e.channel=="connected" || e.channel=="disconnected" {last_swap=Value::Null;}
                    if e.channel=="logs" {
                        match crate::evm::events::decode(&e.data) {
                            Ok(log)=>{if log["kind"]=="Swap" {last_swap=json!({"received_ms":e.received_ms,"fields":log["fields"],"block":log["blockNumber"],"transaction_hash":log["transactionHash"],"removed":log["removed"]});}
                                tracing::debug!(event=%log,"pool websocket event; confirmed volume is backfilled separately");},
                            Err(err)=>tracing::warn!(error=%err,"invalid pool websocket event"),
                        }
                    }
                }},
                _=htimer.tick()=>{
                    let paper = if c.mode==Mode::Paper {store.read::<Paper>("paper.json")?} else {None};
                    let report=json!({"time_ms":crate::now_ms(),"ws":hh,"prices":prices,"account":account,"collateral":account["state"]["lpMakerCollateral"],"account_ws":account_ws,
                        "account_configured":hl.user().is_ok(),"account_age_ms":account["observed_ms"].as_u64().map(|t|crate::now_ms().saturating_sub(t)),"ws_data_age_ms":hh.last_data_ms.map(|t|crate::now_ms().saturating_sub(t)),"refresh_pending":hbusy,"last_refresh_error":herr,
                        "paper_position":paper.map(|p|json!({"coin":c.hyperliquid.hedge_coin,"short_base":p.portfolio.short_base,"equity":p.portfolio.hedge_equity,"pending_order":p.pending}))});
                    tracing::info!(report=%report,"Hyperliquid status");
                    if store.is_writable() {store.write("monitor_hyperliquid.json",&report)?;}
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
                    let report=json!({"time_ms":crate::now_ms(),"ws":eh,"snapshot":chain,
                        "snapshot_age_ms":chain["observed_ms"].as_u64().map(|t|crate::now_ms().saturating_sub(t)),"volume_age_ms":volume_report["as_of_ms"].as_u64().map(|t|crate::now_ms().saturating_sub(t)),"last_unconfirmed_swap":last_swap,"refresh_pending":ebusy,"last_refresh_error":eerr,"recent_volume":volume_report,"volume_refresh_pending":vbusy,"volume_error":volume_error});
                    tracing::info!(report=%report,"Robinhood LP status");
                    if store.is_writable() {store.write("monitor_robinhood.json",&report)?;}
                    if !ebusy {
                        ebusy=true;let cfg=c.clone();let v=venue.clone();let s=store.clone();
                        jobs.spawn(async move { Refresh::Robinhood(match tokio::time::timeout(Duration::from_secs(cfg.monitoring.refresh_timeout_seconds),
                            refresh_chain(&cfg,&v,&s)).await {Ok(v)=>v,Err(e)=>Err(e.into())}) });
                    }
                    if !vbusy {
                        vbusy=true;let cfg=c.clone();let v=venue.clone();let mut vol=volume.clone();
                        jobs.spawn(async move {Refresh::Volume(match tokio::time::timeout(Duration::from_secs(cfg.monitoring.refresh_timeout_seconds),async {
                            let snapshot=v.snapshot().await?;
                            vol.refresh(&cfg,&v,&snapshot).await?;
                            Ok((vol.report(&snapshot,cfg.monitoring.volume_window_seconds),vol))
                        }).await {Ok(r)=>r,Err(e)=>Err(e.into())})});
                    }
                },
                r=jobs.join_next(), if !jobs.is_empty()=>{
                    match r.context("refresh task missing")?? {
                        Refresh::Hyperliquid(r)=>{hbusy=false;match r {Ok(v)=>{account=v;herr=None;},Err(e)=>{herr=Some(format!("{e:#}"));tracing::warn!(error=%e,"Hyperliquid account refresh failed; previous snapshot retained with its timestamp");}}},
                        Refresh::Robinhood(r)=>{ebusy=false;match r {Ok(v)=>{chain=v;eerr=None;},Err(e)=>{eerr=Some(format!("{e:#}"));tracing::warn!(error=%e,"Robinhood refresh failed; previous snapshot retained with its timestamp");}}},
                        Refresh::Volume(r)=>{vbusy=false;match r {Ok((v,vol))=>{volume_report=v;volume=vol;volume_error=None;if store.is_writable(){store.write("monitor_volume.json",&volume)?;}},Err(e)=>{volume_error=Some(format!("{e:#}"));tracing::warn!(error=%e,"volume refresh failed; LP price and position reporting continues");}}},
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
async fn refresh_chain(c: &Config, venue: &UniswapV3, store: &Store) -> Result<Value> {
    let snapshot = venue.snapshot().await?;
    let phase = store.read::<Value>("strategy.json")?;
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
            (
                vec![],
                json!({"status":"paper_strategy_not_started","pnl_usd":Value::Null}),
            )
        }
    } else {
        let identity = store.read::<Value>("execution_identity.json")?;
        let owner = c.liquidity.owner.as_deref().or_else(|| {
            identity
                .as_ref()
                .filter(|v| v["chain_id"] == c.liquidity.chain_id)
                .and_then(|v| v["owner"].as_str())
        });
        let positions = if let Some(owner) = owner {
            let ids = venue.token_ids(owner.parse()?).await?;
            let registered = store
                .read::<BTreeMap<String, String>>("nfts.json")?
                .unwrap_or_default();
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
            venue.positions(owner, &ids).await?
        } else {
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
    Ok(
        json!({"observed_ms":crate::now_ms(),"pool":snapshot,"positions":positions_report(&positions,snapshot.price,c.mode==Mode::Paper),
        "returns":pnl,"strategy":phase}),
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

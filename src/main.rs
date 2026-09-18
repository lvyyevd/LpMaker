use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use lp_maker::{
    config::{Config, Mode},
    domain::{Candle, HedgeVenue, LiquidityVenue, MarketFrame, PoolSnapshot},
    engine::{self, Paper},
    evm::{UniswapV3, tx::Executor},
    hyperliquid::{Client, orders},
    store::Store,
    strategy::Strategy,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "LpMaker: concentrated liquidity + Hyperliquid hedging"
)]
struct Cli {
    #[arg(long, default_value = "config/robinhood.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate local config without network or keys.
    Check,
    /// Read-only native WS monitoring with 30s Hyperliquid / 15s LP reports.
    Monitor {
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Run strategy; paper is default. Real mutations require live config AND --execute.
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        execute: bool,
        /// Assert that a legacy flat account has never opened LP; does not bypass risk checks.
        #[arg(long)]
        first_entry: bool,
    },
    /// Stream public prices/book/trades/candles; optionally account events.
    Watch {
        #[arg(long, value_delimiter = ',')]
        coins: Vec<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Hyperliquid public/account information.
    Info {
        #[command(subcommand)]
        command: InfoCommand,
    },
    /// Robinhood/Uniswap V3 pool snapshots, event logs and positions.
    Pool {
        #[command(subcommand)]
        command: PoolCommand,
    },
    /// Submit explicit perpetual actions (requires live config AND --execute).
    Trade {
        #[arg(long)]
        execute: bool,
        #[command(subcommand)]
        command: TradeCommand,
    },
    /// Explicit on-chain LP operations (requires live config AND --execute).
    Lp {
        #[arg(long)]
        execute: bool,
        #[command(subcommand)]
        command: LpCommand,
    },
    /// Resolve a persisted uncertain operation by its actual on-chain/exchange state.
    Reconcile,
    /// Inspect persisted state and unresolved operations; no key required.
    Status,
    /// Fail unless recent completed strategy decisions prove the runner is healthy.
    Health {
        #[arg(long, default_value_t = 120)]
        max_age_seconds: u64,
    },
    /// Replay canonical completed hourly Candle[] JSON; LP fees excluded.
    Replay {
        #[arg(long)]
        candles: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Subcommand)]
enum InfoCommand {
    Assets,
    Book {
        coin: String,
    },
    Account,
    Orders,
    Fills {
        #[arg(long)]
        start_ms: u64,
    },
    Candles {
        coin: String,
        #[arg(long, default_value = "1h")]
        interval: String,
        #[arg(long)]
        start_ms: u64,
        #[arg(long)]
        end_ms: Option<u64>,
    },
    Funding {
        coin: String,
        #[arg(long)]
        start_ms: u64,
        #[arg(long)]
        end_ms: Option<u64>,
    },
    OrderStatus {
        cloid: String,
    },
}
#[derive(Subcommand)]
enum PoolCommand {
    Snapshot,
    Watch {
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    Positions {
        owner: String,
        #[arg(long, value_delimiter = ',')]
        ids: Vec<String>,
    },
}
#[derive(Subcommand)]
enum TradeCommand {
    Limit {
        coin: String,
        #[arg(long)]
        buy: bool,
        #[arg(long)]
        price: String,
        #[arg(long)]
        size: String,
        #[arg(long, default_value = "Alo")]
        tif: String,
        #[arg(long)]
        reduce_only: bool,
    },
    Close {
        coin: String,
    },
    Cancel {
        coin: String,
        #[arg(long)]
        oid: Option<u64>,
        #[arg(long)]
        cloid: Option<String>,
    },
    CancelAll,
    Modify {
        coin: String,
        oid: u64,
        #[arg(long)]
        buy: bool,
        #[arg(long)]
        price: String,
        #[arg(long)]
        size: String,
        #[arg(long)]
        reduce_only: bool,
    },
    Leverage {
        coin: String,
        #[arg(long, default_value_t = 3)]
        leverage: u32,
        #[arg(long)]
        cross: bool,
    },
    Margin {
        coin: String,
        #[arg(long, allow_hyphen_values = true)]
        usdc_micros: i64,
    },
    Trigger {
        coin: String,
        #[arg(long)]
        buy: bool,
        #[arg(long)]
        price: String,
        #[arg(long)]
        size: String,
        #[arg(long)]
        trigger: String,
        #[arg(long)]
        take_profit: bool,
    },
    ScheduleCancel {
        #[arg(long)]
        after_seconds: u64,
    },
}
#[derive(Subcommand)]
enum LpCommand {
    /// Recover only a pending ERC20 approval, with the same nonce/calldata and higher bounded fees.
    RetryApproval {
        /// Original transaction hash recorded in pending.json.
        #[arg(long)]
        hash: String,
    },
    Mint {
        layer: String,
        #[arg(long)]
        value: f64,
        #[arg(long)]
        width: f64,
    },
    Remove {
        layer: String,
    },
    Collect {
        layer: String,
    },
    Increase {
        layer: String,
        #[arg(long)]
        value: f64,
    },
    Swap {
        #[arg(long)]
        sell_base: bool,
        #[arg(long)]
        amount: f64,
    },
    /// Register an existing owned NFT for a configured strategy layer.
    Import {
        layer: String,
        token_id: String,
    },
}
fn print(v: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}
fn live(c: &Config, execute: bool) -> Result<()> {
    ensure!(
        c.mode == Mode::Live && execute,
        "mutations require mode='live' AND --execute"
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = Config::load(cli.config)?;
    let _logging = lp_maker::logging::init(&c.logging)?;
    if matches!(cli.command, Command::Check) {
        return print(
            json!({"status":"valid","mode":c.mode,"chain_id":c.liquidity.chain_id,"pool":c.liquidity.pool}),
        );
    }
    let result: Result<()> = async {
    let read_only = matches!(
        &cli.command,
        Command::Status
            | Command::Health { .. }
            | Command::Monitor { .. }
            | Command::Info { .. }
            | Command::Watch { .. }
            | Command::Replay { .. }
            | Command::Pool {
                command: PoolCommand::Snapshot | PoolCommand::Positions { .. } | PoolCommand::Watch { .. }
            }
    );
    let store = Arc::new(if read_only {
        Store::readonly(&c.state_dir)
    } else {
        Store::open_with_policy(&c.state_dir, c.storage.clone())?
    });
    let mut hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    match cli.command {
        Command::Check => unreachable!(),
        Command::Health { max_age_seconds } => print(lp_maker::runtime::check_health(&store, max_age_seconds)?),
        Command::Monitor { seconds } => lp_maker::monitor::standalone(c, store, seconds).await,
        Command::Run { once, execute, first_entry } => engine::run(c, store, once, execute, first_entry).await,
        Command::Status => {
            let mut pending = store.pending()?;
            if let Some(p) = pending.as_mut() {
                lp_maker::store::redact_signatures(p);
            }
            print(json!({"checkpoint":store.read::<Value>("checkpoint.json")?,
                "strategy":store.read::<Value>("strategy.json")?,"paper":store.read::<Value>("paper.json")?,
                "nfts":store.read::<Value>("nfts.json")?,"pending":pending,"workflow":store.read::<Value>("workflow.json")?,
                "orders":store.read::<Value>("orders.json")?,"open_orders":store.read::<Value>("open_orders.json")?,
                "live_inventory":store.read::<Value>("live_inventory.json")?,"lp_inventory":store.read::<Value>("lp_inventory.json")?,
                "nonce":store.read::<Value>("evm_nonce.json")?,"startup_reconciliation":store.read::<Value>("startup_reconciliation.json")?,
                "runtime_health":store.read::<Value>("runtime_health.json")?,"hedge_residual":store.read::<Value>("hedge_residual.json")?,
                "hl_identity":store.read::<Value>("hl_identity.json")?}))
        },
        Command::Reconcile => print(engine::reconcile(&c, store).await?),
        Command::Info { command } => {
            let result = match command {
                InfoCommand::Assets => serde_json::to_value(hl.assets().await?)?,
                InfoCommand::Book { coin } => hl.info(json!({"type":"l2Book","coin":coin})).await?,
                InfoCommand::Account => hl.account().await?,
                InfoCommand::Orders => hl.open_orders().await?,
                InfoCommand::Fills { start_ms } => hl.fills(start_ms).await?,
                InfoCommand::Candles {
                    coin,
                    interval,
                    start_ms,
                    end_ms,
                } => serde_json::to_value(
                    hl.candles(
                        &coin,
                        &interval,
                        start_ms,
                        end_ms.unwrap_or_else(lp_maker::now_ms),
                    )
                    .await?,
                )?,
                InfoCommand::Funding {
                    coin,
                    start_ms,
                    end_ms,
                } => {
                    hl.funding_history(&coin, start_ms, end_ms.unwrap_or_else(lp_maker::now_ms))
                        .await?
                }
                InfoCommand::OrderStatus { cloid } => hl.order_status(json!(cloid)).await?,
            };
            print(result)
        }
        Command::Watch {
            coins,
            user,
            seconds,
        } => {
            let coins = if coins.is_empty() {
                c.hyperliquid.coins.clone()
            } else {
                coins
            };
            let user = user
                .or(c.hyperliquid.vault.clone())
                .or(c.hyperliquid.account.clone());
            let subs = lp_maker::hyperliquid::ws::subscriptions(&coins, user.as_deref());
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let url = c.hyperliquid.ws_url.clone();
            let ws_config = c.websocket.clone();
            let task = tokio::spawn(async move {
                lp_maker::hyperliquid::ws::listen_with_config(&url, subs, ws_config, tx, stop_rx)
                    .await
            });
            let end = tokio::time::sleep(Duration::from_secs(if seconds == 0 {
                u64::MAX / 1000
            } else {
                seconds
            }));
            tokio::pin!(end);
            let mut report_tick = tokio::time::interval(Duration::from_secs(
                c.monitoring.hyperliquid_interval_seconds,
            ));
            report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut latest_prices = Value::Null;
            let mut latest_positions = Value::Null;
            let mut latest_spot = Value::Null;
            let mut latest_capacity = json!({});
            loop {
                tokio::select! {
                    e=rx.recv()=>match e{Some(e)=>{
                        if e.channel=="allMids" { latest_prices=json!({"observed_ms":e.received_ms,"mids":e.data["mids"]}); }
                        if e.channel=="clearinghouseState" { latest_positions=json!({"observed_ms":e.received_ms,"data":e.data}); }
                        if e.channel=="spotState" {latest_spot=json!({"observed_ms":e.received_ms,"data":e.data});}
                        if e.channel=="activeAssetData" {let coin=e.data["coin"].as_str().unwrap_or("unknown").to_string();latest_capacity[coin]=json!({"observed_ms":e.received_ms,"data":e.data});}
                        if e.channel=="connected" || e.channel=="disconnected" {latest_prices=Value::Null;latest_positions=Value::Null;latest_spot=Value::Null;latest_capacity=json!({});}
                        println!("{}",serde_json::to_string(&e)?);
                    },None=>break},
                    _=report_tick.tick()=>tracing::info!(prices=%latest_prices,positions=%latest_positions,spot_state=%latest_spot,available_to_trade=%latest_capacity,"Hyperliquid status"),
                    _=&mut end=>break,_=lp_maker::runtime::shutdown()=>break
                }
            }
            stop_tx.send(true)?;
            task.await??;
            Ok(())
        }
        Command::Pool { command } => {
            let venue = UniswapV3::new(c.liquidity.clone())?;
            venue.validate().await?;
            match command {
                PoolCommand::Snapshot => print(venue.snapshot().await?),
                PoolCommand::Positions { owner, ids } => {
                    let ids = if ids.is_empty() {
                        venue.token_ids(owner.parse()?).await?
                    } else {
                        ids
                    };
                    let ids = ids
                        .into_iter()
                        .map(|id| (id.clone(), id))
                        .collect::<Vec<_>>();
                    print(venue.positions(&owner, &ids).await?)
                }
                PoolCommand::Watch { seconds } => {
                    lp_maker::monitor::standalone(c, store, seconds).await
                }
            }
        }
        Command::Trade { execute, command } => {
            live(&c, execute)?;
            hl.enable_signing().await?;
            let result = match command {
                TradeCommand::Limit {
                    coin,
                    buy,
                    price,
                    size,
                    tif,
                    reduce_only,
                } => {
                    hl.order(
                        &coin,
                        buy,
                        &price,
                        &size,
                        &tif,
                        reduce_only,
                        &orders::cloid(),
                    )
                    .await?
                }
                TradeCommand::Cancel { coin, oid, cloid } => {
                    ensure!(
                        oid.is_some() != cloid.is_some(),
                        "specify exactly one of --oid/--cloid"
                    );
                    if let Some(id) = oid {
                        hl.cancel_oid(&coin, id).await?
                    } else {
                        hl.cancel(&coin, &cloid.unwrap()).await?
                    }
                }
                TradeCommand::CancelAll => serde_json::to_value(hl.cancel_all().await?)?,
                TradeCommand::Modify {
                    coin,
                    oid,
                    buy,
                    price,
                    size,
                    reduce_only,
                } => {
                    hl.modify(&coin, oid, buy, &price, &size, reduce_only)
                        .await?
                }
                TradeCommand::Leverage {
                    coin,
                    leverage,
                    cross,
                } => hl.leverage(&coin, leverage, cross).await?,
                TradeCommand::Margin { coin, usdc_micros } => {
                    hl.isolated_margin(&coin, usdc_micros).await?
                }
                TradeCommand::Trigger {
                    coin,
                    buy,
                    price,
                    size,
                    trigger,
                    take_profit,
                } => {
                    hl.trigger(&coin, buy, &price, &size, &trigger, take_profit)
                        .await?
                }
                TradeCommand::ScheduleCancel { after_seconds } => {
                    hl.schedule_cancel(if after_seconds == 0 {
                        None
                    } else {
                        Some(lp_maker::now_ms() + after_seconds * 1000)
                    })
                    .await?
                }
                TradeCommand::Close { coin } => {
                    let size = lp_maker::hyperliquid::position_size(&hl.account().await?, &coin)?;
                    ensure!(size != 0.0, "no open position");
                    let buy = size < 0.0;
                    let (_, asset) = hl.asset(&coin).await?;
                    let (bid, ask, t) = hl.book(&coin).await?;
                    ensure!(
                        lp_maker::now_ms().saturating_sub(t)
                            < c.strategy.max_data_age_seconds * 1000,
                        "stale book"
                    );
                    let slip = c.hyperliquid.emergency_slippage_bps as f64 / 10000.0;
                    let px = orders::price(
                        if buy {
                            ask * (1.0 + slip)
                        } else {
                            bid * (1.0 - slip)
                        },
                        asset.sz_decimals,
                        buy,
                    )?;
                    hl.order(
                        &coin,
                        buy,
                        &px,
                        &orders::quantity(size.abs(), asset.sz_decimals)?,
                        "Ioc",
                        true,
                        &orders::cloid(),
                    )
                    .await?
                }
            };
            print(result)
        }
        Command::Lp { execute, command } => {
            live(&c, execute)?;
            let venue = UniswapV3::new(c.liquidity.clone())?;
            venue.validate().await?;
            let ex = Executor::new(venue, store.clone())?;
            let result = match command {
                LpCommand::RetryApproval { hash } => ex.retry_approval(&hash).await?,
                LpCommand::Mint {
                    layer,
                    value,
                    width,
                } => {
                    ensure!(
                        value <= c.strategy.lp_budget && width > 0.001 && width < 1.0,
                        "mint outside configured budget/range limits"
                    );
                    ex.mint(&layer, value, width).await?
                }
                LpCommand::Swap { sell_base, amount } => ex.swap(sell_base, amount).await?,
                LpCommand::Import { layer, token_id } => {
                    ensure!(
                        c.strategy.layers.iter().any(|l| l.name == layer),
                        "unknown strategy layer"
                    );
                    let mut ids = store
                        .read::<BTreeMap<String, String>>("nfts.json")?
                        .unwrap_or_default();
                    ensure!(
                        !ids.contains_key(&layer) && !ids.values().any(|id| id == &token_id),
                        "layer/NFT already imported"
                    );
                    ex.venue
                        .positions(
                            &ex.owner().to_string(),
                            &[(layer.clone(), token_id.clone())],
                        )
                        .await?;
                    lp_maker::recovery::record_lp_history(&store, json!({"source":"import","token_id":token_id}))?;
                    ids.insert(layer, token_id);
                    store.write("nfts.json", &ids)?;
                    json!({"status":"imported","nfts":ids})
                }
                cmd => {
                    let layer = match &cmd {
                        LpCommand::Remove { layer }
                        | LpCommand::Collect { layer }
                        | LpCommand::Increase { layer, .. } => layer,
                        _ => unreachable!(),
                    };
                    let ids = ex
                        .ids()?
                        .into_iter()
                        .filter(|(l, _)| l == layer)
                        .collect::<Vec<_>>();
                    ensure!(ids.len() == 1, "unknown LP layer");
                    if matches!(cmd, LpCommand::Collect { .. }) {
                        ex.collect(&ids[0].1).await?
                    } else {
                        let p = ex.venue.positions(&ex.owner().to_string(), &ids).await?;
                        if let LpCommand::Increase { value, .. } = cmd {
                            ensure!(
                                value > 0.0 && value <= c.strategy.lp_budget,
                                "increase outside configured budget"
                            );
                            ex.increase(&p[0], value).await?
                        } else {
                            ex.remove(&p[0]).await?
                        }
                    }
                }
            };
            print(result)
        }
        Command::Replay { candles, output } => {
            ensure!(c.mode == Mode::Paper, "replay requires paper mode");
            let candles: Vec<Candle> = serde_json::from_slice(&std::fs::read(candles)?)?;
            let mut strategy = Strategy::default();
            let mut paper = Paper::new(&c);
            let mut curve = vec![];
            for (i, bar) in candles.iter().enumerate() {
                ensure!(
                    bar.close > 0.0
                        && bar.close.is_finite()
                        && (i == 0 || bar.open_ms == candles[i - 1].open_ms + 3_600_000),
                    "replay candles must be positive, sorted, contiguous hourly bars"
                );
                let now = bar.close_ms + 1;
                paper.mark(bar.close, bar.close, now, &c);
                let frame = MarketFrame {
                    now_ms: now,
                    pool: PoolSnapshot {
                        block: i as u64,
                        block_hash: "replay".into(),
                        time_ms: now,
                        price: bar.close,
                        tick: 0,
                        tick_spacing: 1,
                        liquidity: "0".into(),
                        sqrt_price_x96: "0".into(),
                        base_is_token0: true,
                    },
                    hedge_price: bar.close,
                    hedge_time_ms: now,
                    candles: candles[..=i].to_vec(),
                    portfolio: paper.portfolio.clone(),
                };
                let d = strategy.evaluate(&c.strategy, &frame);
                paper.apply(&d, &c, bar.close, bar.close, now)?;
                curve.push(json!({"time_ms":now,"price":bar.close,"equity":paper.portfolio.equity(bar.close),"decision":d,"short_base":paper.portfolio.short_base}));
            }
            let report = json!({"assumptions":["hourly close-only illustrative replay; not execution-quality backtest","LP fees excluded; no liquidation engine; hourly sampling cannot confirm consecutive 15m breakout observations","perp price equals spot; historical funding excluded","maker fills require later sampled price crossing; no queue model","EVM gas and temporary workflow hedges excluded; swap fee/slippage and regular hedge fees included"],"portfolio":paper,"curve":curve});
            std::fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
            print(json!({"output":output,"rows":candles.len()}))
        }
    }
    }.await;
    if let Err(error) = &result {
        tracing::error!(error=%format!("{error:#}"), "command failed; inspect persisted state before retrying a mutation");
    }
    result
}

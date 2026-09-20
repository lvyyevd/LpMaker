//! 请求数量与 WSS 缓存生命周期回归；只连接本机模拟节点。
mod support;
use alloy::primitives::{Address, U256, keccak256};
use lp_maker::{
    config::Config,
    domain::LiquidityVenue,
    evm::rpc::{address_word, tick_word},
    liquidity::uniswap_v3::{
        UniswapV3,
        observations::{FeedGuard, Shared},
    },
    stream::Event,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::rpc::Mock;

const OWNER: &str = "0x0000000000000000000000000000000000000001";
fn cfg() -> lp_maker::config::LiquidityConfig {
    Config::load("config/paper-200.toml").unwrap().liquidity
}
fn hash(number: u64) -> String {
    format!("0x{number:064x}")
}
fn event(channel: &str, data: Value) -> Event {
    Event {
        channel: channel.into(),
        data,
        received_ms: lp_maker::now_ms(),
    }
}
fn head(number: u64, timestamp: u64) -> Event {
    event(
        "newHeads",
        json!({"number":format!("0x{number:x}"),"hash":hash(number),"parentHash":hash(number-1),"timestamp":format!("0x{timestamp:x}")}),
    )
}
fn connect(shared: &Shared, c: &lp_maker::config::LiquidityConfig, timestamp: u64) {
    shared.on_event(&event("connected", json!({})), c).unwrap();
    shared
        .on_event(
            &event("subscriptionResponse", json!({"type":"newHeads"})),
            c,
        )
        .unwrap();
    for n in 100..=102 {
        shared.on_event(&head(n, timestamp + n - 100), c).unwrap();
    }
}
fn encoded(words: &[U256]) -> Value {
    json!(format!(
        "0x{}",
        words
            .iter()
            .map(|w| format!("{w:064x}"))
            .collect::<String>()
    ))
}
async fn fixture() -> (Mock, UniswapV3, u64) {
    let mut c = cfg();
    c.confirmations = 2;
    c.rpc_min_interval_ms = 1;
    let copy = c.clone();
    let time = lp_maker::now_ms() / 1000 - 10;
    let mock = Mock::start(move |request| match request["method"].as_str().unwrap() {
        "eth_blockNumber" => Ok(json!("0x66")),
        "eth_getBlockByNumber" => {
            let n = lp_maker::evm::rpc::hex_u64(&request["params"][0]).unwrap();
            Ok(json!({"hash":hash(n),"timestamp":format!("0x{:x}",time+n-100)}))
        }
        "eth_call" => {
            let data = request["params"][0]["data"].as_str().unwrap();
            let is =
                |name: &str| data.starts_with(&format!("0x{}", hex::encode(&keccak256(name)[..4])));
            let words = if is("slot0()") {
                vec![
                    U256::from_str_radix("3949302602178501257607166", 10).unwrap(),
                    tick_word(-198141),
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::from(1),
                ]
            } else if is("liquidity()") {
                vec![U256::from(1_000_000_000_000u64)]
            } else if is("tickSpacing()") {
                vec![U256::from(1)]
            } else if is("ownerOf(uint256)") {
                vec![address_word(OWNER.parse().unwrap())]
            } else if is("positions(uint256)") {
                let id = U256::from_str_radix(&data[10..], 16).unwrap();
                let shift = if id == U256::from(1) { 0 } else { 100 };
                vec![
                    U256::ZERO,
                    U256::ZERO,
                    address_word(copy.base_token.parse().unwrap()),
                    address_word(copy.quote_token.parse().unwrap()),
                    U256::from(copy.fee),
                    tick_word(-200000 + shift),
                    tick_word(-190000 - shift),
                    U256::from(1_000_000_000u64),
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                ]
            } else if is("ticks(int24)") {
                vec![U256::ZERO; 8]
            } else if is("feeGrowthGlobal0X128()") || is("feeGrowthGlobal1X128()") {
                vec![U256::from(1) << 128]
            } else if is("balanceOf(address)") {
                vec![U256::from(100)]
            } else {
                panic!("unexpected call {data}")
            };
            Ok(encoded(&words))
        }
        other => panic!("unexpected RPC {other}"),
    })
    .await;
    c.rpc_url = mock.url.clone();
    let venue = UniswapV3::new(c).unwrap();
    (mock, venue, time)
}

#[tokio::test]
async fn established_feed_and_shared_snapshots_reduce_pool_reads_without_changing_values() {
    let (mock, venue, time) = fixture().await;
    connect(&venue.observations, &venue.cfg, time);
    let other = UniswapV3::new(venue.cfg.clone()).unwrap();
    let (a, b) = tokio::join!(venue.snapshot(), other.snapshot());
    let a = a.unwrap();
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(b.unwrap()).unwrap()
    );
    assert_eq!(a.block, 100);
    assert_eq!(mock.requests.lock().unwrap().len(), 4); // 首次 RPC 锚定 + slot0 + liquidity + immutable spacing
    assert!(
        !mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "eth_blockNumber")
    );
    venue.observations.invalidate();
    let before = mock.requests.lock().unwrap().len();
    let next = venue.snapshot().await.unwrap();
    assert_eq!(next.price, a.price);
    assert_eq!(mock.requests.lock().unwrap().len() - before, 2); // 正常刷新只读 slot0/liquidity

    let before = mock.requests.lock().unwrap().len();
    venue.execution_snapshot().await.unwrap();
    venue.execution_snapshot().await.unwrap();
    assert_eq!(mock.requests.lock().unwrap().len() - before, 8); // 执行报价每次强制 RPC，不复用观察缓存
    let before = mock.requests.lock().unwrap().len();
    venue.fresh_snapshot().await.unwrap();
    venue.fresh_snapshot().await.unwrap();
    assert_eq!(mock.requests.lock().unwrap().len() - before, 8); // 兑换/流程预检同样绕过缓存，保留原确认数
    assert_eq!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "eth_blockNumber")
            .count(),
        4
    );
}

#[tokio::test]
async fn same_block_position_reads_are_deduplicated_but_wallet_balances_are_always_fresh() {
    let (mock, venue, time) = fixture().await;
    connect(&venue.observations, &venue.cfg, time);
    let snapshot = venue.snapshot().await.unwrap();
    let ids = vec![
        ("core".into(), "1".into()),
        ("satellite".into(), "2".into()),
    ];
    let before = mock.requests.lock().unwrap().len();
    let other = UniswapV3::new(venue.cfg.clone()).unwrap();
    let (a, b) = tokio::join!(
        venue.position_observations(OWNER, &ids, &snapshot),
        other.position_observations(OWNER, &ids, &snapshot)
    );
    let a = a.unwrap();
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(b.unwrap()).unwrap()
    );
    assert_eq!(mock.requests.lock().unwrap().len() - before, 10); // 两个 NFT 共享两个全局手续费累计值
    let observed = venue
        .observations
        .positions(OWNER.parse().unwrap(), &ids, Duration::from_secs(15))
        .unwrap();
    assert_eq!(observed.snapshot.block, snapshot.block);
    assert_eq!(
        serde_json::to_value(observed.rows).unwrap(),
        serde_json::to_value(a).unwrap()
    );
    assert!(
        venue
            .observations
            .positions(Address::ZERO, &ids, Duration::from_secs(15))
            .is_none()
    );

    let before = mock.requests.lock().unwrap().len();
    venue.wallet(OWNER.parse().unwrap()).await.unwrap();
    venue.wallet(OWNER.parse().unwrap()).await.unwrap();
    assert_eq!(mock.requests.lock().unwrap().len() - before, 4);
    venue.observations.invalidate();
    assert!(
        venue
            .observations
            .positions(OWNER.parse().unwrap(), &ids, Duration::from_secs(15))
            .is_none()
    );
    let before = mock.requests.lock().unwrap().len();
    venue
        .position_observations(OWNER, &ids, &snapshot)
        .await
        .unwrap();
    assert_eq!(mock.requests.lock().unwrap().len() - before, 10);
}

#[tokio::test]
async fn disconnected_feed_falls_back_to_rpc_and_reorg_requires_new_anchor() {
    let (mock, venue, time) = fixture().await;
    connect(&venue.observations, &venue.cfg, time);
    venue.snapshot().await.unwrap();
    let before = mock.requests.lock().unwrap().len();
    venue.observations.disconnect();
    venue.snapshot().await.unwrap();
    assert_eq!(mock.requests.lock().unwrap().len() - before, 4);
    assert!(
        mock.requests.lock().unwrap()[before..]
            .iter()
            .any(|r| r["method"] == "eth_blockNumber")
    );
    connect(&venue.observations, &venue.cfg, time);
    let epoch = venue.observations.epoch();
    let mut fork = head(102, time + 2);
    fork.data["hash"] = json!(hash(999));
    venue.observations.on_event(&fork, &venue.cfg).unwrap();
    assert_ne!(epoch, venue.observations.epoch());
    assert!(venue.observations.confirmed_header(2).is_none());
    assert!(venue.observations.canonical_hash(100).is_none());
    drop(FeedGuard(venue.observations.clone()));
    assert!(venue.observations.confirmed_header(0).is_none());
}

#[tokio::test(start_paused = true)]
async fn stale_ws_and_old_generation_cannot_refresh_cache_or_hide_observation_age() {
    let mut c = cfg();
    c.ws_url = "ws://localhost/stale-fixture".into();
    let shared = Shared::for_pool(&c);
    connect(&shared, &c, lp_maker::now_ms() / 1000 - 10);
    let epoch = shared.epoch();
    shared.anchored(epoch);
    let owner = OWNER.parse().unwrap();
    assert!(!shared.inventory_recent(owner, &[("core".into(), "1".into())]));
    shared.inventory_checked(epoch, owner, &["1".into()]);
    assert!(shared.inventory_recent(owner, &[("core".into(), "1".into())]));
    assert!(!shared.inventory_recent(owner, &[("core".into(), "2".into())]));
    shared.save_words(epoch, "key".into(), vec![U256::from(1)]);
    assert!(shared.confirmed_header(2).is_some());
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(shared.confirmed_header(2).is_none());
    assert!(shared.canonical_hash(100).is_none());
    assert!(shared.words("key").is_none());
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(!shared.inventory_recent(owner, &[("core".into(), "1".into())]));
    shared.inventory_checked(epoch, owner, &["1".into()]);
    assert!(shared.inventory_recent(owner, &[("core".into(), "1".into())]));
    shared.invalidate();
    shared.inventory_checked(epoch, owner, &["1".into()]);
    assert!(!shared.inventory_recent(owner, &[("core".into(), "1".into())]));
    shared.save_words(epoch, "old".into(), vec![U256::from(1)]);
    assert!(shared.words("old").is_none());
    assert!(lp_maker::runtime::retryable(
        &shared.ensure_epoch(epoch).unwrap_err()
    ));
}

#[test]
fn cache_identity_isolated_by_chain_pool_and_transport() {
    let c = cfg();
    let shared = Shared::for_pool(&c);
    assert!(Arc::ptr_eq(&shared, &Shared::for_pool(&c)));
    for field in ["chain", "pool", "rpc", "ws"] {
        let mut other = c.clone();
        match field {
            "chain" => other.chain_id += 1,
            "pool" => other.pool = Address::ZERO.to_string(),
            "rpc" => other.rpc_url.push_str("/different"),
            _ => other.ws_url.push_str("/different"),
        }
        assert!(!Arc::ptr_eq(&shared, &Shared::for_pool(&other)));
    }
}

#[test]
fn own_nft_transfer_invalidates_observations_but_unrelated_transfer_does_not() {
    let mut c = cfg();
    c.ws_url = "ws://localhost/nft-fixture".into();
    let shared = Shared::for_pool(&c);
    shared.watch_owner(OWNER.parse().unwrap());
    let epoch = shared.epoch();
    let transfer = |to: u64| {
        event(
            "logs",
            json!({"address":c.position_manager,"topics":[
        format!("{:#x}",keccak256("Transfer(address,address,uint256)")),hash(0),hash(to),hash(9)],"removed":false}),
        )
    };
    shared.on_event(&transfer(2), &c).unwrap();
    assert_eq!(shared.epoch(), epoch);
    shared.on_event(&transfer(1), &c).unwrap();
    assert_ne!(shared.epoch(), epoch);
}

fn swap(c: &lp_maker::config::LiquidityConfig, number: u64, index: u64) -> Event {
    event(
        "logs",
        json!({"address":c.pool,"topics":[format!("{:#x}",keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"))],
        "data":encoded(&[U256::from(1),U256::MAX,U256::from_str_radix("3949302602178501257607166",10).unwrap(),U256::from(1000),tick_word(-198141)]),
        "blockNumber":format!("0x{number:x}"),"blockHash":hash(number),"logIndex":format!("0x{index:x}"),"transactionHash":hash(index+200),"removed":false}),
    )
}

#[tokio::test]
async fn swap_push_is_display_only_and_does_not_invent_confirmed_fees() {
    let (_mock, venue, time) = fixture().await;
    connect(&venue.observations, &venue.cfg, time);
    let snapshot = venue.snapshot().await.unwrap();
    let ids = vec![("core".into(), "1".into())];
    let rows = venue
        .position_observations(OWNER, &ids, &snapshot)
        .await
        .unwrap();
    venue
        .observations
        .on_event(&swap(&venue.cfg, 102, 2), &venue.cfg)
        .unwrap();
    let pushed = venue.observations.latest_swap().unwrap();
    assert!(pushed["price"].as_f64().unwrap() > 0.0);
    assert_eq!(pushed["block_number"], 102);
    let cached = venue
        .observations
        .positions(OWNER.parse().unwrap(), &ids, Duration::from_secs(15))
        .unwrap();
    assert_eq!(cached.snapshot.block, 100);
    assert_eq!(
        serde_json::to_value(cached.rows).unwrap(),
        serde_json::to_value(rows).unwrap()
    );
    venue
        .observations
        .on_event(&swap(&venue.cfg, 101, 99), &venue.cfg)
        .unwrap();
    assert_eq!(venue.observations.latest_swap().unwrap(), pushed);
    let mut removed = swap(&venue.cfg, 102, 2);
    removed.data["removed"] = json!(true);
    venue.observations.on_event(&removed, &venue.cfg).unwrap();
    assert!(venue.observations.latest_swap().is_none());
    assert!(
        venue
            .observations
            .positions(OWNER.parse().unwrap(), &ids, Duration::from_secs(15))
            .is_none()
    );
}

#[tokio::test]
async fn mismatched_ws_chain_never_reaches_price_reads_and_requires_reconciliation() {
    let (mock, venue, time) = fixture().await;
    let shared = &venue.observations;
    shared
        .on_event(&event("connected", json!({})), &venue.cfg)
        .unwrap();
    shared
        .on_event(
            &event("subscriptionResponse", json!({"type":"newHeads"})),
            &venue.cfg,
        )
        .unwrap();
    for n in 100..=102 {
        let mut h = head(n, time + n - 100);
        h.data["hash"] = json!(hash(n + 1000));
        h.data["parentHash"] = json!(hash(n + 999));
        shared.on_event(&h, &venue.cfg).unwrap();
    }
    let error = venue.snapshot().await.unwrap_err();
    assert!(lp_maker::runtime::retryable(&error));
    assert!(shared.snapshot().is_none());
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    assert_eq!(venue.snapshot().await.unwrap().block, 100); // 丢弃不匹配的推送后从 RPC 重读
}

#[tokio::test]
async fn monitor_reuses_one_ws_connection_and_never_queries_volume_archive() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, sync::watch};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let (rpc, venue, time) = fixture().await;
    let archive = Mock::start(|_| panic!("volume/archive RPC must stay unused")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    let sub_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = sub_requests.clone();
    let pool = venue.cfg.clone();
    let stream = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        for _ in 0..3 {
            let Some(Ok(Message::Text(text))) = socket.next().await else {
                panic!("missing subscription")
            };
            let req: Value = serde_json::from_str(&text).unwrap();
            seen.lock().unwrap().push(req.clone());
            socket
                .send(Message::Text(
                    json!({"jsonrpc":"2.0","id":req["id"],"result":format!("sub{}",req["id"])})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        for n in 100..=102 {
            socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"sub1","result":head(n,time+n-100).data}}).to_string().into())).await.unwrap();
        }
        socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"sub2","result":swap(&pool,102,1).data}}).to_string().into())).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Ping(p) = message {
                let _ = socket.send(Message::Pong(p)).await;
            }
        }
    });
    let hl_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hl_url = format!("ws://{}", hl_listener.local_addr().unwrap());
    let hl_stream = tokio::spawn(async move {
        let (socket, _) = hl_listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let req: Value = serde_json::from_str(&text).unwrap();
            let reply = if req["method"] == "ping" {
                json!({"channel":"pong"})
            } else {
                json!({"channel":"subscriptionResponse","data":req})
            };
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(lp_maker::store::Store::open(dir.path()).unwrap());
    let mut c = Config::load("config/paper-200.toml").unwrap();
    c.state_dir = dir.path().to_string_lossy().into();
    c.liquidity = venue.cfg.clone();
    c.liquidity.ws_url = ws_url;
    c.liquidity.archive_rpc_url = Some(archive.url.clone());
    c.hyperliquid.account = None;
    c.hyperliquid.vault = None;
    c.hyperliquid.coins.clear();
    c.hyperliquid.ws_url = hl_url;
    c.hyperliquid.http_url = "http://127.0.0.1:1/never-used".into();
    c.monitoring.robinhood_interval_seconds = 1;
    c.monitoring.robinhood_refresh_seconds = 1;
    c.monitoring.volume_refresh_seconds = 1; // 旧配置无论填多快，都不能重新启用成交量后台任务。
    let cache = Shared::for_pool(&c.liquidity);
    let (stop, rx) = watch::channel(false);
    let s = store.clone();
    let task = tokio::spawn(async move { lp_maker::monitor::run(c, s, rx).await });
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if store
                .read::<Value>("monitor_robinhood.json")
                .unwrap()
                .is_some_and(|v| {
                    v["last_unconfirmed_swap"]["price"].is_number()
                        && v["snapshot"]["pool"]["block"] == 100
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(sub_requests.lock().unwrap().len(), 3);
    assert!(archive.requests.lock().unwrap().is_empty());
    assert!(
        store
            .read::<Value>("monitor_volume.json")
            .unwrap()
            .is_none()
    );
    let report = store
        .read::<Value>("monitor_robinhood.json")
        .unwrap()
        .unwrap();
    assert_eq!(report["volume_enabled"], false);
    assert!(report.get("recent_volume").is_none());
    assert!(report["rpc_requests"]["total"].as_u64().unwrap() > 0);
    let display = lp_maker::monitor::display::liquidity(&report);
    assert!(display.contains("WSS 实时 ETH"));
    assert!(!display.contains("成交量"));
    assert!(cache.confirmed_header(2).is_none()); // task 退出即清除 WSS 观察
    assert!(
        !rpc.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "eth_getLogs")
    );
    stream.abort();
    hl_stream.abort();
}

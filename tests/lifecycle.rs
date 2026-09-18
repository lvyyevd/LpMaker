use alloy::{
    consensus::{Transaction, TxEnvelope},
    eips::eip2718::Decodable2718,
    primitives::{B256, U256, keccak256},
    signers::local::PrivateKeySigner,
};
use futures_util::{SinkExt, StreamExt};
use lp_maker::{
    config::{Config, WebSocketConfig},
    evm::{UniswapV3, nonce::NonceState, rpc::Rpc, tx::Executor},
    monitor::{range_position, volume::Volume},
    store::Store,
    stream::{self, Protocol},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

fn config() -> Config {
    Config::load("config/robinhood.toml").unwrap()
}
fn lifecycle() -> WebSocketConfig {
    WebSocketConfig {
        heartbeat_seconds: 1,
        heartbeat_timeout_seconds: 4,
        idle_timeout_seconds: 2,
        connect_timeout_seconds: 2,
        write_timeout_seconds: 1,
        reconnect_initial_ms: 10,
        reconnect_max_seconds: 1,
    }
}

#[tokio::test]
async fn native_ws_reconnects_and_resubscribes_after_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for generation in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let subscribe: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(subscribe["subscription"]["type"], "allMids");
            socket.send(Message::Text(json!({"channel":"allMids","data":{"mids":{"ETH":"2500"},"generation":generation}}).to_string().into())).await.unwrap();
            if generation == 0 {
                socket.close(None).await.unwrap();
            } else {
                while socket.next().await.is_some() {}
            }
        }
    });
    let (tx, mut rx) = mpsc::channel(32);
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        stream::listen(
            &url,
            Protocol::Hyperliquid,
            vec![json!({"type":"allMids"})],
            lifecycle(),
            tx,
            receiver,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut count = 0;
        while let Some(e) = rx.recv().await {
            if e.channel == "allMids" {
                count += 1;
                if count == 2 {
                    break;
                }
            }
        }
        assert_eq!(count, 2);
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.await.unwrap();
}
#[tokio::test]
async fn pong_does_not_hide_data_idle_timeout() {
    assert_eq!(WebSocketConfig::default().idle_timeout_seconds, 300);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(s).await.unwrap();
        while let Some(Ok(Message::Text(t))) = socket.next().await {
            if serde_json::from_str::<Value>(&t).unwrap()["method"] == "ping"
                && socket
                    .send(Message::Text(json!({"channel":"pong"}).to_string().into()))
                    .await
                    .is_err()
            {
                break;
            }
        }
    });
    let (tx, mut rx) = mpsc::channel(32);
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        stream::listen(
            &url,
            Protocol::Hyperliquid,
            vec![json!({"type":"allMids"})],
            lifecycle(),
            tx,
            receiver,
        )
        .await
    });
    let e = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let e = rx.recv().await.unwrap();
            if e.channel == "disconnected" {
                break e;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        e.data["error"]
            .as_str()
            .unwrap()
            .contains("data idle timeout")
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.await.unwrap();
}
#[tokio::test]
async fn shutdown_cancels_stuck_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, _rx) = mpsc::channel(8);
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        stream::listen(
            &url,
            Protocol::Ethereum,
            vec![json!(["newHeads"])],
            WebSocketConfig::default(),
            tx,
            receiver,
        )
        .await
    });
    let (_socket, _) = listener.accept().await.unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn evm_ws_maps_subscription_ids_and_handles_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(s).await.unwrap();
        let req: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(req["params"], json!(["newHeads"]));
        for value in [
            json!({"jsonrpc":"2.0","id":1,"result":"0xsub"}),
            json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xsub","result":{"number":"0x42"}}}),
        ] {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
        while socket.next().await.is_some() {}
    });
    let (tx, mut rx) = mpsc::channel(32);
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        stream::listen(
            &url,
            Protocol::Ethereum,
            vec![json!(["newHeads"])],
            lifecycle(),
            tx,
            receiver,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let e = rx.recv().await.unwrap();
            if e.channel == "newHeads" {
                assert_eq!(e.data["number"], "0x42");
                break;
            }
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.await.unwrap();
}

#[derive(Default)]
struct MockChain {
    transactions: Mutex<Vec<(u64, String)>>,
    logs: Mutex<Vec<Value>>,
    lose_response: AtomicBool,
    mined: AtomicBool,
    revert: AtomicBool,
}
async fn rpc_server() -> (String, Arc<MockChain>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let state = Arc::new(MockChain::default());
    state.mined.store(true, Ordering::SeqCst);
    let shared = state.clone();
    let server = tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let state = shared.clone();
            tokio::spawn(async move {
                let mut socket = accept_async(s).await.unwrap();
                let Some(Ok(Message::Text(t))) = socket.next().await else {
                    return;
                };
                let req: Value = serde_json::from_str(&t).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "eth_chainId" => json!("0x1237"),
                    "eth_getTransactionCount" => {
                        let n = if state.mined.load(Ordering::SeqCst)
                            || req["params"][1] == "pending"
                        {
                            state.transactions.lock().unwrap().len()
                        } else {
                            0
                        };
                        json!(format!("0x{n:x}"))
                    }
                    "eth_call" => json!("0x"),
                    "eth_estimateGas" => json!("0x5208"),
                    "eth_gasPrice" => json!("0x1"),
                    "eth_getBalance" => json!("0xde0b6b3a7640000"),
                    "eth_blockNumber" => json!("0x100"),
                    "eth_getBlockByNumber" => json!({"hash":"0xcanonical"}),
                    "eth_sendRawTransaction" => {
                        let raw = hex::decode(
                            req["params"][0].as_str().unwrap().trim_start_matches("0x"),
                        )
                        .unwrap();
                        let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                        let hash = format!("{:#x}", keccak256(&raw));
                        state
                            .transactions
                            .lock()
                            .unwrap()
                            .push((tx.nonce(), hash.clone()));
                        if state.lose_response.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        json!(hash)
                    }
                    "eth_getTransactionReceipt" => {
                        let found = state
                            .transactions
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|(_, h)| *h == req["params"][0]);
                        if found && state.mined.load(Ordering::SeqCst) {
                            json!({"transactionHash":req["params"][0],"blockNumber":"0x1","blockHash":"0xcanonical","status":if state.revert.load(Ordering::SeqCst){"0x0"}else{"0x1"},"gasUsed":"0x5208","effectiveGasPrice":"0x1","logs":*state.logs.lock().unwrap()})
                        } else {
                            Value::Null
                        }
                    }
                    other => panic!("unexpected RPC {other}"),
                };
                let _ = socket
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":req["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await;
            });
        }
    });
    (url, state, server)
}
fn executor(url: String, store: Arc<Store>) -> Executor {
    let mut c = config();
    c.liquidity.rpc_url = url;
    // Public deterministic test vector; never a user key, never sent to a real network.
    let signer = PrivateKeySigner::from_bytes(&B256::from([1u8; 32])).unwrap();
    Executor::with_signer(UniswapV3::new(c.liquidity).unwrap(), store, signer).unwrap()
}
#[tokio::test]
async fn reconciled_mint_consumes_first_entry_before_checkpoint_and_burn_keeps_history() {
    let (url, state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let c = config();
    lp_maker::recovery::save(
        &store,
        &c,
        &lp_maker::strategy::Strategy::default(),
        &lp_maker::engine::Paper::new(&c),
    )
    .unwrap();
    let operation = json!({"kind":"mint","layer":"core"});
    state.revert.store(true, Ordering::SeqCst);
    assert!(
        ex.send(ex.venue.manager, vec![1], operation.clone())
            .await
            .is_err()
    );
    assert!(store.read::<Value>("lp_history.json").unwrap().is_none());
    state.revert.store(false, Ordering::SeqCst);
    *state.logs.lock().unwrap() = vec![json!({"address":ex.venue.cfg.position_manager,"topics":[
        format!("{:#x}",keccak256("Transfer(address,address,uint256)")),
        format!("0x{:064x}",0), format!("0x{:0>64}",hex::encode(ex.owner())), format!("0x{:064x}",42)
    ]})];
    state.lose_response.store(true, Ordering::SeqCst);
    assert!(
        ex.send(ex.venue.manager, vec![1], operation.clone())
            .await
            .is_err()
    );
    assert!(store.pending().unwrap().is_some());
    assert!(store.read::<Value>("lp_history.json").unwrap().is_none());
    let hash = store.pending().unwrap().unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_string();
    ex.wait_receipt(&hash, &operation).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    assert_eq!(
        store.read::<Value>("lp_history.json").unwrap().unwrap()["evidence"]["token_id"],
        "42"
    );
    assert_eq!(
        store.read::<Value>("nfts.json").unwrap().unwrap()["core"],
        "42"
    );
    // The checkpoint is deliberately still the original Initial/Warmup checkpoint.
    ex.send(
        ex.venue.manager,
        vec![2],
        json!({"kind":"burn","layer":"core"}),
    )
    .await
    .unwrap();
    assert!(ex.ids().unwrap().is_empty());
    assert_eq!(
        lp_maker::recovery::load(&store, &c)
            .unwrap()
            .0
            .entry_history,
        lp_maker::strategy::EntryHistory::Established
    );
    server.abort();
}
#[tokio::test]
async fn signed_transactions_allocate_unique_nonces_and_survive_restart() {
    let (url, state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let executor = Arc::new(executor(url.clone(), store.clone()));
    let a = executor.clone();
    let b = executor.clone();
    let (x, y) = tokio::join!(
        a.send(a.venue.base, vec![1], json!({"kind":"approve"})),
        b.send(b.venue.base, vec![2], json!({"kind":"approve"}))
    );
    x.unwrap();
    y.unwrap();
    assert_eq!(
        state
            .transactions
            .lock()
            .unwrap()
            .iter()
            .map(|x| x.0)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(store.pending().unwrap().is_none());
    let restarted = lp_maker::evm::nonce::NonceManager::new(
        Rpc::new(url).unwrap(),
        executor.owner(),
        4663,
        120,
        store.clone(),
    );
    assert_eq!(restarted.next().await.unwrap(), 2);
    let events = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert!(!events.contains("raw_transaction"));
    assert!(!events.contains("signature"));
    server.abort();
}
#[tokio::test]
async fn ambiguous_broadcast_blocks_duplicate_send_until_receipt_reconciliation() {
    let (url, state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let executor = executor(url, store.clone());
    state.lose_response.store(true, Ordering::SeqCst);
    assert!(
        executor
            .send(executor.venue.base, vec![1], json!({"kind":"approve"}))
            .await
            .is_err()
    );
    assert!(store.pending().unwrap().is_some());
    assert!(
        executor
            .send(executor.venue.base, vec![1], json!({"kind":"approve"}))
            .await
            .is_err()
    );
    assert_eq!(state.transactions.lock().unwrap().len(), 1);
    let hash = state.transactions.lock().unwrap()[0].1.clone();
    executor
        .wait_receipt(&hash, &json!({"kind":"approve"}))
        .await
        .unwrap();
    assert_eq!(executor.nonce.next().await.unwrap(), 1);
    assert!(store.pending().unwrap().is_none());
    server.abort();
}
#[tokio::test]
async fn reverted_receipt_consumes_nonce_without_rebroadcast() {
    let (url, state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let executor = executor(url, store.clone());
    state.revert.store(true, Ordering::SeqCst);
    assert!(
        executor
            .send(executor.venue.base, vec![1], json!({"kind":"approve"}))
            .await
            .is_err()
    );
    assert!(store.pending().unwrap().is_none());
    assert_eq!(executor.nonce.next().await.unwrap(), 1);
    server.abort();
}
#[test]
fn nonce_regression_external_pending_and_local_unknown_all_block() {
    let mut s = NonceState::default();
    s.observe(8, 8, 1).unwrap();
    assert_eq!(s.available().unwrap(), 8);
    s.next_floor = 9;
    s.observe(8, 8, 2).unwrap();
    assert!(s.available().is_err());
    s.observe(9, 10, 3).unwrap();
    assert!(s.available().is_err());
    s.observe(10, 10, 4).unwrap();
    s.inflight = Some(json!({"nonce":9,"hash":"unknown"}));
    assert!(s.available().is_err());
    assert!(s.observe(11, 10, 5).is_err());
}
fn swap_log() -> Value {
    let words = [
        U256::from(1_000_000_000_000_000_000u64),
        U256::MAX - U256::from(2_500_000_000u64) + U256::from(1),
        U256::from(1),
        U256::from(1),
        U256::from(0),
    ];
    let data: Vec<u8> = words
        .into_iter()
        .flat_map(|w| w.to_be_bytes::<32>())
        .collect();
    json!({"topics":[format!("{:#x}",keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"))],"data":format!("0x{}",hex::encode(data)),"blockHash":"0xabc","logIndex":"0x1","blockNumber":"0xa","transactionHash":"0xtx","removed":false})
}
#[test]
fn volume_deduplicates_backfill_and_removes_reorged_logs() {
    let c = config();
    let mut v = Volume::default();
    let mut l = swap_log();
    v.insert(&l, &c, true).unwrap();
    v.insert(&l, &c, true).unwrap();
    assert_eq!(v.trades.len(), 1);
    let t = v.trades.values().next().unwrap();
    assert_eq!(t.quote, 2500.0);
    assert_eq!(t.base, 1.0);
    l["removed"] = json!(true);
    v.insert(&l, &c, true).unwrap();
    assert!(v.trades.is_empty());
}
#[test]
fn price_position_covers_both_boundaries() {
    assert_eq!(range_position(99.0, 100.0, 110.0)["status"], "below");
    assert_eq!(range_position(100.0, 100.0, 110.0)["status"], "in_range");
    assert_eq!(range_position(110.0, 100.0, 110.0)["status"], "above");
    assert_eq!(range_position(105.0, 100.0, 110.0)["fraction"], 0.5);
}
#[test]
fn config_accepts_ws_and_wss_but_rejects_wrong_schemes_and_timeouts() {
    let mut c = config();
    c.liquidity.rpc_url = "ws://localhost:1234".into();
    c.liquidity.ws_url = "ws://localhost:1234".into();
    c.hyperliquid.ws_url = "ws://localhost:2345".into();
    c.validate().unwrap();
    c.liquidity.ws_url = "https://localhost".into();
    assert!(c.validate().is_err());
    c.liquidity.ws_url = "wss://localhost".into();
    c.websocket.idle_timeout_seconds = 0;
    assert!(c.validate().is_err());
}

#[test]
fn endpoint_changes_preserve_state_but_budget_changes_require_migration() {
    use lp_maker::engine::transport_independent_fingerprint as fingerprint;
    let c = config();
    let old = json!({"mode":c.mode,"liquidity":c.liquidity,"hyperliquid":c.hyperliquid,"strategy":c.strategy});
    let mut new = old.clone();
    new["liquidity"]["rpc_url"] = json!("ws://localhost:9999");
    new["liquidity"]["archive_rpc_url"] = Value::Null;
    new["liquidity"]["nonce_refresh_seconds"] = json!(60);
    assert_eq!(
        fingerprint(&old.to_string()).unwrap(),
        fingerprint(&new.to_string()).unwrap()
    );
    new["strategy"]["total_capital"] = json!(200);
    assert_ne!(
        fingerprint(&old.to_string()).unwrap(),
        fingerprint(&new.to_string()).unwrap()
    );
}
#[tokio::test]
async fn pending_file_recovers_crash_before_nonce_state_write() {
    let (url, _state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    store.begin(json!({"venue":"evm","hash":"0xunresolved","nonce":0,"owner":ex.owner(),"prepared_ms":1})).unwrap();
    let state = ex.nonce.refresh().await.unwrap();
    assert_eq!(state.next_floor, 1);
    assert!(state.inflight.is_some());
    assert!(state.available().is_err());
    server.abort();
}
#[tokio::test]
async fn state_directory_cannot_be_reused_by_another_signer() {
    let (url, _state, server) = rpc_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let first = executor(url.clone(), store.clone());
    let mut c = config();
    c.liquidity.rpc_url = url;
    let other = PrivateKeySigner::from_bytes(&B256::from([2u8; 32])).unwrap();
    assert!(
        Executor::with_signer(UniswapV3::new(c.liquidity).unwrap(), store.clone(), other).is_err()
    );
    assert_eq!(
        store
            .read::<Value>("execution_identity.json")
            .unwrap()
            .unwrap()["owner"],
        json!(first.owner())
    );
    server.abort();
}
#[tokio::test]
async fn volume_uses_block_time_when_log_timestamp_is_zero_and_rebuilds_after_reorg() {
    use std::sync::atomic::AtomicU64;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let generation = Arc::new(AtomicU64::new(0));
    let shared = generation.clone();
    let server = tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let g = shared.clone();
            tokio::spawn(async move {
                let mut socket = accept_async(s).await.unwrap();
                let Some(Ok(Message::Text(t))) = socket.next().await else {
                    return;
                };
                let req: Value = serde_json::from_str(&t).unwrap();
                let gen_id = g.load(Ordering::SeqCst);
                let hash = |n: u64| format!("0x{:064x}", n + gen_id * 100_000);
                let result = match req["method"].as_str().unwrap() {
                    "eth_chainId" => json!("0x1237"),
                    "eth_getBlockByNumber" => {
                        let n = lp_maker::evm::rpc::hex_u64(&req["params"][0]).unwrap();
                        json!({"hash":hash(n),"timestamp":format!("0x{:x}",1_700_000_000+n/10)})
                    }
                    "eth_getLogs" => {
                        let from =
                            lp_maker::evm::rpc::hex_u64(&req["params"][0]["fromBlock"]).unwrap();
                        let to = lp_maker::evm::rpc::hex_u64(&req["params"][0]["toBlock"]).unwrap();
                        json!(
                            [1900u64, 2000, 4999]
                                .into_iter()
                                .filter(|n| *n >= from && *n <= to)
                                .map(|n| {
                                    let mut log = swap_log();
                                    log["address"] = req["params"][0]["address"].clone();
                                    log["blockNumber"] = json!(format!("0x{n:x}"));
                                    log["blockHash"] = json!(hash(n));
                                    log["blockTimestamp"] = json!("0x0");
                                    log
                                })
                                .collect::<Vec<_>>()
                        )
                    }
                    other => panic!("unexpected history RPC {other}"),
                };
                let _ = socket
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":req["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await;
            });
        }
    });
    let mut c = config();
    c.liquidity.rpc_url = url.clone();
    c.liquidity.archive_rpc_url = Some(url);
    let venue = UniswapV3::new(c.liquidity.clone()).unwrap();
    let mut snapshot = lp_maker::domain::PoolSnapshot {
        block: 5000,
        block_hash: format!("0x{:064x}", 5000),
        time_ms: 1_700_000_500_000,
        price: 2500.,
        tick: 0,
        tick_spacing: 1,
        liquidity: "1".into(),
        sqrt_price_x96: "1".into(),
        base_is_token0: true,
    };
    let mut volume = Volume::default();
    volume.refresh(&c, &venue, &snapshot).await.unwrap();
    let report = volume.report(&snapshot, 300);
    assert_eq!(report["from_block"], 2000);
    assert_eq!(report["swap_count"], 2);
    assert_eq!(report["volume_usdg"], 5000.0);
    assert_eq!(report["complete"], true);
    volume.refresh(&c, &venue, &snapshot).await.unwrap();
    assert_eq!(volume.trades.len(), 2);
    generation.store(1, Ordering::SeqCst);
    snapshot.block_hash = format!("0x{:064x}", 105000);
    volume.refresh(&c, &venue, &snapshot).await.unwrap();
    assert_eq!(volume.trades.len(), 2);
    assert_eq!(volume.report(&snapshot, 300)["complete"], true);
    server.abort();
}

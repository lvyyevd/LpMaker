use alloy::{
    consensus::{SignableTransaction, Transaction, TxEnvelope, TxLegacy},
    eips::eip2718::{Decodable2718, Encodable2718},
    primitives::{B256, TxKind, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::SolCall,
};
use futures_util::{SinkExt, StreamExt};
use lp_maker::{
    config::Config,
    evm::{UniswapV3, abi::IERC20, fees::Fees, rpc::Rpc, tx::Executor},
    store::Store,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct Chain {
    sent: Vec<(String, TxEnvelope)>,
    mined: Option<String>,
    known: Vec<String>,
    base: Option<u64>,
    required_base: u128,
    priority_error: Option<i64>,
    drop_response: bool,
    pending_external: bool,
    consumed_external: bool,
    original_wins: Option<String>,
    l1_fee: u128,
}
impl Default for Chain {
    fn default() -> Self {
        Self {
            sent: vec![],
            mined: None,
            known: vec![],
            base: Some(58_324_000),
            required_base: 58_324_000,
            priority_error: None,
            drop_response: false,
            pending_external: false,
            consumed_external: false,
            original_wins: None,
            l1_fee: 100,
        }
    }
}
async fn server() -> (String, Arc<Mutex<Chain>>, tokio::task::JoinHandle<()>) {
    server_for(4663).await
}
async fn server_for(chain_id: u64) -> (String, Arc<Mutex<Chain>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let shared = Arc::new(Mutex::new(Chain::default()));
    let state = shared.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let mut socket = accept_async(stream).await.unwrap();
                let Some(Ok(Message::Text(text))) = socket.next().await else {
                    return;
                };
                let req: Value = serde_json::from_str(&text).unwrap();
                let (result, error, drop_response) = {
                    let mut s = state.lock().unwrap();
                    let mut error = None;
                    let mut drop_response = false;
                    let result = match req["method"].as_str().unwrap() {
                        "eth_chainId" => json!(format!("0x{chain_id:x}")),
                        "eth_gasPrice" => json!(format!("0x{:x}", 58_182_000)),
                        "eth_maxPriorityFeePerGas" => {
                            if let Some(code) = s.priority_error {
                                error =
                                    Some(json!({"code":code,"message":"priority RPC unavailable"}));
                            }
                            json!("0x0")
                        }
                        "eth_getBlockByNumber" => {
                            let mut block = json!({"hash":"0xcanonical"});
                            if let Some(base) = s.base {
                                block["baseFeePerGas"] = json!(format!("0x{base:x}"));
                            }
                            block
                        }
                        "eth_blockNumber" => json!("0x100"),
                        "eth_getBalance" => json!("0xde0b6b3a7640000"),
                        "eth_estimateGas" => json!("0xc350"),
                        "eth_call" => {
                            if req["params"][0]["to"].as_str().is_some_and(|a| {
                                a.eq_ignore_ascii_case(
                                    lp_maker::liquidity::chains::base::fees::ORACLE,
                                )
                            }) {
                                let input = req["params"][0]["data"].as_str().unwrap();
                                let l1_selector = format!(
                                    "0x{}",
                                    hex::encode(&keccak256("getL1FeeUpperBound(uint256)")[..4])
                                );
                                json!(format!(
                                    "0x{:064x}",
                                    if input.starts_with(&l1_selector) {
                                        s.l1_fee
                                    } else {
                                        0
                                    }
                                ))
                            } else {
                                json!("0x")
                            }
                        }
                        "eth_getTransactionCount" => {
                            let advanced = s.mined.is_some()
                                || s.consumed_external
                                || (req["params"][1] == "pending"
                                    && (s.pending_external || !s.known.is_empty()));
                            json!(if advanced { "0xd0" } else { "0xcf" })
                        }
                        "eth_getTransactionByHash" => {
                            if s.known.iter().any(|h| req["params"][0] == *h) {
                                json!({"hash":req["params"][0]})
                            } else {
                                Value::Null
                            }
                        }
                        "eth_getTransactionReceipt" => {
                            if s.mined.as_ref().is_some_and(|h| req["params"][0] == *h) {
                                json!({"transactionHash":req["params"][0],"blockNumber":"0x1","blockHash":"0xcanonical","status":"0x1","logs":[]})
                            } else {
                                Value::Null
                            }
                        }
                        "eth_sendRawTransaction" => {
                            let raw = hex::decode(
                                req["params"][0].as_str().unwrap().trim_start_matches("0x"),
                            )
                            .unwrap();
                            let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                            let hash = format!("{:#x}", keccak256(raw));
                            s.sent.push((hash.clone(), tx.clone()));
                            if let Some(original) = s.original_wins.clone() {
                                s.mined = Some(original);
                                error = Some(json!({"code":-32000,"message":"nonce too low"}));
                            } else if tx.max_fee_per_gas() < s.required_base {
                                error = Some(
                                    json!({"code":-32000,"message":"max fee per gas less than block base fee"}),
                                );
                            } else {
                                s.known.push(hash.clone());
                                s.mined = Some(hash.clone());
                            }
                            drop_response = s.drop_response;
                            s.drop_response = false;
                            json!(hash)
                        }
                        other => panic!("unexpected RPC {other}"),
                    };
                    (result, error, drop_response)
                };
                if drop_response {
                    return;
                }
                let response = match error {
                    Some(error) => json!({"jsonrpc":"2.0","id":req["id"],"error":error}),
                    None => json!({"jsonrpc":"2.0","id":req["id"],"result":result}),
                };
                let _ = socket
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }
    });
    (url, shared, task)
}
fn signer() -> PrivateKeySigner {
    // Public deterministic test vector, used only with the loopback mock RPC.
    PrivateKeySigner::from_bytes(&B256::from([1u8; 32])).unwrap()
}
fn executor(url: String, store: Arc<Store>) -> Executor {
    let mut c = Config::load("config/robinhood.toml").unwrap();
    c.liquidity.rpc_url = url;
    Executor::with_signer(UniswapV3::new(c.liquidity).unwrap(), store, signer()).unwrap()
}
fn approval(ex: &Executor) -> (Vec<u8>, Value) {
    let data = IERC20::approveCall {
        spender: ex.venue.router,
        value: U256::from(39_800_000),
    }
    .abi_encode();
    let op = json!({"kind":"approve","token":ex.venue.quote,"spender":ex.venue.router,"raw_amount":"39800000"});
    (data, op)
}
async fn legacy_pending(ex: &Executor) -> String {
    let (data, op) = approval(ex);
    let tx = TxLegacy {
        chain_id: Some(4663),
        nonce: 207,
        gas_price: 58_182_000,
        gas_limit: 60_000,
        to: TxKind::Call(ex.venue.quote),
        value: U256::ZERO,
        input: data.into(),
    };
    let signature = signer().sign_hash_sync(&tx.signature_hash()).unwrap();
    let raw = tx.into_signed(signature).encoded_2718();
    let hash = format!("{:#x}", keccak256(&raw));
    ex.store.begin(json!({"venue":"evm","owner":ex.owner(),"hash":hash,"nonce":207,"prepared_ms":lp_maker::now_ms(),"operation":op,"raw_transaction":format!("0x{}",hex::encode(raw))})).unwrap();
    ex.nonce.prepared(207, &hash).await.unwrap();
    hash
}

#[tokio::test]
async fn rising_base_fee_uses_type_two_cap_and_zero_tip_without_duplicate_nonce() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let (data, op) = approval(&ex);
    ex.send(ex.venue.quote, data, op).await.unwrap();
    let tx = state.lock().unwrap().sent[0].1.clone();
    assert!(matches!(tx, TxEnvelope::Eip1559(_)));
    assert_eq!(tx.max_fee_per_gas(), 116_648_000);
    assert_eq!(tx.max_priority_fee_per_gas(), Some(0));
    assert_eq!(tx.nonce(), 207);
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    assert!(store.pending().unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn base_chain_fee_budget_blocks_l1_overrun_then_sends_one_recoverable_transaction() {
    let (url, state, server) = server_for(8453).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut c = Config::load("config/base.toml").unwrap();
    c.liquidity.rpc_url = url.clone();
    let ex = Executor::with_signer(
        UniswapV3::new(c.liquidity.clone()).unwrap(),
        store.clone(),
        signer(),
    )
    .unwrap();
    state.lock().unwrap().l1_fee = 2_000_000_000_000_000; // Headroom makes L1 alone exceed 0.003 ETH.
    let (data, op) = approval(&ex);
    assert!(
        ex.send(ex.venue.quote, data, op)
            .await
            .unwrap_err()
            .to_string()
            .contains("gas budget")
    );
    assert!(state.lock().unwrap().sent.is_empty());
    assert!(store.pending().unwrap().is_none());
    state.lock().unwrap().l1_fee = 100;
    state.lock().unwrap().drop_response = true;
    let (data, op) = approval(&ex);
    assert!(ex.send(ex.venue.quote, data, op).await.is_err());
    let hash = store.pending().unwrap().unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(state.lock().unwrap().sent[0].1.chain_id(), Some(8453));
    drop(ex);
    drop(store);
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = Executor::with_signer(
        UniswapV3::new(c.liquidity).unwrap(),
        store.clone(),
        signer(),
    )
    .unwrap();
    ex.retry_approval(&hash).await.unwrap();
    assert!(store.pending().unwrap().is_none());
    assert_eq!(state.lock().unwrap().sent.len(), 1);
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    server.abort();
}

#[tokio::test]
async fn custom_reserves_reach_signed_fee_cap_and_gas_limit() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut ex = executor(url, store.clone());
    ex.venue.cfg.gas_fee_buffer_bps = 5_000;
    ex.venue.cfg.gas_limit_buffer_bps = 2_500;
    let (data, op) = approval(&ex);
    ex.send(ex.venue.quote, data, op).await.unwrap();
    let tx = state.lock().unwrap().sent[0].1.clone();
    assert_eq!(tx.max_fee_per_gas(), 87_486_000);
    assert_eq!(tx.max_priority_fee_per_gas(), Some(0));
    assert_eq!(tx.gas_limit(), 62_500);
    assert!(store.pending().unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn replacement_uses_configured_headroom_but_keeps_original_gas_limit() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut ex = executor(url, store.clone());
    let hash = legacy_pending(&ex).await;
    ex.venue.cfg.gas_fee_buffer_bps = 30_000;
    ex.venue.cfg.gas_limit_buffer_bps = 5_000;
    ex.retry_approval(&hash).await.unwrap();
    let tx = state.lock().unwrap().sent[0].1.clone();
    assert_eq!(tx.max_fee_per_gas(), 306_023_501);
    assert_eq!(tx.gas_limit(), 60_000);
    assert_eq!(tx.nonce(), 207);
    server.abort();
}

#[test]
fn reserve_rounds_up_small_estimates_and_rejects_overflow_or_zero_buffer() {
    use lp_maker::evm::fees::buffered;
    assert_eq!(buffered(21_001, 2_000).unwrap(), 25_202);
    assert_eq!(buffered(1, 1).unwrap(), 2);
    assert_eq!(buffered(0, 10_000).unwrap(), 0);
    assert!(buffered(u128::MAX, 1).is_err());
    assert!(buffered(100, 0).is_err());
    assert!(buffered(100, 40_001).is_err());
}

#[test]
fn old_configs_get_reserve_defaults_and_preserve_state_binding() {
    use lp_maker::engine::transport_independent_fingerprint as fingerprint;
    let config = Config::load("config/robinhood.toml").unwrap();
    let mut old = serde_json::to_value(&config).unwrap();
    let liquidity = old["liquidity"].as_object_mut().unwrap();
    liquidity.remove("gas_fee_buffer_bps");
    liquidity.remove("gas_limit_buffer_bps");
    let mut restored: Config = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(restored.liquidity.gas_fee_buffer_bps, 10_000);
    assert_eq!(restored.liquidity.gas_limit_buffer_bps, 2_000);
    restored.validate().unwrap();
    restored.liquidity.gas_fee_buffer_bps = 20_000;
    restored.liquidity.gas_limit_buffer_bps = 3_000;
    let old_fingerprint = fingerprint(&old.to_string()).unwrap();
    assert_eq!(
        old_fingerprint,
        fingerprint(&serde_json::to_string(&restored).unwrap()).unwrap()
    );
    restored.liquidity.max_gas_native *= 2.0;
    assert_ne!(
        old_fingerprint,
        fingerprint(&serde_json::to_string(&restored).unwrap()).unwrap()
    );
    for (fee, limit) in [(0, 2_000), (40_001, 2_000), (10_000, 0), (10_000, 10_001)] {
        restored.liquidity.gas_fee_buffer_bps = fee;
        restored.liquidity.gas_limit_buffer_bps = limit;
        assert!(restored.validate().is_err());
    }
}

#[tokio::test]
async fn fee_rejection_is_durable_and_still_blocks_automatic_duplicate_send() {
    let (url, state, server) = server().await;
    state.lock().unwrap().required_base = 500_000_000;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let (data, op) = approval(&ex);
    assert!(
        ex.send(ex.venue.quote, data.clone(), op.clone())
            .await
            .is_err()
    );
    assert_eq!(
        store.pending().unwrap().unwrap()["last_broadcast_error"]["rpc_rejection"],
        true
    );
    assert!(ex.send(ex.venue.quote, data, op).await.is_err());
    assert_eq!(state.lock().unwrap().sent.len(), 1);
    server.abort();
}

#[tokio::test]
async fn legacy_approval_replacement_preserves_nonce_calldata_and_reconciles() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let hash = legacy_pending(&ex).await;
    ex.retry_approval(&hash).await.unwrap();
    let tx = state.lock().unwrap().sent[0].1.clone();
    assert_eq!(tx.nonce(), 207);
    assert_eq!(tx.input().as_ref(), approval(&ex).0);
    assert_eq!(tx.to(), Some(ex.venue.quote));
    assert_eq!(tx.value(), U256::ZERO);
    assert_eq!(tx.gas_limit(), 60_000);
    assert!(tx.max_fee_per_gas() > 58_182_000);
    assert!(store.pending().unwrap().is_none());
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    let events = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert!(events.contains("evm_approval_fee_replacement"));
    assert!(!events.contains("raw_transaction"));
    server.abort();
}

#[tokio::test]
async fn replacement_lost_response_survives_full_restart_and_checks_all_hashes() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url.clone(), store.clone());
    let hash = legacy_pending(&ex).await;
    state.lock().unwrap().drop_response = true;
    assert!(ex.retry_approval(&hash).await.is_err());
    let mut visible = store.pending().unwrap().unwrap();
    assert_eq!(visible["replacements"].as_array().unwrap().len(), 1);
    lp_maker::store::redact_signatures(&mut visible);
    assert!(!visible.to_string().contains("raw_transaction"));
    assert!(ex.nonce.next().await.is_err());
    drop(ex);
    drop(store);
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    // The retry entry point discovers the earlier replacement already mined; no second send.
    ex.retry_approval(&hash).await.unwrap();
    assert_eq!(state.lock().unwrap().sent.len(), 1);
    assert!(store.pending().unwrap().is_none());
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    server.abort();
}

#[tokio::test]
async fn original_mining_during_replacement_keeps_both_hashes_until_reconciled() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let hash = legacy_pending(&ex).await;
    state.lock().unwrap().original_wins = Some(hash.clone());
    assert!(ex.retry_approval(&hash).await.is_err());
    assert!(store.pending().unwrap().is_some());
    ex.wait_receipt(&hash, &approval(&ex).1).await.unwrap();
    assert_eq!(state.lock().unwrap().sent.len(), 1);
    assert!(store.pending().unwrap().is_none());
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    server.abort();
}

#[tokio::test]
async fn second_recovery_keeps_rejected_replacement_and_uses_same_nonce() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url.clone(), store.clone());
    let hash = legacy_pending(&ex).await;
    state.lock().unwrap().required_base = 500_000_000;
    assert!(ex.retry_approval(&hash).await.is_err());
    assert_eq!(
        store.pending().unwrap().unwrap()["replacements"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Also exercise a crash after writing pending but before writing nonce state.
    std::fs::remove_file(dir.path().join("evm_nonce.json")).unwrap();
    drop(ex);
    drop(store);
    state.lock().unwrap().required_base = 58_324_000;
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    ex.retry_approval(&hash).await.unwrap();
    let sent = state.lock().unwrap().sent.clone();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|(_, tx)| tx.nonce() == 207));
    assert!(sent[1].1.max_fee_per_gas() > sent[0].1.max_fee_per_gas());
    assert_eq!(sent[0].1.input(), sent[1].1.input());
    assert!(store.pending().unwrap().is_none());
    assert_eq!(ex.nonce.next().await.unwrap(), 208);
    server.abort();
}

#[tokio::test]
async fn recovery_blocks_mismatched_hash_content_external_nonce_and_nonapproval() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let hash = legacy_pending(&ex).await;
    let original = store.pending().unwrap().unwrap();
    assert!(
        ex.retry_approval(&format!("{:#x}", B256::ZERO))
            .await
            .is_err()
    );
    for (key, value) in [("kind", "swap"), ("raw_amount", "1")] {
        let mut corrupt = original.clone();
        corrupt["operation"][key] = json!(value);
        store.write("pending.json", &corrupt).unwrap();
        assert!(ex.retry_approval(&hash).await.is_err());
    }
    store.write("pending.json", &original).unwrap();
    state.lock().unwrap().pending_external = true;
    assert!(ex.retry_approval(&hash).await.is_err());
    state.lock().unwrap().pending_external = false;
    state.lock().unwrap().consumed_external = true;
    assert!(ex.retry_approval(&hash).await.is_err());
    assert!(state.lock().unwrap().sent.is_empty());
    assert_eq!(store.pending().unwrap().unwrap(), original);
    server.abort();
}

#[tokio::test]
async fn gas_budget_blocks_new_and_replacement_intents_before_broadcast() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut ex = executor(url, store.clone());
    ex.venue.cfg.max_gas_native = 1e-10;
    let (data, op) = approval(&ex);
    assert!(ex.send(ex.venue.quote, data, op).await.is_err());
    assert!(store.pending().unwrap().is_none());
    let hash = legacy_pending(&ex).await;
    let original = store.pending().unwrap().unwrap();
    assert!(ex.retry_approval(&hash).await.is_err());
    assert_eq!(store.pending().unwrap().unwrap(), original);
    assert!(state.lock().unwrap().sent.is_empty());
    server.abort();
}

#[tokio::test]
async fn corrupted_signed_pending_cannot_be_cleared_by_an_apparent_receipt() {
    let (url, state, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = executor(url, store.clone());
    let hash = legacy_pending(&ex).await;
    let mut pending = store.pending().unwrap().unwrap();
    pending["raw_transaction"] = json!("0x00");
    store.write("pending.json", &pending).unwrap();
    state.lock().unwrap().mined = Some(hash.clone());
    assert!(ex.wait_receipt(&hash, &approval(&ex).1).await.is_err());
    assert_eq!(store.pending().unwrap().unwrap(), pending);
    assert!(state.lock().unwrap().sent.is_empty());
    server.abort();
}

#[tokio::test]
async fn unsupported_tip_falls_back_but_other_rpc_errors_do_not() {
    let (url, state, server) = server().await;
    let rpc = Rpc::new(url).unwrap();
    state.lock().unwrap().priority_error = Some(-32601);
    let fees = Fees::estimate(&rpc, 10_000).await.unwrap();
    assert_eq!(fees.max_priority_fee_per_gas, Some(0));
    assert_eq!(fees.max_fee_per_gas, 116_648_000);
    state.lock().unwrap().priority_error = Some(-32000);
    assert!(Fees::estimate(&rpc, 10_000).await.is_err());
    state.lock().unwrap().base = None;
    let legacy = Fees::estimate(&rpc, 10_000).await.unwrap();
    assert_eq!(legacy.max_priority_fee_per_gas, None);
    assert_eq!(legacy.max_fee_per_gas, 116_364_000);
    server.abort();
}

#[test]
fn fee_overflow_is_rejected() {
    let mut fees = Fees {
        fee_buffer_bps: 10_000,
        base_fee_per_gas: Some(1),
        max_fee_per_gas: 2,
        max_priority_fee_per_gas: Some(0),
    };
    assert!(fees.replacement(u128::MAX, 0).is_err());
    assert!(fees.replacement(2, u128::MAX).is_err());
}

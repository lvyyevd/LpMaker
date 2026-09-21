//! 退出尾差回归测试：仅访问本地模拟 RPC，签名器使用公开固定测试向量。
mod support;
use alloy::{
    primitives::{B256, U256, keccak256},
    signers::local::PrivateKeySigner,
};
use lp_maker::{
    config::Config,
    evm::rpc::tick_word,
    liquidity::{
        self,
        uniswap_v3::{slippage::parse_sqrt, tx::Executor},
    },
    store::Store,
};
use serde_json::{Value, json};
use std::sync::Arc;
use support::rpc::Mock;

fn words(v: &[U256]) -> Value {
    json!(format!(
        "0x{}",
        v.iter().map(|x| format!("{x:064x}")).collect::<String>()
    ))
}
fn has(data: &[u8], signature: &str) -> bool {
    data.starts_with(&keccak256(signature)[..4])
}

async fn fixture(
    path: &str,
    balance: u64,
    stale: bool,
) -> (Mock, tempfile::TempDir, Arc<Store>, Executor) {
    let mut c = Config::load(path).unwrap();
    let chain_id = c.liquidity.chain_id;
    let mock = Mock::start(move |req| {
        Ok(match req["method"].as_str().unwrap() {
            "eth_chainId" => json!(format!("0x{chain_id:x}")),
            "eth_blockNumber" => json!("0x100"),
            "eth_getBlockByNumber" => json!({"hash":"0xcanonical","timestamp":format!("0x{:x}",lp_maker::now_ms()/1000-if stale {600} else {0}),"baseFeePerGas":"0x1"}),
            "eth_call" => {
                let data = hex::decode(req["params"][0]["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
                if has(&data,"slot0()") {words(&[parse_sqrt("3953120541360100857610261").unwrap(),tick_word(-198122),U256::ZERO,U256::ZERO,U256::ZERO,U256::ZERO,U256::from(1)])}
                else if has(&data,"liquidity()") {words(&[U256::from(100)])}
                else if has(&data,"tickSpacing()") {words(&[U256::from(60)])}
                else if has(&data,"balanceOf(address)") {words(&[U256::from(balance)])}
                else if has(&data,"quoteExactInputSingle((address,address,uint256,uint24,uint160))") {words(&[U256::ZERO,U256::from(1),U256::ZERO,U256::from(80_000)])}
                else {panic!("unexpected contract call in dust test")}
            }
            other => panic!("unexpected RPC {other}; dust must not approve, simulate, allocate a nonce or broadcast"),
        })
    }).await;
    c.liquidity.rpc_url = mock.url.clone();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = Executor::with_signer(
        liquidity::connect(c.liquidity).unwrap(),
        store.clone(),
        PrivateKeySigner::from_bytes(&B256::from([1_u8; 32])).unwrap(),
    )
    .unwrap();
    (mock, dir, store, ex)
}

#[tokio::test]
async fn tiny_manual_exit_keeps_exact_balance_and_audit_without_sending_or_nonce_changes() {
    for path in ["config/paper-200.toml", "config/base.toml"] {
        let (mock, dir, store, ex) = fixture(path, 17, false).await;
        let result = ex.sell_all_base().await.unwrap();
        assert_eq!(result["status"], "base_dust_retained");
        assert_eq!(result["dust"]["raw_base"], "17");
        assert_eq!(result["dust"]["raw_quote_ceiling"], "1");
        assert!(store.pending().unwrap().is_none());
        assert!(
            !mock
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["method"] == "eth_sendRawTransaction"
                    || r["method"] == "eth_getTransactionCount")
        );
        let events = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(events.contains("manual_exit_base_dust_retained"));
    }
}

#[tokio::test]
async fn regular_strategy_swap_still_rejects_zero_minimum_output() {
    let (_mock, _dir, store, ex) = fixture("config/paper-200.toml", 17, false).await;
    let error = ex.swap(true, 17e-18).await.unwrap_err();
    assert!(
        error.to_string().contains("zero minimum output"),
        "{error:#}"
    );
    assert!(store.pending().unwrap().is_none());
}

#[tokio::test]
async fn zero_quoter_output_for_non_dust_must_not_be_treated_as_empty_wallet() {
    let (_mock, _dir, store, ex) = fixture("config/base.toml", 16_000_000_000_000_003, false).await;
    let error = ex.sell_all_base().await.unwrap_err();
    assert!(
        error.to_string().contains("invalid QuoterV2 response"),
        "{error:#}"
    );
    assert!(store.pending().unwrap().is_none());
}

#[tokio::test]
async fn empty_wallet_needs_no_swap_but_stale_price_cannot_prove_dust() {
    let (_mock, _dir, _store, ex) = fixture("config/paper-200.toml", 0, false).await;
    assert_eq!(
        ex.sell_all_base().await.unwrap()["status"],
        "base_already_empty"
    );
    let (_mock, _dir, _store, ex) = fixture("config/paper-200.toml", 17, true).await;
    assert!(ex.sell_all_base().await.is_err());
}

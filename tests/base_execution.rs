mod support;
use alloy::{
    consensus::{Transaction, TxEnvelope},
    eips::eip2718::Decodable2718,
    primitives::{Address, B256, U256, keccak256},
    signers::local::PrivateKeySigner,
    sol_types::SolCall,
};
use lp_maker::{
    config::Config,
    domain::{LiquidityVenue, LpPosition},
    evm::rpc::{address_word, tick_word},
    liquidity::{
        self,
        uniswap_v3::{
            abi::{INfpm, IRouter02},
            slippage::{Range, parse_sqrt},
            tx::Executor,
        },
    },
    math,
    store::Store,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
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
#[derive(Default)]
struct Chain {
    sent: Vec<TxEnvelope>,
    receipts: BTreeMap<String, Value>,
    base: U256,
    quote: U256,
    position: Vec<U256>,
}

/// End-to-end signed calldata test against a loopback node: swap, mint, restart, withdraw.
/// The signer is a public fixed vector; no environment variables or external writes are used.
#[tokio::test]
async fn base_swap_mint_restart_and_remove_keep_correct_addresses_fee_ticks_and_nonce() {
    let mut c = Config::load("config/base.toml").unwrap();
    let manager: Address = c.liquidity.position_manager.parse().unwrap();
    let router: Address = c.liquidity.swap_router.parse().unwrap();
    let base: Address = c.liquidity.base_token.parse().unwrap();
    let quote: Address = c.liquidity.quote_token.parse().unwrap();
    let state = Arc::new(Mutex::new(Chain {
        quote: U256::from(120_000_000),
        ..Default::default()
    }));
    let shared = state.clone();
    let mock=Mock::start(move |req| {
        let mut s=shared.lock().unwrap();
        let result=match req["method"].as_str().unwrap() {
            "eth_chainId"=>json!("0x2105"),
            "eth_blockNumber"=>json!("0x100"),
            "eth_getBlockByNumber"=>json!({"hash":"0xcanonical","timestamp":format!("0x{:x}",lp_maker::now_ms()/1000),"baseFeePerGas":"0x1"}),
            "eth_getTransactionCount"=>json!(format!("0x{:x}",s.sent.len())),
            "eth_gasPrice"|"eth_maxPriorityFeePerGas"=>json!("0x1"),
            "eth_estimateGas"=>json!("0x30d40"),
            "eth_getBalance"=>json!("0xde0b6b3a7640000"),
            "eth_getTransactionReceipt"=>s.receipts.get(req["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null),
            "eth_call"=>{
                let data=hex::decode(req["params"][0]["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let to:Address=req["params"][0]["to"].as_str().unwrap().parse().unwrap();
                if has(&data,"slot0()") {words(&[U256::from_str_radix("3953120541360100857610261",10).unwrap(),tick_word(-198122),U256::ZERO,U256::ZERO,U256::ZERO,U256::ZERO,U256::from(1)])}
                else if has(&data,"liquidity()") {words(&[U256::from(1_000_000_000_000_u64)])}
                else if has(&data,"tickSpacing()") {words(&[U256::from(60)])}
                else if has(&data,"balanceOf(address)") {words(&[if to==base{s.base}else{assert_eq!(to,quote);s.quote}])}
                else if has(&data,"allowance(address,address)") {words(&[U256::MAX])}
                else if has(&data,"positions(uint256)") {words(&s.position)}
                else if has(&data,"getL1FeeUpperBound(uint256)")||has(&data,"getOperatorFee(uint256)") {words(&[U256::from(100)])}
                else if has(&data,"quoteExactInputSingle((address,address,uint256,uint24,uint160))") {
                    assert_eq!(U256::from_be_slice(&data[100..132]),U256::from(3000));
                    words(&[U256::from(16_000_000_000_000_000_u64),U256::from(1),U256::ZERO,U256::from(80_000)])
                } else {assert!(to==router||to==manager);json!("0x")} // Simulation only.
            }
            "eth_sendRawTransaction"=>{
                let bytes=hex::decode(req["params"][0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let tx=TxEnvelope::decode_2718(&mut bytes.as_slice()).unwrap();
                assert_eq!(tx.chain_id(),Some(8453));assert_eq!(tx.nonce(),s.sent.len() as u64);
                let mut logs=vec![];
                if tx.to()==Some(router) {
                    let outer=IRouter02::multicallCall::abi_decode(tx.input()).unwrap();
                    let swap=IRouter02::exactInputSingleCall::abi_decode(&outer.data[0]).unwrap();
                    assert_eq!(swap.params.tokenIn,quote);assert_eq!(swap.params.tokenOut,base);
                    assert_eq!(swap.params.fee.to::<u32>(),3000);
                    assert_eq!(swap.params.amountOutMinimum,U256::from(15_952_000_000_000_000_u64));
                    assert!(swap.params.amountIn<=s.quote);s.quote-=swap.params.amountIn;
                    s.base+=U256::from(16_000_000_000_000_000_u64);
                } else {
                    assert_eq!(tx.to(),Some(manager));
                    if tx.input().starts_with(&INfpm::mintCall::SELECTOR) {
                        let mint=INfpm::mintCall::abi_decode(tx.input()).unwrap();
                        assert_eq!(mint.params.token0,base);assert_eq!(mint.params.token1,quote);
                        assert_eq!(mint.params.fee.to::<u32>(),3000);
                        assert_eq!(mint.params.tickLower.as_i32()%60,0);assert_eq!(mint.params.tickUpper.as_i32()%60,0);
                        assert!(mint.params.amount0Desired<=s.base && mint.params.amount1Desired<=s.quote);
                        assert!(mint.params.amount0Min>U256::ZERO && mint.params.amount1Min>U256::ZERO);
                        let liquidity = Range::new(mint.params.tickLower.as_i32(), mint.params.tickUpper.as_i32()).unwrap()
                            .liquidity(parse_sqrt("3953120541360100857610261").unwrap(), mint.params.amount0Desired, mint.params.amount1Desired).unwrap();
                        s.position = vec![U256::ZERO, U256::ZERO, address_word(base), address_word(quote), U256::from(3000),
                            tick_word(mint.params.tickLower.as_i32()), tick_word(mint.params.tickUpper.as_i32()), liquidity,
                            U256::ZERO, U256::ZERO, U256::ZERO, U256::ZERO];
                        s.base-=mint.params.amount0Desired;s.quote-=mint.params.amount1Desired;
                        logs.push(json!({"address":manager,"topics":[format!("{:#x}",keccak256("Transfer(address,address,uint256)")),format!("0x{:064x}",U256::ZERO),format!("0x{:0>64}",hex::encode(mint.params.recipient)),format!("0x{:064x}",U256::from(42))]}));
                    } else {
                        let remove=INfpm::multicallCall::abi_decode(tx.input()).unwrap();
                        assert_eq!(remove.data.len(),3);
                        let decrease=INfpm::decreaseLiquidityCall::abi_decode(&remove.data[0]).unwrap();
                        assert_eq!(decrease.params.tokenId,U256::from(42));assert!(decrease.params.liquidity>0);
                        assert_eq!(INfpm::collectCall::abi_decode(&remove.data[1]).unwrap().params.tokenId,U256::from(42));
                        assert_eq!(INfpm::burnCall::abi_decode(&remove.data[2]).unwrap().tokenId,U256::from(42));
                    }
                }
                let hash=format!("{:#x}",keccak256(&bytes));
                s.receipts.insert(hash.clone(),json!({"transactionHash":hash,"blockNumber":"0x1","blockHash":"0xcanonical","status":"0x1","logs":logs}));
                s.sent.push(tx);json!(hash)
            }
            method=>panic!("unexpected RPC {method}"),
        };
        Ok(result)
    }).await;
    c.liquidity.rpc_url = mock.url.clone();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let signer = PrivateKeySigner::from_bytes(&B256::from([1u8; 32])).unwrap();
    let ex = Executor::with_signer(
        liquidity::connect(c.liquidity.clone()).unwrap(),
        store.clone(),
        signer.clone(),
    )
    .unwrap();
    ex.swap(false, 40.0).await.unwrap();
    ex.mint("core", 79.0, 0.08).await.unwrap();
    assert_eq!(ex.ids().unwrap(), vec![("core".into(), "42".into())]);
    assert!(store.pending().unwrap().is_none());
    let input = state.lock().unwrap().sent[1].input().clone();
    let mint = INfpm::mintCall::abi_decode(&input).unwrap();
    let lower = math::tick_to_price(mint.params.tickLower.as_i32(), 18, 6, true);
    let upper = math::tick_to_price(mint.params.tickUpper.as_i32(), 18, 6, true);
    let price = ex.venue.snapshot().await.unwrap().price;
    let b = liquidity::uniswap_v3::units(mint.params.amount0Desired, 18).unwrap();
    let q = liquidity::uniswap_v3::units(mint.params.amount1Desired, 6).unwrap();
    let l = math::liquidity_for_value(b * price + q, lower, upper, price).unwrap();
    let position = LpPosition {
        layer: "core".into(),
        token_id: Some("42".into()),
        lower,
        upper,
        liquidity: l,
        raw_liquidity: state.lock().unwrap().position[7].to_string(),
        unclaimed_base: 0.0,
        unclaimed_quote: 0.0,
        base: b,
        quote: q,
    };
    drop(ex);
    drop(store);
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let ex = Executor::with_signer(
        liquidity::connect(c.liquidity).unwrap(),
        store.clone(),
        signer,
    )
    .unwrap();
    assert_eq!(ex.ids().unwrap().len(), 1);
    ex.remove(&position).await.unwrap();
    assert!(ex.ids().unwrap().is_empty());
    assert!(store.pending().unwrap().is_none());
    assert_eq!(ex.nonce.next().await.unwrap(), 3);
    assert_eq!(state.lock().unwrap().sent.len(), 3);
    assert!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "eth_sendRawTransaction")
    );
}

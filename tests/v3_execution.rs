//! 本地 RPC 故障注入：只用公开测试密钥，绝不加载 .env 或连接真实链。
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
    liquidity::uniswap_v3::{
        UniswapV3,
        abi::{IERC20, INfpm},
        slippage::{Range, sqrt_at_tick},
        tx::Executor,
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
        v.iter().map(|v| format!("{v:064x}")).collect::<String>()
    ))
}
fn has(data: &[u8], signature: &str) -> bool {
    data.starts_with(&keccak256(signature)[..4])
}

struct Chain {
    sent: Vec<TxEnvelope>,
    receipts: BTreeMap<String, Value>,
    approved: Vec<Address>,
    tick: i32,
    approval_jump: i32,
    reject_simulation: bool,
    uncertain_broadcast: bool,
    position: Vec<U256>,
}
struct Fixture {
    ex: Executor,
    state: Arc<Mutex<Chain>>,
    mock: Mock,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new(base_chain: bool) -> Self {
        let mut c = Config::load(if base_chain {
            "config/base.toml"
        } else {
            "config/paper-200.toml"
        })
        .unwrap();
        let cfg = c.liquidity.clone();
        let base: Address = cfg.base_token.parse().unwrap();
        let quote: Address = cfg.quote_token.parse().unwrap();
        let manager: Address = cfg.position_manager.parse().unwrap();
        let state = Arc::new(Mutex::new(Chain {
            sent: vec![],
            receipts: BTreeMap::new(),
            approved: vec![],
            tick: -198100,
            approval_jump: 10,
            reject_simulation: false,
            uncertain_broadcast: false,
            position: vec![],
        }));
        let shared = state.clone();
        let mock = Mock::start(move |req| {
            let mut s = shared.lock().unwrap();
            let result = match req["method"].as_str().unwrap() {
                "eth_chainId" => json!(format!("0x{:x}", cfg.chain_id)),
                "eth_blockNumber" => json!(format!("0x{:x}", 256+s.sent.len())),
                "eth_getBlockByNumber" => json!({"hash":"0xcanonical","timestamp":format!("0x{:x}",lp_maker::now_ms()/1000),"baseFeePerGas":"0x1"}),
                "eth_getTransactionCount" => json!(format!("0x{:x}",s.sent.len())),
                "eth_gasPrice"|"eth_maxPriorityFeePerGas" => json!("0x1"),
                "eth_estimateGas" => json!("0x30d40"),
                "eth_getBalance" => json!("0xde0b6b3a7640000"),
                "eth_getTransactionReceipt" => s.receipts.get(req["params"][0].as_str().unwrap()).cloned().unwrap_or(Value::Null),
                "eth_call" => {
                    let data = hex::decode(req["params"][0]["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
                    let to: Address = req["params"][0]["to"].as_str().unwrap().parse().unwrap();
                    if has(&data,"slot0()") { words(&[sqrt_at_tick(s.tick).unwrap(), tick_word(s.tick), U256::ZERO, U256::ZERO, U256::ZERO, U256::ZERO, U256::from(1)]) }
                    else if has(&data,"liquidity()") { words(&[U256::from(1_000_000_000_000_u64)]) }
                    else if has(&data,"tickSpacing()") { words(&[U256::from(if base_chain {60} else {1})]) }
                    else if has(&data,"balanceOf(address)") { words(&[if to == base {U256::from(1_000_000_000_000_000_000_u64)} else {assert_eq!(to,quote); U256::from(1_000_000_000)}]) }
                    else if has(&data,"allowance(address,address)") { words(&[if s.approved.contains(&to) {U256::MAX} else {U256::ZERO}]) }
                    else if has(&data,"positions(uint256)") { words(&s.position) }
                    else if has(&data,"getL1FeeUpperBound(uint256)")||has(&data,"getOperatorFee(uint256)") { words(&[U256::from(100)]) }
                    else if to == manager {
                        if s.reject_simulation { return Err(json!({"code":3,"message":"execution reverted: Price slippage check"})); }
                        if data.starts_with(&INfpm::mintCall::SELECTOR) {
                            let p = INfpm::mintCall::abi_decode(&data).unwrap().params;
                            let r = Range::new(p.tickLower.as_i32(), p.tickUpper.as_i32()).unwrap();
                            // 报价后再移动约 0.03%，检查 calldata 的最低数量确实可以被满足。
                            let price = sqrt_at_tick(s.tick + 3).unwrap();
                            let l = r.liquidity(price,p.amount0Desired,p.amount1Desired).unwrap();
                            let used = r.amounts(price,l,true).unwrap();
                            assert!(used.0 >= p.amount0Min && used.1 >= p.amount1Min);
                        } else if data.starts_with(&INfpm::increaseLiquidityCall::SELECTOR) {
                            let p = INfpm::increaseLiquidityCall::abi_decode(&data).unwrap().params;
                            let r = Range::new(lp_maker::evm::rpc::signed_tick(s.position[5]),lp_maker::evm::rpc::signed_tick(s.position[6])).unwrap();
                            let price = sqrt_at_tick(s.tick - 3).unwrap();
                            let used = r.amounts(price,r.liquidity(price,p.amount0Desired,p.amount1Desired).unwrap(),true).unwrap();
                            assert!(used.0 >= p.amount0Min && used.1 >= p.amount1Min);
                        } else if data.starts_with(&INfpm::multicallCall::SELECTOR) {
                            let outer = INfpm::multicallCall::abi_decode(&data).unwrap();
                            let p = INfpm::decreaseLiquidityCall::abi_decode(&outer.data[0]).unwrap().params;
                            let r = Range::new(lp_maker::evm::rpc::signed_tick(s.position[5]),lp_maker::evm::rpc::signed_tick(s.position[6])).unwrap();
                            assert_eq!(U256::from(p.liquidity),s.position[7]);
                            let actual = r.amounts(sqrt_at_tick(s.tick + 3).unwrap(),s.position[7],false).unwrap();
                            assert!(actual.0 >= p.amount0Min && actual.1 >= p.amount1Min);
                            assert!(p.amount0Min > U256::ZERO && p.amount1Min > U256::ZERO);
                        }
                        json!("0x")
                    } else { assert!(data.starts_with(&IERC20::approveCall::SELECTOR)); json!("0x") }
                }
                "eth_sendRawTransaction" => {
                    let raw = hex::decode(req["params"][0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                    let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                    assert_eq!(tx.nonce(),s.sent.len() as u64);
                    let hash = format!("{:#x}",keccak256(&raw));
                    let mut logs = vec![];
                    if tx.to() != Some(manager) {
                        let approved = tx.to().unwrap();
                        assert!(approved == base || approved == quote);
                        assert!(tx.input().starts_with(&IERC20::approveCall::SELECTOR));
                        if s.approved.is_empty() { s.tick += s.approval_jump; }
                        s.approved.push(approved);
                    } else if tx.input().starts_with(&INfpm::mintCall::SELECTOR) {
                        let p = INfpm::mintCall::abi_decode(tx.input()).unwrap().params;
                        let r = Range::new(p.tickLower.as_i32(),p.tickUpper.as_i32()).unwrap();
                        let l = r.liquidity(sqrt_at_tick(s.tick).unwrap(),p.amount0Desired,p.amount1Desired).unwrap();
                        s.position = vec![U256::ZERO,U256::ZERO,address_word(base),address_word(quote),U256::from(cfg.fee),tick_word(p.tickLower.as_i32()),tick_word(p.tickUpper.as_i32()),l,U256::ZERO,U256::ZERO,U256::ZERO,U256::ZERO];
                        logs.push(json!({"address":manager,"topics":[format!("{:#x}",keccak256("Transfer(address,address,uint256)")),format!("0x{:064x}",U256::ZERO),format!("0x{:0>64}",hex::encode(p.recipient)),format!("0x{:064x}",U256::from(42))]}));
                    } else if tx.input().starts_with(&INfpm::increaseLiquidityCall::SELECTOR) {
                        let p = INfpm::increaseLiquidityCall::abi_decode(tx.input()).unwrap().params;
                        let r = Range::new(lp_maker::evm::rpc::signed_tick(s.position[5]),lp_maker::evm::rpc::signed_tick(s.position[6])).unwrap();
                        let delta = r.liquidity(sqrt_at_tick(s.tick).unwrap(),p.amount0Desired,p.amount1Desired).unwrap();
                        s.position[7] += delta;
                    } else {
                        assert!(tx.input().starts_with(&INfpm::multicallCall::SELECTOR));
                        s.position[7] = U256::ZERO;
                    }
                    let uncertain = tx.to() == Some(manager) && s.uncertain_broadcast;
                    s.sent.push(tx);
                    if uncertain { return Err(json!({"code":-32000,"message":"broadcast result unavailable"})); }
                    s.receipts.insert(hash.clone(),json!({"transactionHash":hash,"blockNumber":"0x1","blockHash":"0xcanonical","status":"0x1","logs":logs}));
                    json!(hash)
                }
                other => panic!("unexpected RPC {other}"),
            };
            Ok(result)
        }).await;
        c.liquidity.rpc_url = mock.url.clone();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let ex = Executor::with_signer(
            UniswapV3::new(c.liquidity).unwrap(),
            store,
            PrivateKeySigner::from_bytes(&B256::from([1_u8; 32])).unwrap(),
        )
        .unwrap();
        Self {
            ex,
            state,
            mock,
            _dir: dir,
        }
    }

    fn position(&self) -> LpPosition {
        let s = self.state.lock().unwrap();
        let p = &s.position;
        LpPosition {
            layer: "satellite".into(),
            token_id: Some("42".into()),
            lower: math::tick_to_price(lp_maker::evm::rpc::signed_tick(p[5]), 18, 6, true),
            upper: math::tick_to_price(lp_maker::evm::rpc::signed_tick(p[6]), 18, 6, true),
            liquidity: 0.0,
            raw_liquidity: p[7].to_string(), // 故意不给可依赖的展示浮点流动性。
            unclaimed_base: 0.0,
            unclaimed_quote: 0.0,
            base: 0.0,
            quote: 0.0,
        }
    }
}

#[tokio::test]
async fn robinhood_and_base_refresh_after_approvals_then_mint_increase_and_remove() {
    for base in [false, true] {
        let f = Fixture::new(base).await;
        // 监控保留确认块，执行报价使用最新块。
        assert_eq!(f.ex.venue.snapshot().await.unwrap().block, 244);
        assert_eq!(f.ex.venue.execution_snapshot().await.unwrap().block, 256);
        f.ex.mint("satellite", 40.0, 0.02).await.unwrap();
        assert_eq!(f.ex.ids().unwrap(), vec![("satellite".into(), "42".into())]);
        assert_eq!(f.state.lock().unwrap().sent.len(), 3);
        let p = f.position();
        f.ex.increase(&p, 10.0).await.unwrap();
        assert!(
            f.ex.remove(&p)
                .await
                .unwrap_err()
                .to_string()
                .contains("liquidity changed")
        );
        assert_eq!(f.state.lock().unwrap().sent.len(), 4);
        f.ex.remove(&f.position()).await.unwrap();
        assert!(f.ex.ids().unwrap().is_empty());
        assert!(f.ex.store.pending().unwrap().is_none());
        assert_eq!(f.ex.nonce.next().await.unwrap(), 5);
        let requests = f.mock.requests.lock().unwrap();
        let last_approval = requests
            .iter()
            .rposition(|r| {
                r["method"] == "eth_sendRawTransaction"
                    && TxEnvelope::decode_2718(
                        &mut hex::decode(r["params"][0].as_str().unwrap().trim_start_matches("0x"))
                            .unwrap()
                            .as_slice(),
                    )
                    .unwrap()
                    .input()
                    .starts_with(&IERC20::approveCall::SELECTOR)
            })
            .unwrap();
        assert!(requests[last_approval + 1..].iter().any(|r| {
            r["method"] == "eth_call"
                && r["params"][0]["data"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("0x{}", hex::encode(&keccak256("slot0()")[..4])))
        }));
    }
}

#[tokio::test]
async fn preflight_revert_keeps_prior_approvals_old_nft_workflow_and_nonce() {
    let f = Fixture::new(false).await;
    f.state.lock().unwrap().reject_simulation = true;
    f.ex.store.write("nfts.json", &json!({"core":"7"})).unwrap();
    let workflow = json!({"decision":"Deploy","started_ms":123});
    f.ex.store.write("workflow.json", &workflow).unwrap();
    let error = f.ex.mint("satellite", 40.0, 0.02).await.unwrap_err();
    assert!(format!("{error:#}").contains("before signing/broadcast"));
    assert!(f.ex.store.pending().unwrap().is_none());
    assert_eq!(f.ex.nonce.next().await.unwrap(), 2);
    assert_eq!(f.state.lock().unwrap().sent.len(), 2);
    assert_eq!(f.ex.ids().unwrap(), vec![("core".into(), "7".into())]);
    assert_eq!(
        f.ex.store.read::<Value>("workflow.json").unwrap(),
        Some(workflow)
    );
}

#[tokio::test]
async fn excessive_approval_price_move_stops_without_broadcasting_lp() {
    let f = Fixture::new(false).await;
    f.state.lock().unwrap().approval_jump = 50;
    let error = f.ex.mint("satellite", 40.0, 0.02).await.unwrap_err();
    assert!(format!("{error:#}").contains("during approvals"));
    assert_eq!(f.state.lock().unwrap().sent.len(), 2);
    assert!(f.ex.store.pending().unwrap().is_none());
    assert_eq!(f.ex.nonce.next().await.unwrap(), 2);
}

#[tokio::test]
async fn broadcast_uncertainty_stays_persisted_and_blocks_duplicate_mint() {
    let f = Fixture::new(false).await;
    f.state.lock().unwrap().uncertain_broadcast = true;
    assert!(f.ex.mint("satellite", 40.0, 0.02).await.is_err());
    let pending = f.ex.store.pending().unwrap().unwrap();
    assert_eq!(pending["operation"]["kind"], "mint");
    assert!(f.ex.mint("satellite", 40.0, 0.02).await.is_err());
    assert_eq!(f.ex.store.pending().unwrap().unwrap(), pending);
    assert_eq!(f.state.lock().unwrap().sent.len(), 3);
}

#[tokio::test]
async fn bounded_robinhood_mint_keeps_inward_ticks_and_persists_actual_nft() {
    use lp_maker::domain::LiquidityExecutor;
    let f = Fixture::new(false).await;
    let snap = f.ex.venue.execution_snapshot().await.unwrap();
    let base =
        f.ex.bounded_base_requirement(119., (0.07825, 0.08), &snap)
            .unwrap();
    assert!(base > 0. && base * snap.price < 119.);
    f.ex.mint_bounded_layer("band", 119., (0.07825, 0.08))
        .await
        .unwrap();
    assert_eq!(f.ex.ids().unwrap(), vec![("band".into(), "42".into())]);
    {
        let state = f.state.lock().unwrap();
        let tx = state.sent.last().unwrap();
        let p = INfpm::mintCall::abi_decode(tx.input()).unwrap().params;
        let lo = math::tick_to_price(p.tickLower.as_i32(), 18, 6, true);
        let hi = math::tick_to_price(p.tickUpper.as_i32(), 18, 6, true);
        assert!(lo >= snap.price * (1. - 0.07825));
        assert!(hi <= snap.price * 1.08);
        assert!(p.amount0Min > U256::ZERO && p.amount1Min > U256::ZERO);
        assert_eq!(state.sent.len(), 3); // two approvals followed by one mint
    }
    assert!(f.ex.store.pending().unwrap().is_none());
    // 持久化NFT必须阻止重复mint。
    assert!(
        f.ex.mint_bounded_layer("band", 119., (0.07825, 0.08))
            .await
            .is_err()
    );
}

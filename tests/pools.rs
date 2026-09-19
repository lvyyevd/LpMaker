mod support;
use alloy::{
    primitives::{Address, B256, U256, keccak256},
    signers::local::PrivateKeySigner,
};
use lp_maker::{
    config::{Config, Mode},
    domain::{LiquidityExecutor, LiquidityVenue, PoolSnapshot},
    engine::transport_independent_fingerprint,
    evm::rpc::{Rpc, address_word},
    liquidity::{
        self, chains,
        uniswap_v3::{mint, tx::Executor},
    },
    monitor::{denomination, display},
    recovery,
    store::Store,
    strategy::{EntryHistory, Phase},
};
use serde_json::{Value, json};
use std::sync::Arc;
use support::rpc::Mock;

fn base() -> Config {
    Config::load("config/base.toml").unwrap()
}
fn robinhood() -> Config {
    Config::load("config/paper-200.toml").unwrap()
}
fn words(values: &[U256]) -> Value {
    json!(format!(
        "0x{}",
        values
            .iter()
            .map(|n| format!("{n:064x}"))
            .collect::<String>()
    ))
}
fn selector(signature: &str) -> String {
    format!("0x{}", hex::encode(&keccak256(signature)[..4]))
}
fn fingerprint(c: &Config) -> Value {
    transport_independent_fingerprint(&json!({"mode":c.mode,"liquidity":c.liquidity,"strategy":c.strategy,"hyperliquid":c.hyperliquid}).to_string()).unwrap()
}

#[test]
fn pre_refactor_config_fingerprint_still_matches_and_base_cannot_reuse_it() {
    let old = transport_independent_fingerprint(include_str!("fixtures/robinhood_config_v1.json"))
        .unwrap();
    assert_eq!(old, fingerprint(&robinhood()));
    assert_ne!(old, fingerprint(&base()));
    // A neutral monitoring alias must not change stored economic/account identity.
    let old_cfg = std::fs::read_to_string("config/paper-200.toml").unwrap();
    let alias_cfg = old_cfg
        .replace("robinhood_interval_seconds", "liquidity_interval_seconds")
        .replace("robinhood_refresh_seconds", "liquidity_refresh_seconds");
    let alias: Config = toml::from_str(&alias_cfg).unwrap();
    assert_eq!(old, fingerprint(&alias));
}
fn snapshot() -> PoolSnapshot {
    PoolSnapshot {
        block: 42,
        block_hash: "fixed".into(),
        time_ms: lp_maker::now_ms(),
        price: 2483.17,
        tick: -198140,
        tick_spacing: 60,
        liquidity: "1000000000000".into(),
        sqrt_price_x96: "1".into(),
        base_is_token0: true,
    }
}

#[test]
fn profiles_keep_independent_state_and_the_same_strategy_parameters() {
    let b = base();
    let r = robinhood();
    assert_eq!(b.mode, Mode::Paper);
    assert_eq!(
        serde_json::to_value(&b.strategy).unwrap(),
        serde_json::to_value(&r.strategy).unwrap()
    );
    assert_ne!(b.state_dir, r.state_dir);
    assert_ne!(b.hyperliquid.private_key_env, r.hyperliquid.private_key_env);
    assert_eq!(chains::pool(&b.liquidity).unwrap().tick_spacing, 60);
    assert_eq!(chains::labels(&b.liquidity)["quote_symbol"], "USDC");
    assert_eq!(chains::monitor_file(&r.liquidity), "monitor_robinhood.json");
    assert_eq!(chains::monitor_file(&b.liquidity), "monitor_liquidity.json");
    assert_eq!(b.monitoring.robinhood_interval_seconds, 60);
    assert!(
        liquidity::connect(b.liquidity.clone())
            .unwrap()
            .quoter()
            .unwrap()
            .is_some()
    );
    assert!(
        liquidity::connect(r.liquidity.clone())
            .unwrap()
            .quoter()
            .unwrap()
            .is_none()
    );
    assert_ne!(fingerprint(&b), fingerprint(&r));
    let mut wrong = b.clone();
    wrong.liquidity.fee = 100;
    assert!(wrong.validate().is_err());
    let mut wrong = b.clone();
    wrong.liquidity.quote_token = r.liquidity.quote_token;
    assert!(wrong.validate().is_err());
    let mut wrong = b;
    wrong.hyperliquid.hedge_coin = "BTC".into();
    assert!(wrong.validate().is_err());
}

#[tokio::test]
async fn base_contract_validation_matches_deployed_interfaces_and_rejects_wrong_pool() {
    for fee in [3000, 100] {
        let mut c = base();
        let chain = &chains::base::CHAIN;
        let pool = &chains::base::pools::weth_usdc::POOL;
        let mock = Mock::start(move |req| match req["method"].as_str().unwrap() {
            "eth_chainId" => Ok(json!("0x2105")),
            "eth_getCode" => Ok(json!("0x6000")),
            "eth_call" => {
                let input = req["params"][0]["data"].as_str().unwrap();
                let to: Address = req["params"][0]["to"].as_str().unwrap().parse().unwrap();
                let value = if input.starts_with(&selector("token0()")) {
                    address_word(pool.base_token.parse().unwrap())
                } else if input.starts_with(&selector("token1()")) {
                    address_word(pool.quote_token.parse().unwrap())
                } else if input.starts_with(&selector("fee()")) {
                    U256::from(fee)
                } else if input.starts_with(&selector("factory()")) {
                    address_word(chain.factory.parse().unwrap())
                } else if input.starts_with(&selector("getPool(address,address,uint24)")) {
                    address_word(pool.address.parse().unwrap())
                } else if input.starts_with(&selector("decimals()")) {
                    U256::from(if to == pool.base_token.parse::<Address>().unwrap() {
                        18
                    } else {
                        6
                    })
                } else if input.starts_with(&selector("tickSpacing()")) {
                    U256::from(60)
                } else {
                    panic!("unexpected or incorrect interface: {req}")
                };
                Ok(words(&[value]))
            }
            _ => panic!("validation must be read-only"),
        })
        .await;
        c.liquidity.rpc_url = mock.url.clone();
        let result = liquidity::connect(c.liquidity).unwrap().validate().await;
        assert_eq!(result.is_ok(), fee == 3000);
    }
}

#[tokio::test]
async fn base_swap_minimum_includes_pool_fee_before_slippage_and_pins_quote_block() {
    let mock = Mock::start(|req| {
        assert_eq!(req["method"], "eth_call");
        assert_eq!(req["params"][1], "0x2a");
        let raw = hex::decode(
            req["params"][0]["data"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        assert_eq!(
            &raw[..4],
            &keccak256("quoteExactInputSingle((address,address,uint256,uint24,uint160))")[..4]
        );
        assert_eq!(
            U256::from_be_slice(&raw[68..100]),
            U256::from(40_000_000_000_000_000_u64)
        );
        assert_eq!(U256::from_be_slice(&raw[100..132]), U256::from(3000));
        Ok(words(&[
            U256::from(99_650_000),
            U256::from(1),
            U256::ZERO,
            U256::from(80_000),
        ]))
    })
    .await;
    let mut c = base();
    c.liquidity.rpc_url = mock.url.clone();
    let v = liquidity::connect(c.liquidity).unwrap();
    let min = v
        .minimum_swap_output(
            v.base,
            v.quote,
            U256::from(40_000_000_000_000_000_u64),
            42,
            100.0,
            6,
        )
        .await
        .unwrap();
    assert_eq!(min, U256::from(99_351_050));
    assert!(min < U256::from(99_700_000)); // Spot less 0.3% would wrongly reject a valid fee-paying swap.
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    let mut r = robinhood();
    r.liquidity.rpc_url = mock.url.clone();
    let v = liquidity::connect(r.liquidity).unwrap();
    assert_eq!(
        v.minimum_swap_output(v.base, v.quote, U256::from(1), 42, 100.0, 6)
            .await
            .unwrap(),
        U256::from(99_700_000)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1); // Original route performs no new quotation request.
}

#[tokio::test]
async fn failed_or_empty_quote_never_falls_back_to_spot_price() {
    for result in [
        Err(json!({"code":3,"message":"reverted"})),
        Ok(words(&[U256::ZERO; 4])),
    ] {
        let mock = Mock::start(move |_| result.clone()).await;
        let mut c = base();
        c.liquidity.rpc_url = mock.url.clone();
        let v = liquidity::connect(c.liquidity).unwrap();
        assert!(
            v.minimum_swap_output(v.base, v.quote, U256::from(10), 42, 100.0, 6)
                .await
                .is_err()
        );
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r["method"] == "eth_call")
        );
    }
}

#[test]
fn base_tick_spacing_uses_real_inventory_ratio_and_caps_amounts_without_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(dir.path()).unwrap());
    let signer = PrivateKeySigner::from_bytes(&B256::from([1u8; 32])).unwrap();
    let c = base();
    let snap = snapshot();
    let ex =
        Executor::with_signer(liquidity::connect(c.liquidity.clone()).unwrap(), s, signer).unwrap();
    let (b, q) = mint::aligned_amounts(&c.liquidity, &snap, 40.0, 0.02).unwrap();
    assert!((b * snap.price + q - 40.0).abs() < 1e-9);
    assert!((b * snap.price - 20.0).abs() > 0.1);
    assert_eq!(ex.mint_base_requirement(40.0, 0.02, &snap).unwrap(), b);
    let (rb, rq) =
        mint::fit_balances(U256::MAX, U256::MAX, U256::from(100), U256::from(90)).unwrap();
    assert_eq!((rb, rq), (U256::from(90), U256::from(90)));
    assert!(mint::fit_balances(U256::from(1), U256::from(1), U256::ZERO, U256::from(1)).is_err());
}

#[tokio::test]
async fn base_l1_and_operator_fees_receive_headroom_and_failed_queries_block() {
    let mock = Mock::start(|req| {
        let input = req["params"][0]["data"].as_str().unwrap();
        if input.starts_with(&selector("getL1FeeUpperBound(uint256)")) {
            assert_eq!(
                U256::from_str_radix(&input[10..], 16).unwrap(),
                U256::from(356)
            );
            Ok(words(&[U256::from(100)]))
        } else {
            assert!(input.starts_with(&selector("getOperatorFee(uint256)")));
            Ok(words(&[U256::from(5)]))
        }
    })
    .await;
    assert_eq!(
        chains::base::fees::extra_fee(&Rpc::new(mock.url.clone()).unwrap(), 100, 21000, 10000)
            .await
            .unwrap(),
        U256::from(210)
    );
    let mock = Mock::start(|_| Err(json!({"code":3,"message":"Oracle unavailable"}))).await;
    assert!(
        chains::base::fees::extra_fee(&Rpc::new(mock.url.clone()).unwrap(), 100, 21000, 10000)
            .await
            .is_err()
    );
}

#[test]
fn base_reports_use_usdc_and_robinhood_reports_keep_legacy_keys() {
    let mut report = json!({"market":chains::labels(&base().liquidity),"mode":"live","snapshot":{
        "pool":{"price":2500},"positions":[{"layer":"core","principal_value_usdg":80,"unclaimed_fees_usdg":0.01}],
        "fee_apr_1h":{"apr_pct":10,"fees_usdg":0.01,"average_principal_usdg":80,"observed_seconds":3600,"complete":true}},
        "recent_volume":{"window_seconds":300,"volume_usdg":1000,"volume_weth":0.4,"swap_count":5,"complete":true}});
    let mut legacy = report.clone();
    legacy["market"] = chains::labels(&robinhood().liquidity);
    denomination::normalize(&mut report, false);
    denomination::normalize(&mut legacy, true);
    assert!(!report.to_string().contains("usdg"));
    let text = display::liquidity(&report);
    assert!(text.contains("Base LP 状态"));
    assert!(text.contains("USDC"));
    assert!(!text.contains("USDG"));
    assert!(
        legacy["snapshot"]["positions"][0]
            .get("principal_value_usdg")
            .is_some()
    );
    assert!(display::robinhood(&legacy).contains("Robinhood LP 状态"));
}

#[test]
fn old_live_checkpoint_retains_nfts_protection_pause_and_pending_on_reload() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let mut c = robinhood();
    c.mode = Mode::Live;
    let cp: Value =
        serde_json::from_str(include_str!("fixtures/robinhood_checkpoint_v1.json")).unwrap();
    s.write("checkpoint.json", &cp).unwrap();
    s.write(
        "nfts.json",
        &json!({"core":"1214297","satellite":"1214301"}),
    )
    .unwrap();
    let pending =
        json!({"venue":"evm","hash":"0xoriginal","nonce":219,"operation":{"kind":"mint"}});
    s.write("pending.json", &pending).unwrap();
    let (strategy, _) = recovery::load(&s, &c).unwrap();
    assert_eq!(strategy.phase, Phase::Paused);
    assert_eq!(strategy.entry_history, EntryHistory::Established);
    assert_eq!(strategy.pause_since, 1789713119728);
    assert!(strategy.layers["satellite"].protected);
    assert_eq!(strategy.fraction, 0.25);
    assert_eq!(strategy.peak_equity, 388.419245);
    assert_eq!(s.pending().unwrap().unwrap(), pending);
    assert_eq!(
        s.read::<Value>("nfts.json").unwrap().unwrap()["core"],
        "1214297"
    );
    assert_eq!(s.read::<Value>("checkpoint.json").unwrap().unwrap(), cp);
}

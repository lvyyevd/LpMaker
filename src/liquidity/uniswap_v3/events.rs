use super::rpc::signed_tick;
use alloy::primitives::{I256, U256, keccak256};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// Original log identity is retained for deduplication: (blockHash, logIndex).
pub fn decode(log: &Value) -> Result<Value> {
    let topic = log["topics"][0].as_str().context("event topic missing")?;
    let data = hex::decode(
        log["data"]
            .as_str()
            .context("event data missing")?
            .trim_start_matches("0x"),
    )?;
    ensure!(data.len() % 32 == 0, "malformed pool event");
    let words = data.chunks(32).map(U256::from_be_slice).collect::<Vec<_>>();
    let is = |s: &str| topic.eq_ignore_ascii_case(&format!("{:#x}", keccak256(s)));
    let (kind, fields) = if is("Swap(address,address,int256,int256,uint160,uint128,int24)") {
        ensure!(words.len() == 5, "invalid Swap event");
        (
            "Swap",
            json!({"amount0":I256::from_raw(words[0]).to_string(),"amount1":I256::from_raw(words[1]).to_string(),"sqrtPriceX96":words[2].to_string(),"liquidity":words[3].to_string(),"tick":signed_tick(words[4])}),
        )
    } else if is("Mint(address,address,int24,int24,uint128,uint256,uint256)") {
        ensure!(words.len() == 4, "invalid Mint event");
        (
            "Mint",
            json!({"liquidity":words[1].to_string(),"amount0":words[2].to_string(),"amount1":words[3].to_string()}),
        )
    } else if is("Burn(address,int24,int24,uint128,uint256,uint256)") {
        ensure!(words.len() == 3, "invalid Burn event");
        (
            "Burn",
            json!({"liquidity":words[0].to_string(),"amount0":words[1].to_string(),"amount1":words[2].to_string()}),
        )
    } else if is("Collect(address,address,int24,int24,uint128,uint128)") {
        ensure!(words.len() == 3, "invalid Collect event");
        (
            "Collect",
            json!({"amount0":words[1].to_string(),"amount1":words[2].to_string()}),
        )
    } else {
        ("Other", json!({}))
    };
    Ok(
        json!({"kind":kind,"fields":fields,"blockHash":log["blockHash"],"blockNumber":log["blockNumber"],"logIndex":log["logIndex"],"transactionHash":log["transactionHash"],"removed":log["removed"],"raw":log}),
    )
}

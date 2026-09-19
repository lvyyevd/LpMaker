//! 只读验收：不会构造签名器或发送交易，可重复检查公开节点和合约。
use alloy::primitives::U256;
use anyhow::Result;
use lp_maker::{config::Config, domain::LiquidityVenue, liquidity};

#[tokio::main]
async fn main() -> Result<()> {
    let c = Config::load("config/base.toml")?;
    let venue = liquidity::connect(c.liquidity.clone())?;
    venue.validate().await?;
    let snapshot = venue.snapshot().await?;
    let bought = venue
        .quote_exact_input(
            venue.quote,
            venue.base,
            U256::from(10_000_000),
            snapshot.block,
        )
        .await?;
    let sold = venue
        .quote_exact_input(
            venue.base,
            venue.quote,
            U256::from(1_000_000_000_000_000_u64),
            snapshot.block,
        )
        .await?;
    let extra = liquidity::chains::base::fees::extra_fee(
        &venue.rpc,
        512,
        200_000,
        c.liquidity.gas_fee_buffer_bps,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "read_only":true,"market":liquidity::chains::labels(&c.liquidity),"snapshot":snapshot,
            "buy_with_10_usdc_weth":liquidity::uniswap_v3::units(bought,18)?,
            "sell_0_001_weth_usdc":liquidity::uniswap_v3::units(sold,6)?,
            "l1_and_operator_fee_reserve_eth":liquidity::uniswap_v3::units(extra,18)?
        }))?
    );
    Ok(())
}

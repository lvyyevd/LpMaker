//! 已在线运行的 WETH/USDG 0.01% 池；层名称 core/satellite 由共用策略配置决定。
use crate::liquidity::chains::{Pool, robinhood};
pub static POOL: Pool = Pool {
    chain: &robinhood::CHAIN,
    id: "robinhood/weth-usdg-100",
    address: "0x52e65B17fB6E5BA00Ed806f37Afcd2DaA50271Ca",
    base_token: "0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73",
    quote_token: "0x5fc5360d0400a0fd4f2af552add042d716f1d168",
    base_symbol: "WETH",
    quote_symbol: "USDG",
    hedge_coin: "ETH",
    base_decimals: 18,
    quote_decimals: 6,
    fee: 100,
    tick_spacing: 1,
};

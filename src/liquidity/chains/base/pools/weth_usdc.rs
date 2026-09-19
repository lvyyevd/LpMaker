//! 用户指定的 WETH/USDC 0.3% 池；它不是 Base 上的其他 WETH/USDC 费率池。
use crate::liquidity::chains::{Pool, base};
pub static POOL: Pool = Pool {
    chain: &base::CHAIN,
    id: "base/weth-usdc-3000",
    address: "0x6c561B446416E1A00E8E93E221854d6eA4171372",
    base_token: "0x4200000000000000000000000000000000000006",
    quote_token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    base_symbol: "WETH",
    quote_symbol: "USDC",
    hedge_coin: "ETH",
    base_decimals: 18,
    quote_decimals: 6,
    fee: 3000,
    tick_spacing: 60,
};

//! EVM 通用基础设施：RPC、费用估算与 nonce 生命周期。
//! 旧路径继续导出 V3 类型，兼容现有测试和外部调用；实现已归入 liquidity。
pub mod fees;
pub mod nonce;
pub mod rpc;
pub use crate::liquidity::uniswap_v3::{
    UniswapV3, abi, bounded_amount, events, raw_units, tx, units,
};

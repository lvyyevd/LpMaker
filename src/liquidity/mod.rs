//! 流动性接入层：链和池只描述部署事实，共用协议实现负责读写，策略保持独立。
pub mod chains;
pub mod uniswap_v3;

use crate::config::LiquidityConfig;
use anyhow::{Result, ensure};

/// 所有运行入口从这里选择协议；增加新协议时在此接入，不复制策略状态机。
pub fn connect(config: LiquidityConfig) -> Result<uniswap_v3::UniswapV3> {
    ensure!(config.kind == "uniswap_v3", "unsupported liquidity adapter");
    chains::validate_pool(&config)?;
    uniswap_v3::UniswapV3::new(config)
}

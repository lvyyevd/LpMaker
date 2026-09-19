//! 每条链一个目录、每个已支持池一个文件。这里只存部署信息，不存密钥或策略状态。
pub mod base;
pub mod robinhood;

use crate::config::LiquidityConfig;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub struct Chain {
    pub id: u64,
    pub name: &'static str,
    pub factory: &'static str,
    pub position_manager: &'static str,
    pub swap_router: &'static str,
    pub quoter_v2: Option<&'static str>,
}

pub struct Pool {
    pub chain: &'static Chain,
    pub id: &'static str,
    pub address: &'static str,
    pub base_token: &'static str,
    pub quote_token: &'static str,
    pub base_symbol: &'static str,
    pub quote_symbol: &'static str,
    pub hedge_coin: &'static str,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub fee: u32,
    pub tick_spacing: i32,
}

pub fn chain(id: u64) -> Option<&'static Chain> {
    match id {
        4663 => Some(&robinhood::CHAIN),
        8453 => Some(&base::CHAIN),
        _ => None,
    }
}

pub fn pool(c: &LiquidityConfig) -> Option<&'static Pool> {
    [
        &robinhood::pools::weth_usdg::POOL,
        &base::pools::weth_usdc::POOL,
    ]
    .into_iter()
    .find(|p| p.chain.id == c.chain_id && p.address.eq_ignore_ascii_case(&c.pool))
}

/// 已登记的池不允许静默换成其他代币、费率或路由；RPC 地址可由运维独立调整。
pub fn validate_pool(c: &LiquidityConfig) -> Result<()> {
    let Some(p) = pool(c) else {
        return Ok(());
    };
    for (actual, expected) in [
        (c.factory.as_str(), p.chain.factory),
        (&c.position_manager, p.chain.position_manager),
        (&c.swap_router, p.chain.swap_router),
        (&c.base_token, p.base_token),
        (&c.quote_token, p.quote_token),
    ] {
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "registered pool contract mismatch: {}",
            p.id
        );
    }
    ensure!(
        c.fee == p.fee
            && c.base_decimals == p.base_decimals
            && c.quote_decimals == p.quote_decimals,
        "registered pool fee/decimals mismatch: {}",
        p.id
    );
    Ok(())
}

/// 标签随池子解析，避免 Base 的 USDC 被中文日志误写成 USDG。
pub fn labels(c: &LiquidityConfig) -> Value {
    if let Some(p) = pool(c) {
        json!({"chain":p.chain.name,"chain_id":p.chain.id,"pool_id":p.id,"pool":p.address,
            "base_symbol":p.base_symbol,"quote_symbol":p.quote_symbol,"price_symbol":p.hedge_coin,"fee":p.fee})
    } else {
        json!({"chain":chain(c.chain_id).map(|c|c.name).unwrap_or("EVM"),"chain_id":c.chain_id,
            "pool":c.pool,"base_symbol":"基础币","quote_symbol":"报价币","price_symbol":"基础币","fee":c.fee})
    }
}

pub fn monitor_file(c: &LiquidityConfig) -> &'static str {
    // Robinhood 的旧状态文件名是兼容接口，不能因为目录重构而改名。
    if c.chain_id == robinhood::CHAIN.id {
        "monitor_robinhood.json"
    } else {
        "monitor_liquidity.json"
    }
}

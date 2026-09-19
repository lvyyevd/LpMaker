//! V3 建仓配比与 tick 对齐；只有新 Base 接入使用精确配比，旧链行为保持原样。
use crate::{config::LiquidityConfig, domain::PoolSnapshot, math};
use alloy::primitives::{U256, U512};
use anyhow::{Result, ensure};

pub fn aligned_amounts(
    c: &LiquidityConfig,
    s: &PoolSnapshot,
    value: f64,
    width: f64,
) -> Result<(f64, f64)> {
    let (lo, hi) = math::range(s.price, width);
    let (tl, tu) = math::aligned_ticks(
        lo,
        hi,
        s.tick_spacing,
        c.base_decimals,
        c.quote_decimals,
        s.base_is_token0,
    )?;
    let a = math::tick_to_price(tl, c.base_decimals, c.quote_decimals, s.base_is_token0);
    let b = math::tick_to_price(tu, c.base_decimals, c.quote_decimals, s.base_is_token0);
    let liquidity = math::liquidity_for_value(value, a.min(b), a.max(b), s.price)?;
    Ok(math::amounts(liquidity, a.min(b), a.max(b), s.price))
}

/// 换币会扣池费，且两笔交易间价格可能变化。按真实余额同比缩小额度，绝不超支。
/// 不用浮点处理代币整数，也不为完成建仓再盲目补买一次。
pub fn fit_balances(
    mut base: U256,
    mut quote: U256,
    available_base: U256,
    available_quote: U256,
) -> Result<(U256, U256)> {
    ensure!(
        base > U256::ZERO && quote > U256::ZERO,
        "mint requires two positive token amounts"
    );
    if base > available_base {
        quote = U256::from(U512::from(quote) * U512::from(available_base) / U512::from(base));
        base = available_base;
    }
    if quote > available_quote {
        base = U256::from(U512::from(base) * U512::from(available_quote) / U512::from(quote));
        quote = available_quote;
    }
    ensure!(
        base > U256::ZERO && quote > U256::ZERO,
        "insufficient token balances for mint"
    );
    Ok((base, quote))
}

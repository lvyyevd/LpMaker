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

/// 新策略专用：相对建仓报价的算术区间，tick 向内取整，不扩大8%边界。
/// 旧策略仍使用 aligned_amounts 的原有几何区间与对齐规则。
pub fn bounded_ticks(
    c: &LiquidityConfig,
    s: &PoolSnapshot,
    widths: (f64, f64),
) -> Result<(i32, i32)> {
    ensure!(
        s.tick_spacing > 0
            && [widths.0, widths.1]
                .iter()
                .all(|w| w.is_finite() && *w > 0.001 && *w <= 0.08),
        "invalid bounded LP widths"
    );
    let lo = s.price * (1. - widths.0);
    let hi = s.price * (1. + widths.1);
    let a = math::price_to_tick(lo, c.base_decimals, c.quote_decimals, s.base_is_token0)?;
    let b = math::price_to_tick(hi, c.base_decimals, c.quote_decimals, s.base_is_token0)?;
    let step = f64::from(s.tick_spacing);
    let tl = ((a.min(b) / step).ceil() * step) as i32;
    let tu = ((a.max(b) / step).floor() * step) as i32;
    let low = math::tick_to_price(tl, c.base_decimals, c.quote_decimals, s.base_is_token0);
    let high = math::tick_to_price(tu, c.base_decimals, c.quote_decimals, s.base_is_token0);
    ensure!(
        tl < tu
            && low.min(high) >= lo
            && low.max(high) <= hi
            && low.min(high) < s.price
            && low.max(high) > s.price,
        "tick spacing cannot fit bounded LP range"
    );
    Ok((tl, tu))
}
pub fn bounded_amounts(
    c: &LiquidityConfig,
    s: &PoolSnapshot,
    value: f64,
    widths: (f64, f64),
) -> Result<(f64, f64)> {
    let (tl, tu) = bounded_ticks(c, s, widths)?;
    let a = math::tick_to_price(tl, c.base_decimals, c.quote_decimals, s.base_is_token0);
    let b = math::tick_to_price(tu, c.base_decimals, c.quote_decimals, s.base_is_token0);
    let l = math::liquidity_for_value(value, a.min(b), a.max(b), s.price)?;
    Ok(math::amounts(l, a.min(b), a.max(b), s.price))
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    #[test]
    fn both_token_orders_round_inward_and_fit_all_prices() {
        let c = crate::config::Config::load("config/eth-band-paper.toml")
            .unwrap()
            .liquidity;
        for price in [100., 1500., 2700., 10000.] {
            for base_is_token0 in [true, false] {
                for tick_spacing in [1, 10, 60] {
                    let s = PoolSnapshot {
                        block: 1,
                        block_hash: "test".into(),
                        time_ms: 0,
                        price,
                        tick: 0,
                        tick_spacing,
                        liquidity: "0".into(),
                        sqrt_price_x96: "0".into(),
                        base_is_token0,
                    };
                    let (tl, tu) = bounded_ticks(&c, &s, (0.07825, 0.08)).unwrap();
                    let a =
                        math::tick_to_price(tl, c.base_decimals, c.quote_decimals, base_is_token0);
                    let b =
                        math::tick_to_price(tu, c.base_decimals, c.quote_decimals, base_is_token0);
                    assert!(a.min(b) >= price * (1. - 0.07825));
                    assert!(a.max(b) <= price * 1.08);
                    assert_eq!(tl % tick_spacing, 0);
                    assert_eq!(tu % tick_spacing, 0);
                    let (base, quote) = bounded_amounts(&c, &s, 119.4, (0.07825, 0.08)).unwrap();
                    assert!((base * price + quote - 119.4).abs() < 1e-8);
                    assert!(base > 0. && quote > 0.);
                }
            }
        }
    }
}

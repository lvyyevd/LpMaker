//! V3 专用滑点计算：滑点约束价格，不是把两笔计划投入量各打一个折扣。
//! 全部使用链上的 Q64.96 / 原始代币整数，避免窄区间和不同精度的舍入误差。
//! 算法对应 Uniswap v3-sdk Position 的 mint/burnAmountsWithSlippage；
//! TickMath 常量与舍入方式来自其 MIT 实现，许可证见 third_party/uniswap-v3-sdk.LICENSE。
use alloy::primitives::{U256, U512};
use anyhow::{Context, Result, ensure};

pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;

pub fn sqrt_at_tick(tick: i32) -> Result<U256> {
    ensure!((MIN_TICK..=MAX_TICK).contains(&tick), "invalid V3 tick");
    let factors = [
        "fffcb933bd6fad37aa2d162d1a594001",
        "fff97272373d413259a46990580e213a",
        "fff2e50f5f656932ef12357cf3c7fdcc",
        "ffe5caca7e10e4e61c3624eaa0941cd0",
        "ffcb9843d60f6159c9db58835c926644",
        "ff973b41fa98c081472e6896dfb254c0",
        "ff2ea16466c96a3843ec78b326b52861",
        "fe5dee046a99a2a811c461f1969c3053",
        "fcbe86c7900a88aedcffc83b479aa3a4",
        "f987a7253ac413176f2b074cf7815e54",
        "f3392b0822b70005940c7a398e4b70f3",
        "e7159475a2c29b7443b29c7fa6e889d9",
        "d097f3bdfd2022b8845ad8f792aa5825",
        "a9f746462d870fdf8a65dc1f90e061e5",
        "70d869a156d2a1b890bb3df62baf32f7",
        "31be135f97d08fd981231505542fcfa6",
        "9aa508b5b7a84e1c677de54f3e99bc9",
        "5d6af8dedb81196699c329225ee604",
        "2216e584f5fa1ea926041bedfe98",
        "48a170391f7dc42444e8fa2",
    ];
    let mut ratio = U256::from(1) << 128;
    for (bit, factor) in factors.iter().enumerate() {
        if tick.unsigned_abs() & (1 << bit) != 0 {
            ratio = (ratio * U256::from_str_radix(factor, 16)?) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    let rounded = ratio >> 32;
    Ok(rounded + U256::from((ratio & U256::from(u32::MAX)) != U256::ZERO))
}

fn narrow(n: U512) -> Result<U256> {
    ensure!(n <= U512::from(U256::MAX), "V3 amount overflow");
    Ok(U256::from(n))
}

fn divide(n: U512, d: U512, round_up: bool) -> Result<U256> {
    ensure!(d > U512::ZERO, "invalid V3 denominator");
    narrow(n / d + U512::from(round_up && n % d != U512::ZERO))
}

/// 用 512 位中间值求 sqrt(P × (1 ± 滑点))，并保持在 V3 可执行价格内。
pub fn price_bounds(sqrt: U256, bps: u32) -> Result<(U256, U256)> {
    let min = sqrt_at_tick(MIN_TICK)?;
    let max = sqrt_at_tick(MAX_TICK)?;
    ensure!(
        sqrt >= min && sqrt < max && bps < 10000,
        "invalid V3 price/slippage"
    );
    let square = U512::from(sqrt) * U512::from(sqrt);
    let at = |factor: u32| -> Result<U256> {
        narrow((square * U512::from(factor) / U512::from(10000)).root(2))
    };
    Ok((
        at(10000 - bps)?.max(min + U256::from(1)),
        at(10000 + bps)?.min(max - U256::from(1)),
    ))
}

/// 授权等待期间不能无限追价。原计划超出配置滑点后交回恢复流程，不追加买币。
pub fn check_price_move(before: U256, after: U256, bps: u32) -> Result<()> {
    let (lo, hi) = price_bounds(before, bps)?;
    ensure!(
        after >= lo && after <= hi,
        "LP price moved beyond configured slippage during approvals; no LP transaction sent"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct Range {
    lower: U256,
    upper: U256,
}

impl Range {
    pub fn new(lower_tick: i32, upper_tick: i32) -> Result<Self> {
        ensure!(lower_tick < upper_tick, "invalid V3 range");
        Ok(Self {
            lower: sqrt_at_tick(lower_tick)?,
            upper: sqrt_at_tick(upper_tick)?,
        })
    }

    pub fn contains(&self, sqrt: U256) -> bool {
        sqrt > self.lower && sqrt < self.upper
    }

    /// 对应 NFPM 的 LiquidityAmounts，amount0 的中间乘积必须先向下取整。
    pub fn liquidity(&self, sqrt: U256, amount0: U256, amount1: U256) -> Result<U256> {
        let q96 = U512::from(1) << 96;
        let l0 = |a: U256, b: U256| -> Result<U256> {
            let intermediate = U512::from(a) * U512::from(b) / q96;
            divide(U512::from(amount0) * intermediate, U512::from(b - a), false)
        };
        let l1 = |a: U256, b: U256| -> Result<U256> {
            divide(U512::from(amount1) * q96, U512::from(b - a), false)
        };
        let l = if sqrt <= self.lower {
            l0(self.lower, self.upper)?
        } else if sqrt >= self.upper {
            l1(self.lower, self.upper)?
        } else {
            l0(sqrt, self.upper)?.min(l1(self.lower, sqrt)?)
        };
        ensure!(l <= U256::from(u128::MAX), "V3 liquidity exceeds uint128");
        Ok(l)
    }

    /// mint/increase 时向上取整，burn 时向下取整；输入流动性是链上原始整数。
    pub fn amounts(&self, sqrt: U256, l: U256, round_up: bool) -> Result<(U256, U256)> {
        ensure!(l <= U256::from(u128::MAX), "V3 liquidity exceeds uint128");
        let p = sqrt.clamp(self.lower, self.upper);
        let q96 = U512::from(1) << 96;
        let a0 = divide(
            U512::from(l) * q96 * U512::from(self.upper - p),
            U512::from(self.upper) * U512::from(p),
            round_up,
        )?;
        let a1 = divide(U512::from(l) * U512::from(p - self.lower), q96, round_up)?;
        Ok((a0, a1))
    }

    pub fn mint_minimums(
        &self,
        sqrt: U256,
        amount0: U256,
        amount1: U256,
        bps: u32,
    ) -> Result<(U256, U256)> {
        ensure!(
            self.contains(sqrt),
            "LP entry price is outside planned ticks"
        );
        let l = self.liquidity(sqrt, amount0, amount1)?;
        ensure!(l > U256::ZERO, "zero V3 entry liquidity");
        let mins = self.minimums(sqrt, l, bps, true)?;
        ensure!(
            mins.0 > U256::ZERO || mins.1 > U256::ZERO,
            "LP range too narrow for configured slippage"
        );
        ensure!(
            mins.0 <= amount0 && mins.1 <= amount1,
            "invalid LP minimum amounts"
        );
        Ok(mins)
    }

    pub fn burn_minimums(&self, sqrt: U256, l: U256, bps: u32) -> Result<(U256, U256)> {
        self.minimums(sqrt, l, bps, false)
    }

    fn minimums(&self, sqrt: U256, l: U256, bps: u32, round_up: bool) -> Result<(U256, U256)> {
        let (lo, hi) = price_bounds(sqrt, bps)?;
        // 价格升高，token0 减少；价格降低，token1 减少。两者分别计算最小值。
        Ok((
            self.amounts(hi, l, round_up)?.0,
            self.amounts(lo, l, round_up)?.1,
        ))
    }
}

pub fn parse_sqrt(raw: &str) -> Result<U256> {
    U256::from_str_radix(raw, 10).context("invalid pool sqrtPriceX96")
}

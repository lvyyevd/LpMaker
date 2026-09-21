//! 手工退出的可保留尾差。用原始整数和池价核对，不能因 Quoter 返回 0 就忽略资产。
//! 同时限制基础币 <= 1e-8 单位，报价币现值向上取整 <= 2 个最小单位。
use super::slippage::{MAX_TICK, MIN_TICK, parse_sqrt, sqrt_at_tick};
use crate::{config::LiquidityConfig, domain::PoolSnapshot};
use alloy::primitives::{U256, U512};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

const QUOTE_ATOMS: u64 = 2;

#[derive(Clone, Debug, Serialize)]
pub struct BaseDust {
    pub raw_base: String,
    pub raw_quote_ceiling: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub sqrt_price_x96: String,
    pub base_is_token0: bool,
    pub block: u64,
    pub block_hash: String,
    pub time_ms: u64,
}

fn within_limit(raw: U256, decimals: u8, sqrt: U256, base0: bool) -> Result<Option<U256>> {
    ensure!(decimals <= 36, "unsupported dust token decimals");
    let base_limit = U256::from(10).pow(U256::from(decimals)) / U256::from(100_000_000);
    if raw.is_zero() || raw > base_limit {
        return Ok(None);
    }
    ensure!(
        sqrt >= sqrt_at_tick(MIN_TICK)? && sqrt < sqrt_at_tick(MAX_TICK)?,
        "invalid dust pool price"
    );
    let raw = U512::from(raw);
    let sqrt = U512::from(sqrt);
    let squared = sqrt.checked_mul(sqrt).context("dust price overflow")?;
    let q192 = U512::from(1) << 192;
    let (numerator, denominator) = if base0 {
        (
            raw.checked_mul(squared).context("dust amount overflow")?,
            q192,
        )
    } else {
        (
            raw.checked_mul(q192).context("dust amount overflow")?,
            squared,
        )
    };
    ensure!(!denominator.is_zero(), "invalid dust price");
    // 向上取整避免把 2.x 个原始报价单位当成可保留的 2 个。
    let ceiling =
        numerator / denominator + U512::from(u8::from(numerator % denominator != U512::ZERO));
    Ok((ceiling <= U512::from(QUOTE_ATOMS)).then(|| U256::from(ceiling.to::<u64>())))
}

impl BaseDust {
    /// 该证据只来自本次实时观察；最终核对仍重读余额和池价，不信任旧文件中的估值。
    pub fn assess(
        cfg: &LiquidityConfig,
        snapshot: &PoolSnapshot,
        balance: U256,
    ) -> Result<Option<Self>> {
        let sqrt = parse_sqrt(&snapshot.sqrt_price_x96)?;
        Ok(
            within_limit(balance, cfg.base_decimals, sqrt, snapshot.base_is_token0)?.map(
                |ceiling| Self {
                    raw_base: balance.to_string(),
                    raw_quote_ceiling: ceiling.to_string(),
                    base_decimals: cfg.base_decimals,
                    quote_decimals: cfg.quote_decimals,
                    sqrt_price_x96: snapshot.sqrt_price_x96.clone(),
                    base_is_token0: snapshot.base_is_token0,
                    block: snapshot.block,
                    block_hash: snapshot.block_hash.clone(),
                    time_ms: snapshot.time_ms,
                },
            ),
        )
    }
    pub fn validate(&self, balance: U256) -> Result<()> {
        ensure!(
            U256::from_str_radix(&self.raw_base, 10)? == balance,
            "dust proof balance changed"
        );
        let ceiling = within_limit(
            balance,
            self.base_decimals,
            parse_sqrt(&self.sqrt_price_x96)?,
            self.base_is_token0,
        )?
        .context("base remainder exceeds exit dust limit")?;
        ensure!(
            ceiling.to_string() == self.raw_quote_ceiling,
            "dust valuation proof mismatch"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_quote_value_up_and_handles_both_token_orders() {
        let q96 = U256::from(1) << 96;
        for base0 in [true, false] {
            assert_eq!(
                within_limit(U256::from(2), 18, q96, base0).unwrap(),
                Some(U256::from(2))
            );
            assert!(
                within_limit(U256::from(3), 18, q96, base0)
                    .unwrap()
                    .is_none()
            );
        }
        // 2.x 报价单位必须拒绝，不能向下取整后通过。
        assert!(
            within_limit(U256::from(2), 18, q96 + U256::from(1), true)
                .unwrap()
                .is_none()
        );
        assert!(
            within_limit(U256::from(2), 18, q96 - U256::from(1), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            within_limit(U256::from(8), 18, q96 * U256::from(2), false).unwrap(),
            Some(U256::from(2))
        );
    }

    #[test]
    fn base_cap_is_independent_of_quote_value_and_zero_is_not_dust() {
        let cheap = sqrt_at_tick(MIN_TICK).unwrap();
        assert!(
            within_limit(U256::from(10_000_000_001_u64), 18, cheap, true)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            within_limit(U256::from(10_000_000_000_u64), 18, cheap, true).unwrap(),
            Some(U256::from(1))
        );
        assert!(within_limit(U256::ZERO, 18, cheap, true).unwrap().is_none());
        assert!(
            within_limit(U256::from(1), 6, cheap, true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_pool_prices_cannot_authorize_dust() {
        for price in [
            U256::ZERO,
            U256::from(1),
            sqrt_at_tick(MAX_TICK).unwrap(),
            U256::MAX,
        ] {
            for base0 in [true, false] {
                assert!(within_limit(U256::from(1), 18, price, base0).is_err());
            }
        }
    }
}

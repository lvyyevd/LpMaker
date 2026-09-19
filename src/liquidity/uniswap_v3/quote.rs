//! 换币报价。Base 的 0.3% 池必须先扣实际池费和价格影响，再计算滑点保护。
//! Quoter 仅通过 eth_call 模拟，不会创建订单或发送链上交易。
use super::{UniswapV3, raw_units};
use crate::evm::rpc::address_word;
use alloy::primitives::{Address, U256};
use anyhow::{Context, Result, ensure};

impl UniswapV3 {
    pub fn quoter(&self) -> Result<Option<Address>> {
        crate::liquidity::chains::chain(self.cfg.chain_id)
            .and_then(|c| c.quoter_v2)
            .map(str::parse)
            .transpose()
            .context("invalid quoter address")
    }

    /// 输入为链上整数金额；返回结果已包含手续费与跨 tick 价格影响。
    pub async fn quote_exact_input(
        &self,
        token_in: Address,
        token_out: Address,
        input: U256,
        block: u64,
    ) -> Result<U256> {
        ensure!(
            input > U256::ZERO
                && ((token_in == self.base && token_out == self.quote)
                    || (token_in == self.quote && token_out == self.base)),
            "invalid pool quote request"
        );
        let quoter = self.quoter()?.context("no QuoterV2 configured for chain")?;
        let values = self
            .rpc
            .words(
                quoter,
                "quoteExactInputSingle((address,address,uint256,uint24,uint160))",
                &[
                    address_word(token_in),
                    address_word(token_out),
                    input,
                    U256::from(self.cfg.fee),
                    U256::ZERO,
                ],
                &format!("0x{block:x}"),
            )
            .await?;
        ensure!(
            values.len() == 4 && values[0] > U256::ZERO,
            "invalid QuoterV2 response"
        );
        Ok(values[0])
    }

    /// Robinhood 继续使用原来的价格/滑点公式；新 Base 路径以 Quoter 结果为基准。
    pub async fn minimum_swap_output(
        &self,
        token_in: Address,
        token_out: Address,
        input: U256,
        block: u64,
        legacy_expected: f64,
        output_decimals: u8,
    ) -> Result<U256> {
        if self.quoter()?.is_some() {
            let quoted = self
                .quote_exact_input(token_in, token_out, input, block)
                .await?;
            let minimum = quoted
                .checked_mul(U256::from(10000 - self.cfg.slippage_bps))
                .context("quoted output overflow")?
                / U256::from(10000);
            tracing::info!(chain_id=self.cfg.chain_id, pool=%self.pool, block, raw_input=%input,
                raw_quoted_output=%quoted, raw_minimum_output=%minimum, "池子实际换币报价与滑点保护已核对");
            Ok(minimum)
        } else {
            raw_units(
                legacy_expected * (1.0 - self.cfg.slippage_bps as f64 / 10000.0),
                output_decimals,
            )
        }
    }
}

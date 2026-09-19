//! Base 的总费用包含 L1 数据费；只检查 gas × maxFeePerGas 会漏算这一部分。
use crate::evm::{fees::buffered, rpc::Rpc};
use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};

pub const ORACLE: &str = "0x420000000000000000000000000000000000000F";

/// 当前执行器只生成空 access-list、零 value 的 legacy/type-2 交易。
/// calldata + 256 字节覆盖该封装的无签名长度；Oracle 自行计入签名开销。
/// 无法读取 Oracle 时停止发送，不把未知 L1 费用当零。
pub async fn extra_fee(
    rpc: &Rpc,
    calldata_len: usize,
    gas_limit: u64,
    buffer_bps: u32,
) -> Result<U256> {
    let oracle: Address = ORACLE.parse()?;
    let length = calldata_len
        .checked_add(256)
        .context("transaction length overflow")?;
    let l1 = rpc
        .words(
            oracle,
            "getL1FeeUpperBound(uint256)",
            &[U256::from(length)],
            "latest",
        )
        .await?[0];
    let operator = rpc
        .words(
            oracle,
            "getOperatorFee(uint256)",
            &[U256::from(gas_limit)],
            "latest",
        )
        .await?[0];
    let total = l1.checked_add(operator).context("Base fee overflow")?;
    let total: u128 = total.try_into().context("Base fee exceeds u128")?;
    let reserved = U256::from(buffered(total, buffer_bps)?);
    tracing::info!(l1_fee_upper_bound=%l1, operator_fee=%operator, extra_fee_reserved=%reserved,
        "Base L1 数据费和 operator 费用已加入 Gas 预算");
    Ok(reserved)
}

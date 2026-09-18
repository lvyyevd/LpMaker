//! Fee headroom changes the EIP-1559 cap, not the price paid for every gas unit.
use super::rpc::{Rpc, RpcError};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize)]
pub struct Fees {
    pub fee_buffer_bps: u32,
    pub base_fee_per_gas: Option<u128>,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: Option<u128>,
}
fn quantity(value: &Value) -> Result<u128> {
    Ok(u128::from_str_radix(
        value
            .as_str()
            .context("fee must be hex")?
            .trim_start_matches("0x"),
        16,
    )?)
}
/// Always round up so small integer estimates still receive the configured reserve.
pub fn buffered(value: u128, extra_bps: u32) -> Result<u128> {
    ensure!((1..=40_000).contains(&extra_bps), "invalid gas buffer");
    Ok(value
        .checked_mul(10_000 + u128::from(extra_bps))
        .and_then(|v| v.checked_add(9_999))
        .context("gas buffer overflow")?
        / 10_000)
}
fn bump(value: u128) -> Result<u128> {
    value
        .checked_add(value / 4)
        .and_then(|v| v.checked_add(1))
        .context("replacement fee overflow")
}
impl Fees {
    pub async fn estimate(rpc: &Rpc, fee_buffer_bps: u32) -> Result<Self> {
        ensure!(
            (1..=40_000).contains(&fee_buffer_bps),
            "invalid gas fee buffer"
        );
        let gas_price = quantity(&rpc.request("eth_gasPrice", json!([])).await?)?;
        let block = rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        ensure!(
            block.is_object(),
            "latest block unavailable for fee estimation"
        );
        match block.get("baseFeePerGas").filter(|v| !v.is_null()) {
            Some(value) => {
                let base = quantity(value)?;
                let tip = match rpc.request("eth_maxPriorityFeePerGas", json!([])).await {
                    Ok(value) => quantity(&value)?,
                    Err(e)
                        if e.downcast_ref::<RpcError>()
                            .is_some_and(|r| matches!(r.code, -32601 | -32004)) =>
                    {
                        tracing::warn!(
                            "priority-fee RPC unsupported; deriving tip from gas price and base fee"
                        );
                        gas_price.saturating_sub(base)
                    }
                    Err(e) => return Err(e),
                };
                Ok(Self {
                    fee_buffer_bps,
                    base_fee_per_gas: Some(base),
                    max_fee_per_gas: buffered(base, fee_buffer_bps)?
                        .checked_add(tip)
                        .context("fee overflow")?
                        .max(buffered(gas_price, fee_buffer_bps)?),
                    max_priority_fee_per_gas: Some(tip),
                })
            }
            None => Ok(Self {
                fee_buffer_bps,
                base_fee_per_gas: None,
                max_fee_per_gas: buffered(gas_price, fee_buffer_bps)?,
                max_priority_fee_per_gas: None,
            }),
        }
    }
    pub fn replacement(&mut self, old_cap: u128, old_tip: u128) -> Result<()> {
        self.max_fee_per_gas = self.max_fee_per_gas.max(bump(old_cap)?);
        if let Some(tip) = &mut self.max_priority_fee_per_gas {
            *tip = (*tip).max(bump(old_tip)?);
            self.max_fee_per_gas = self.max_fee_per_gas.max(
                buffered(
                    self.base_fee_per_gas.context("missing base fee")?,
                    self.fee_buffer_bps,
                )?
                .checked_add(*tip)
                .context("fee overflow")?,
            );
        }
        Ok(())
    }
}

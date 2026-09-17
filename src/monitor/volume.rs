use crate::{
    config::Config,
    domain::PoolSnapshot,
    evm::{UniswapV3, events, rpc::hex_u64},
};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub block: u64,
    pub quote: f64,
    pub base: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Volume {
    pub chain_id: u64,
    pub pool: String,
    pub cursor: Option<(u64, String, u64)>,
    pub coverage_start_ms: u64,
    #[serde(default)]
    pub window_from_block: u64,
    pub trades: BTreeMap<String, Trade>,
}
impl Volume {
    pub fn insert(&mut self, log: &Value, c: &Config, base0: bool) -> Result<()> {
        let key = format!(
            "{}:{}",
            log["blockHash"].as_str().context("log block hash")?,
            log["logIndex"].as_str().context("log index")?
        );
        if log["removed"].as_bool() == Some(true) {
            self.trades.remove(&key);
            return Ok(());
        }
        let decoded = events::decode(log)?;
        if decoded["kind"] != "Swap" {
            return Ok(());
        }
        let fields = &decoded["fields"];
        let amount = |key: &str, decimals: u8| -> Result<f64> {
            Ok(fields[key]
                .as_str()
                .context("swap amount")?
                .parse::<f64>()?
                .abs()
                / 10f64.powi(decimals as i32))
        };
        self.trades.insert(
            key,
            Trade {
                block: hex_u64(&log["blockNumber"])?,
                base: amount(
                    if base0 { "amount0" } else { "amount1" },
                    c.liquidity.base_decimals,
                )?,
                quote: amount(
                    if base0 { "amount1" } else { "amount0" },
                    c.liquidity.quote_decimals,
                )?,
            },
        );
        Ok(())
    }
    pub fn report(&self, snapshot: &PoolSnapshot, window_seconds: u64) -> Value {
        let cutoff = snapshot.time_ms.saturating_sub(window_seconds * 1000);
        let selected: Vec<_> = self
            .trades
            .values()
            .filter(|t| t.block >= self.window_from_block && t.block <= snapshot.block)
            .collect();
        json!({"window_seconds":window_seconds,"as_of_ms":snapshot.time_ms,"from_block":self.window_from_block,
            "confirmed_through_block":self.cursor.as_ref().map(|x|x.0),
            "complete":self.coverage_start_ms<=cutoff && self.cursor.as_ref().is_some_and(|x|x.0>=snapshot.block),
            "swap_count":selected.len(),"volume_usdg":selected.iter().map(|t|t.quote).sum::<f64>(),
            "volume_weth":selected.iter().map(|t|t.base).sum::<f64>(),
            "method":"absolute quote leg per confirmed Swap, counted once; window located by block timestamps"})
    }
    async fn header(venue: &UniswapV3, number: u64) -> Result<Value> {
        venue
            .archive_rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await
    }
    /// Find the first block at/after the cutoff. Some RPC logs carry timestamp=0;
    /// filtering by an exact block boundary avoids relying on that optional field.
    async fn cutoff_block(
        c: &Config,
        venue: &UniswapV3,
        snapshot: &PoolSnapshot,
        cutoff: u64,
    ) -> Result<(u64, u64)> {
        let mut low = snapshot.block.saturating_sub(c.monitoring.backfill_blocks);
        let first_time = hex_u64(&Self::header(venue, low).await?["timestamp"])? * 1000;
        if first_time > cutoff {
            return Ok((low, first_time));
        }
        let mut high = snapshot.block;
        while low < high {
            let mid = low + (high - low) / 2;
            let time = hex_u64(&Self::header(venue, mid).await?["timestamp"])? * 1000;
            if time < cutoff {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok((low, cutoff))
    }
    pub async fn refresh(
        &mut self,
        c: &Config,
        venue: &UniswapV3,
        snapshot: &PoolSnapshot,
    ) -> Result<()> {
        ensure!(
            hex_u64(&venue.archive_rpc.request("eth_chainId", json!([])).await?)?
                == c.liquidity.chain_id,
            "history RPC chain mismatch"
        );
        let anchor = Self::header(venue, snapshot.block).await?;
        ensure!(
            anchor["hash"] == snapshot.block_hash,
            "primary/history RPC canonical block mismatch"
        );
        let cutoff = snapshot
            .time_ms
            .saturating_sub(c.monitoring.volume_window_seconds * 1000);
        let wrong_pool = self.chain_id != c.liquidity.chain_id
            || !self.pool.eq_ignore_ascii_case(&c.liquidity.pool);
        let old = self
            .cursor
            .as_ref()
            .is_some_and(|x| x.2 < cutoff || x.0 > snapshot.block);
        let reorg = if let Some((n, hash, _)) = &self.cursor {
            Self::header(venue, *n).await?["hash"].as_str() != Some(hash)
        } else {
            false
        };
        if wrong_pool || old || reorg {
            tracing::info!(reorg, old, wrong_pool, "pool volume backfill restarted");
            *self = Self {
                chain_id: c.liquidity.chain_id,
                pool: c.liquidity.pool.clone(),
                ..Default::default()
            };
        }
        let (window_from, coverage_start) = Self::cutoff_block(c, venue, snapshot, cutoff).await?;
        self.window_from_block = window_from;
        self.coverage_start_ms = coverage_start;
        let mut from = self
            .cursor
            .as_ref()
            .map(|x| x.0 + 1)
            .unwrap_or(window_from)
            .max(window_from);
        // Small parallel batches avoid large eth_getLogs responses timing out on public RPCs.
        for _ in 0..4 {
            if from > snapshot.block {
                break;
            }
            let group_end = (from + 799).min(snapshot.block);
            let ranges: Vec<_> = (from..=group_end)
                .step_by(200)
                .map(|start| (start, (start + 199).min(group_end)))
                .collect();
            let mut batches: Vec<_> =
                stream::iter(ranges.into_iter().map(|(start, to)| async move {
                    Ok::<_, anyhow::Error>((start, to, venue.logs(start, to).await?))
                }))
                .buffer_unordered(4)
                .try_collect()
                .await?;
            batches.sort_by_key(|(start, _, _)| *start);
            for (start, to, raw) in batches {
                let logs = raw.as_array().context("pool logs array")?;
                let header = Self::header(venue, to).await?;
                for log in logs {
                    let n = hex_u64(&log["blockNumber"])?;
                    ensure!(
                        n >= start && n <= to,
                        "RPC returned log outside requested range"
                    );
                    ensure!(
                        log["address"]
                            .as_str()
                            .is_some_and(|a| a.eq_ignore_ascii_case(&c.liquidity.pool)),
                        "RPC returned another pool's log"
                    );
                    if n == to {
                        ensure!(log["blockHash"] == header["hash"], "reorg during backfill");
                    }
                    self.insert(log, c, snapshot.base_is_token0)?;
                }
                self.cursor = Some((
                    to,
                    header["hash"].as_str().context("block hash")?.into(),
                    hex_u64(&header["timestamp"])? * 1000,
                ));
                tracing::debug!(
                    from,
                    to,
                    events = logs.len(),
                    "confirmed pool log batch backfilled"
                );
                from = to + 1;
            }
        }
        self.trades.retain(|_, t| t.block >= window_from);
        Ok(())
    }
}

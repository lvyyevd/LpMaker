//! Rolling fee APR, never an annualization of portfolio PnL or token price appreciation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

pub const WINDOW_MS: u64 = 3_600_000;
const YEAR_MS: f64 = 365.0 * 24.0 * 3_600_000.0;
pub const FILE: &str = "lp_performance.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub identity: Value,
    pub positions: BTreeMap<String, Holding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Holding {
    pub since_ms: u64,
    pub since_source: String,
    pub samples: VecDeque<Sample>,
    pub reset_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub time_ms: u64,
    pub block: u64,
    pub block_hash: String,
    pub revision: String,
    pub fee_base: f64,
    pub fee_quote: f64,
    pub principal_quote: f64,
}

fn finite(v: &Value) -> Result<f64> {
    let n = v.as_f64().context("missing fee/principal observation")?;
    ensure!(
        n.is_finite() && n >= 0.0,
        "invalid fee/principal observation"
    );
    Ok(n)
}
fn valid(s: &Sample) -> bool {
    s.time_ms > 0
        && !s.block_hash.is_empty()
        && !s.revision.is_empty()
        && [s.fee_base, s.fee_quote, s.principal_quote]
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0)
}

impl History {
    /// A corrupted optional monitoring cache must not invent historical fees.
    pub fn validate(&self) -> Result<()> {
        for h in self.positions.values() {
            ensure!(
                h.since_ms > 0 && h.samples.len() <= 3602,
                "invalid holding history"
            );
            let mut previous = None;
            for s in &h.samples {
                ensure!(valid(s), "invalid APR sample");
                if let Some(p) = previous {
                    let p: &Sample = p;
                    ensure!(
                        s.time_ms > p.time_ms
                            && s.block > p.block
                            && s.revision == p.revision
                            && s.fee_base >= p.fee_base
                            && s.fee_quote >= p.fee_quote,
                        "non-contiguous APR samples"
                    );
                }
                previous = Some(s);
            }
        }
        Ok(())
    }

    /// Update only after all positions were read at one fresh, confirmed chain block.
    pub fn observe(
        &mut self,
        snapshot: &mut Value,
        now: u64,
        max_gap_ms: u64,
        starts: &Starts,
    ) -> Result<()> {
        if snapshot["mode"] == "paper" || snapshot["positions_observed"] != true {
            return Ok(());
        }
        let time = snapshot["pool"]["time_ms"]
            .as_u64()
            .context("pool timestamp")?;
        ensure!(
            time > 0 && time <= now + 5000 && now.saturating_sub(time) <= max_gap_ms,
            "APR snapshot stale"
        );
        let price = finite(&snapshot["pool"]["price"])?;
        ensure!(price > 0.0, "invalid APR price");
        let identity = snapshot["accounting_identity"].clone();
        ensure!(identity["owner"].is_string(), "APR owner missing");
        if self.identity != identity {
            self.positions.clear();
            self.identity = identity;
        }
        if snapshot["accounting_reorg"] == true {
            for h in self.positions.values_mut() {
                h.samples.clear();
                h.reset_reason = Some("chain_changed".into());
            }
        }
        let block = snapshot["pool"]["block"].as_u64().context("pool block")?;
        let hash = snapshot["pool"]["block_hash"]
            .as_str()
            .context("pool block hash")?
            .to_string();
        let positions = snapshot["positions"]
            .as_array_mut()
            .context("positions missing")?;
        let mut ids = BTreeSet::new();
        for p in positions.iter_mut() {
            let id = p["token_id"].as_str().context("position id")?.to_string();
            ensure!(ids.insert(id.clone()), "duplicate APR position");
            let sample = Sample {
                time_ms: time,
                block,
                block_hash: hash.clone(),
                revision: p["accounting_revision"]
                    .as_str()
                    .context("position accounting revision")?
                    .into(),
                fee_base: finite(&p["unclaimed_base"])?,
                fee_quote: finite(&p["unclaimed_quote"])?,
                principal_quote: finite(&p["principal_value_usdg"])?,
            };
            let start = starts.get(&self.identity, &id).filter(|t| *t <= time);
            let holding = self.positions.entry(id).or_insert_with(|| Holding {
                since_ms: start.unwrap_or(time),
                since_source: if start.is_some() {
                    "confirmed_mint"
                } else {
                    "first_observation"
                }
                .into(),
                samples: VecDeque::new(),
                reset_reason: None,
            });
            if let Some(start) = start.filter(|start| *start < holding.since_ms) {
                holding.since_ms = start;
                holding.since_source = "confirmed_mint".into();
            }
            if let Some(old) = holding.samples.back() {
                // A repeated confirmed block cannot add elapsed time or duplicate fee income.
                if old.block == block && old.block_hash == hash {
                    decorate(p, holding, time, price);
                    continue;
                }
                let reset = if time <= old.time_ms || block <= old.block {
                    Some("chain_changed")
                } else if time - old.time_ms > max_gap_ms {
                    Some("observation_gap")
                } else if sample.revision != old.revision {
                    Some("position_operated")
                } else if sample.fee_base < old.fee_base || sample.fee_quote < old.fee_quote {
                    Some("fee_counter_decreased")
                } else {
                    None
                };
                if let Some(reason) = reset {
                    holding.samples.clear();
                    holding.reset_reason = Some(reason.into());
                }
            }
            holding.samples.push_back(sample);
            let cutoff = time.saturating_sub(WINDOW_MS);
            // Keep one boundary sample for linear interpolation at exactly one hour ago.
            while holding.samples.len() > 2 && holding.samples[1].time_ms <= cutoff {
                holding.samples.pop_front();
            }
            decorate(p, holding, time, price);
        }
        self.positions.retain(|id, _| ids.contains(id));
        // Aggregate over a common window; never average APR percentages from unequal windows.
        let start = self
            .positions
            .values()
            .filter_map(|h| h.samples.front().map(|s| s.time_ms))
            .max()
            .unwrap_or(time)
            .max(time.saturating_sub(WINDOW_MS));
        let mut fees = 0.0;
        let mut capital_ms = 0.0;
        for h in self.positions.values() {
            let (f, c) = integrate(h, start, price);
            fees += f;
            capital_ms += c;
        }
        snapshot["fee_apr_1h"] = rate(start, time, fees, capital_ms);
        Ok(())
    }
}

fn decorate(p: &mut Value, holding: &Holding, time: u64, price: f64) {
    p["holding"] = json!({"since_ms":holding.since_ms,"source":holding.since_source,
        "seconds":time.saturating_sub(holding.since_ms)/1000});
    let start = holding
        .samples
        .front()
        .map(|s| s.time_ms)
        .unwrap_or(time)
        .max(time.saturating_sub(WINDOW_MS));
    let (fee, capital) = integrate(holding, start, price);
    let mut report = rate(start, time, fee, capital);
    report["last_reset_reason"] = json!(holding.reset_reason);
    p["fee_apr_1h"] = report;
}

fn integrate(holding: &Holding, start: u64, price: f64) -> (f64, f64) {
    let mut fees = 0.0;
    let mut capital_ms = 0.0;
    for (a, b) in holding.samples.iter().zip(holding.samples.iter().skip(1)) {
        if b.time_ms <= start {
            continue;
        }
        let left = a.time_ms.max(start);
        let dt = (b.time_ms - left) as f64;
        let fraction = dt / (b.time_ms - a.time_ms) as f64;
        // Difference token quantities FIRST. Revaluing old WETH fees is not new fee income.
        fees += ((b.fee_base - a.fee_base) * price + b.fee_quote - a.fee_quote) * fraction;
        let left_capital = b.principal_quote + (a.principal_quote - b.principal_quote) * fraction;
        capital_ms += (left_capital + b.principal_quote) * 0.5 * dt;
    }
    (fees, capital_ms)
}
fn rate(start: u64, end: u64, fees: f64, capital_ms: f64) -> Value {
    let elapsed = end.saturating_sub(start);
    let valid = elapsed >= 60_000
        && capital_ms > 0.0
        && capital_ms.is_finite()
        && fees.is_finite()
        && fees >= 0.0;
    let apr = (fees / capital_ms * YEAR_MS * 100.0)
        .is_finite()
        .then_some(fees / capital_ms * YEAR_MS * 100.0)
        .filter(|_| valid);
    json!({"window_seconds":3600,"observed_seconds":elapsed/1000,"complete":elapsed>=WINDOW_MS,
        "from_ms":start,"as_of_ms":end,"fees_usdg":if valid {Some(fees)} else {None},
        "average_principal_usdg":if valid {Some(capital_ms/elapsed as f64)} else {None},"apr_pct":apr,
        "status":if apr.is_none() {"collecting"} else if elapsed<WINDOW_MS {"partial"} else {"ready"},
        "basis":"fee token deltas valued at latest pool price; time-weighted LP principal; interpolated boundary; simple 365-day APR; excludes IL, gas, swaps, hedge fees and funding"})
}

/// Bounded migration of local, confirmed mint receipts. Missing history remains unknown.
#[derive(Clone, Debug, Default)]
pub struct Starts(BTreeMap<String, u64>);
impl Starts {
    fn key(manager: &str, owner: &str, id: &str) -> String {
        format!(
            "{}:{}:{id}",
            manager.to_ascii_lowercase(),
            owner.to_ascii_lowercase()
        )
    }
    fn get(&self, identity: &Value, id: &str) -> Option<u64> {
        self.0
            .get(&Self::key(
                identity["manager"].as_str()?,
                identity["owner"].as_str()?,
                id,
            ))
            .copied()
    }
    pub fn insert_receipt(&mut self, record: &Value) {
        use alloy::primitives::{U256, keccak256};
        if record["kind"] != "operation_result" || record["data"]["status"] != "0x1" {
            return;
        }
        let Some(time) = record["time_ms"].as_u64().filter(|t| *t > 0) else {
            return;
        };
        let Some(logs) = record["data"]["logs"].as_array() else {
            return;
        };
        let transfer = format!("{:#x}", keccak256("Transfer(address,address,uint256)"));
        for log in logs {
            if log["topics"][0] != transfer
                || log["topics"][1] != format!("0x{}", "0".repeat(64))
                || log["removed"] == true
            {
                continue;
            }
            let (Some(manager), Some(to), Some(token)) = (
                log["address"].as_str(),
                log["topics"][2].as_str(),
                log["topics"][3].as_str(),
            ) else {
                continue;
            };
            if to.len() != 66 || !to.is_ascii() || !to.starts_with("0x") {
                continue;
            }
            let Ok(id) = U256::from_str_radix(token.trim_start_matches("0x"), 16) else {
                continue;
            };
            let owner = format!("0x{}", &to[26..]);
            self.0
                .entry(Self::key(manager, &owner, &id.to_string()))
                .and_modify(|old| *old = (*old).min(time))
                .or_insert(time);
        }
    }
    pub fn load(root: &Path) -> Result<Self> {
        let mut paths = vec![root.join("events.jsonl")];
        let archive = root.join("event_archive");
        if archive.exists() {
            let mut archived = std::fs::read_dir(archive)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with("events-") && s.ends_with(".jsonl"))
                })
                .collect::<Vec<_>>();
            archived.sort();
            archived.reverse();
            paths.extend(archived);
        }
        let mut out = Self::default();
        let mut budget = 64 * 1024 * 1024_u64;
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let file = File::open(path)?;
            let size = file.metadata()?.len();
            if size > budget {
                continue;
            }
            budget -= size;
            for line in BufReader::new(file).lines() {
                let line = line?;
                // Receipt records contain no signing secrets; ignore all other journal content.
                if !line.contains("operation_result") {
                    continue;
                }
                if let Ok(record) = serde_json::from_str(&line) {
                    out.insert_receipt(&record);
                }
            }
        }
        Ok(out)
    }
}

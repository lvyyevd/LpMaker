//! Restart checkpoints and auditable recovery progress. Remote balances remain authoritative.
use crate::{
    config::{Config, Mode},
    engine::Paper,
    store::Store,
    strategy::{Phase, Strategy},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema: u32,
    pub saved_ms: u64,
    pub mode: Mode,
    pub strategy: Strategy,
    pub paper: Option<Paper>,
}
pub fn save(store: &Store, c: &Config, strategy: &Strategy, paper: &Paper) -> Result<()> {
    let checkpoint = Checkpoint {
        schema: 1,
        saved_ms: crate::now_ms(),
        mode: c.mode.clone(),
        strategy: strategy.clone(),
        paper: if c.mode == Mode::Paper {
            Some(paper.clone())
        } else {
            None
        },
    };
    // One authoritative atomic commit for phase + simulated balances + simulated pending order.
    store.write("checkpoint.json", &checkpoint)?;
    export(store, &checkpoint)
}
fn export(store: &Store, checkpoint: &Checkpoint) -> Result<()> {
    store.write("strategy.json", &checkpoint.strategy)?;
    if let Some(paper) = &checkpoint.paper {
        store.write("paper.json", paper)?;
    }
    Ok(())
}
pub fn load(store: &Store, c: &Config) -> Result<(Strategy, Paper)> {
    if let Some(cp) = store.read::<Checkpoint>("checkpoint.json")? {
        ensure!(
            cp.schema == 1 && cp.mode == c.mode,
            "checkpoint schema/mode mismatch; refusing to reset state"
        );
        ensure!(
            c.mode != Mode::Paper || cp.paper.is_some(),
            "paper checkpoint has no ledger"
        );
        export(store, &cp)?; // Repair a crash between the commit and compatibility-file writes.
        return Ok((cp.strategy, cp.paper.unwrap_or_else(|| Paper::new(c))));
    }
    let strategy = store.read::<Strategy>("strategy.json")?;
    let paper = store.read::<Paper>("paper.json")?;
    ensure!(
        strategy.is_some() || store.read::<Value>("equity_baseline.json")?.is_none(),
        "risk state/checkpoint missing from an existing account; refusing to reset drawdown history"
    );
    ensure!(
        c.mode != Mode::Paper || strategy.is_some() == paper.is_some(),
        "incomplete legacy paper files; refusing to reset balances or risk phase"
    );
    let strategy = strategy.unwrap_or_default();
    let paper = paper.unwrap_or_else(|| Paper::new(c));
    save(store, c, &strategy, &paper)?;
    stage(store, "checkpoint_migrated", json!({"schema":1}))?;
    Ok((strategy, paper))
}
pub fn stage(store: &Store, name: &str, details: Value) -> Result<()> {
    store.update::<Value>("startup_reconciliation.json", |report| {
        if !report.is_object() {
            *report = json!({"started_ms":crate::now_ms(),"status":"checking","stages":[]});
        }
        report["updated_ms"] = json!(crate::now_ms());
        report["stages"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid startup report"))?
            .push(json!({"time_ms":crate::now_ms(),"stage":name,"details":details}));
        if matches!(name, "ready" | "blocked") {
            report["status"] = json!(name);
        }
        Ok(())
    })?;
    tracing::info!(stage=name, details=%details, "startup reconciliation");
    store.event(
        "startup_reconciliation",
        json!({"stage":name,"details":details}),
    )
}
pub fn start(store: &Store) -> Result<()> {
    // Older attempts remain in the append-only event journal.
    store.write(
        "startup_reconciliation.json",
        &json!({"started_ms":crate::now_ms(),"status":"checking","stages":[]}),
    )
}
pub fn invalidate_observation_streaks(strategy: &mut Strategy) {
    strategy.healthy_hours = 0;
    for layer in strategy.layers.values_mut() {
        layer.outside_count = 0;
        layer.outside_side = 0;
    }
}
pub fn finish_workflow_recovery(strategy: &mut Strategy) {
    if strategy.phase != Phase::Halted {
        strategy.phase = Phase::Paused;
        strategy.pause_since = crate::now_ms();
    }
    strategy.fraction = 0.0;
    invalidate_observation_streaks(strategy);
}
pub fn pause_incomplete_inventory(
    strategy: &mut Strategy,
    actual_layers: &[String],
    c: &Config,
) -> bool {
    if matches!(strategy.phase, Phase::Active | Phase::Recovering)
        && c.strategy
            .layers
            .iter()
            .any(|l| !actual_layers.contains(&l.name))
    {
        strategy.phase = Phase::Paused;
        strategy.pause_since = crate::now_ms();
        strategy.fraction = 0.0;
        invalidate_observation_streaks(strategy);
        return true;
    }
    false
}

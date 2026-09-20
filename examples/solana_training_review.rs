//! 离线复核已完成的 v1 训练批次；不会启动策略或使用账户密钥。
//! 先核对冻结的源码/数据绑定，再按开发期评分挑选真正做过 LP 的候选。
use anyhow::{Context, Result, ensure};
use clap::Parser;
use lp_maker::{
    config::Mode,
    solana::{
        config::Config,
        research::{
            self, AccountModel, Outcome,
            training::{Record, trial_config},
        },
    },
};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    training: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 250919)]
    seed: u64,
}
/// 与已冻结的 v1 格式核对，不修改训练器或它的日志。
fn check_binding(c: &Config, a: &Args, p: &Value) -> Result<()> {
    ensure!(p["schema"] == 1, "review supports training schema v1");
    let target = p["target_return"].as_f64().context("target")?;
    let mut bytes = serde_json::to_vec(&(1_u32, c, a.seed, target))?;
    for source in [
        include_bytes!("../src/solana/research/training.rs").as_slice(),
        include_bytes!("../src/solana/research/simulation.rs"),
        include_bytes!("../src/solana/research/verification.rs"),
        include_bytes!("../src/solana/research/data.rs"),
        include_bytes!("../src/solana/regime.rs"),
        include_bytes!("../src/solana/dlmm.rs"),
    ] {
        bytes.extend_from_slice(alloy::primitives::keccak256(source).as_slice());
    }
    for name in [
        "pool_1h.json",
        "hyperliquid_1h.json",
        "funding.json",
        "manifest.json",
    ] {
        bytes.extend_from_slice(
            alloy::primitives::keccak256(std::fs::read(a.data.join(name))?).as_slice(),
        );
    }
    ensure!(
        p["binding"] == alloy::primitives::keccak256(bytes).to_string(),
        "frozen code/data/config binding mismatch; do not mix models"
    );
    Ok(())
}
fn write(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn matches_saved(a: &Outcome, b: &Outcome) -> Result<()> {
    for (x, y) in [
        (a.pnl, b.pnl),
        (a.lp_fees, b.lp_fees),
        (a.native_reserve_pnl, b.native_reserve_pnl),
        (a.native_reserve_cost, b.native_reserve_cost),
        (a.hedge_cost, b.hedge_cost),
        (a.swap_cost, b.swap_cost),
        (a.operation_cost, b.operation_cost),
        (a.funding, b.funding),
        (a.max_drawdown_pct, b.max_drawdown_pct),
    ] {
        ensure!((x - y).abs() < 1e-8, "saved outcome does not reproduce");
    }
    ensure!(
        a.model_rejections == b.model_rejections && a.operations == b.operations,
        "saved model state does not reproduce"
    );
    Ok(())
}
fn main() -> Result<()> {
    let a = Args::parse();
    let base = Config::load(a.training.join("research-config.toml"))?;
    ensure!(
        base.mode == Mode::Paper,
        "offline review requires paper config"
    );
    let p: Value = serde_json::from_slice(&std::fs::read(a.training.join("progress.json"))?)?;
    ensure!(p["status"] != "running", "training is still running");
    check_binding(&base, &a, &p)?;
    let mut count = 0_u64;
    let mut active = 0_u64;
    let mut positive = 0_u64;
    let mut passed = 0_u64;
    let mut best: Option<Record> = None;
    let mut last = None;
    for line in BufReader::new(File::open(a.training.join("trials.jsonl"))?).lines() {
        let r: Record = serde_json::from_str(&line?)?;
        ensure!(
            last.is_none_or(|index| r.index > index),
            "duplicate or out-of-order trial index"
        );
        last = Some(r.index);
        count += 1;
        passed += u64::from(r.development_passed);
        if r.train.active_hours < 100. || r.validation.active_hours < 24. {
            continue;
        }
        active += 1;
        positive += u64::from(r.train.pnl > 0. && r.validation.pnl > 0.);
        if best.as_ref().is_none_or(|b| r.score > b.score) {
            best = Some(r);
        }
    }
    ensure!(
        p["completed"].as_u64() == Some(count),
        "progress/log completed-count mismatch"
    );
    std::fs::create_dir_all(&a.output)?;
    let best = best.context("no trial met minimum active-time requirements")?;
    let c = trial_config(&base, &best.trial);
    c.validate()?;
    std::fs::write(
        a.output.join("active-candidate-paper.toml"),
        toml::to_string_pretty(&c)?,
    )?;
    let data = research::load(&a.data)?;
    let signals = research::cached_signals(&c, &data);
    let t1 = data.start + 92 * 24 * 3_600_000;
    let t2 = data.start + 122 * 24 * 3_600_000;
    let replay = |lo, hi, apr, costs, curve| {
        research::simulate_account(
            &c,
            &data,
            &signals,
            lo,
            hi,
            apr,
            costs,
            curve,
            AccountModel {
                native_sol: best.native_sol,
            },
        )
    };
    matches_saved(&best.train, &replay(data.start, t1, 0.4, 1., false)?)?;
    matches_saved(&best.validation, &replay(t1, t2, 0.4, 1., false)?)?;
    matches_saved(
        &best.train_double_cost,
        &replay(data.start, t1, 0.4, 2., false)?,
    )?;
    matches_saved(
        &best.validation_double_cost,
        &replay(t1, t2, 0.4, 2., false)?,
    )?;
    let full = replay(data.start, data.end, 0.4, 1., true)?;
    let later = replay(t2, data.end, 0.4, 1., true)?;
    let double = replay(data.start, data.end, 0.4, 2., false)?;
    let low_apr = replay(data.start, data.end, 0.3, 1., false)?;
    let zero_apr = replay(data.start, data.end, 0., 1., false)?;
    let result = json!({"review":"development-score-best candidate among train>=100h and validation>=24h; not ranked by six-month return", "frozen_binding_matched":true,"four_saved_outcomes_reproduced":true,
        "completed":count,"active_time_eligible":active,"positive_train_and_validation":positive,"development_passed":passed,
        "capital":c.strategy.total_capital,"compute_seconds":p["compute_seconds"],"original_selection":p["best"],"active_candidate":best,
        "hourly_full":full,"hourly_later_62_days":later,"hourly_double_cost":double,"hourly_apr_30":low_apr,"hourly_zero_apr":zero_apr});
    write(&a.output.join("review.json"), &result)?;
    let fine = research::verify_account(
        &c,
        &a.data,
        &a.output.join("active-fine.json"),
        0.4,
        best.native_sol,
    )?;
    println!(
        "reviewed {count} trials; active={active}; positive train/validation={positive}; passed={passed}; selected={}",
        best.index
    );
    println!(
        "hourly full PnL={:.6}, fees={:.6}, later62={:.6}, doubled={:.6}; fine full={}",
        full.pnl, full.lp_fees, later.pnl, double.pnl, fine["full"]["pnl"]
    );
    Ok(())
}

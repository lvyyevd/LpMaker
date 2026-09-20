//! 可恢复的 SOL 离线搜索。目标是筛选标准，不是可承诺的未来收益。
//! 此模块不调用交易接口，不覆盖线上配置；每一个完成的候选都先落盘。
use super::{
    AccountModel, HOUR, Outcome, Parameters, cached_signals, configure, load, simulate_account,
};
use crate::{
    config::Mode,
    solana::{config::Config, regime},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    time::Instant,
};

const VERSION: u32 = 1;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trial {
    pub parameters: Parameters,
    pub lp_budget: f64,
    pub hedge_deadband_usd: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub index: u64,
    pub trial: Trial,
    pub native_sol: f64,
    pub train: Outcome,
    pub validation: Outcome,
    pub train_double_cost: Outcome,
    pub validation_double_cost: Outcome,
    pub score: f64,
    pub development_passed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Best {
    record: Record,
    full: Outcome,
    later_62_days: Outcome,
    double_cost: Outcome,
    apr_30_percent: Outcome,
    target_reached_in_hourly_model: bool,
    target_passes_fine_model: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Progress {
    schema: u32,
    binding: String,
    next_index: u64,
    completed: u64,
    rejected_budget: u64,
    compute_seconds: f64,
    status: String,
    target_return: f64,
    best: Option<Best>,
}
/// SDK 的账户长度公式 + 当前 Solana 常见免租金公式；不是历史 RPC 报价。
/// 额外 0.05 SOL 预留给 ATA/bin-array/原生 gas；真实执行仍由 RPC 模拟校验。
pub fn native_reserve(width: f64) -> f64 {
    let bins = 2. * (width.ln_1p() / 1.0004_f64.ln()).ceil() + 1.;
    let bytes = 8112. + (bins - 70.).max(0.) * 112.;
    (bytes + 128.) * 6960. / 1e9 + 0.05
}
pub fn trial_config(base: &Config, p: &Trial) -> Config {
    let mut c = configure(base, &p.parameters);
    c.strategy.lp_budget = p.lp_budget;
    c.strategy.hedge_collateral = (p.lp_budget / 2.5).ceil();
    c.strategy.reserve =
        c.strategy.total_capital - c.strategy.lp_budget - c.strategy.hedge_collateral;
    c.strategy.hedge_deadband_usd = p.hedge_deadband_usd;
    c.solana.max_rent_sol = native_reserve(p.parameters.width);
    c.mode = Mode::Paper;
    c.state_dir = "data/solana-trained-paper".into();
    c.logging.directory = "data/logs/solana-trained-paper".into();
    c
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn pick<T: Copy>(&mut self, values: &[T]) -> T {
        values[(self.next() % values.len() as u64) as usize]
    }
}
/// 无日期特例，无未来行情输入；同一 seed/index 永远生成同一组参数。
pub fn candidate(seed: u64, index: u64, capital: f64) -> Trial {
    let mut r = Rng(seed.wrapping_add(index.wrapping_mul(0xd1342543de82ef95)));
    Trial {
        parameters: Parameters {
            width: r.pick(&[
                0.008, 0.012, 0.02, 0.03, 0.04, 0.06, 0.08, 0.10, 0.125, 0.15, 0.18,
            ]),
            hedge_ratio: r.pick(&[0.10, 0.25, 0.40, 0.60, 0.80, 1.]),
            cooldown_hours: r.pick(&[6, 12, 24, 48, 72, 120, 168]),
            regime: regime::Config {
                ema_entry_hours: r.pick(&[6, 12, 24, 48, 72, 96, 144]),
                ema_exit_hours: r.pick(&[12, 24, 48, 72, 96, 144]),
                momentum_hours: r.pick(&[6, 12, 24, 48, 72, 120, 168]),
                min_momentum: r.pick(&[0., 0.005, 0.01, 0.02, 0.04, 0.06, 0.08]),
                max_momentum: r.pick(&[0.10, 0.15, 0.25, 1.]),
                max_hourly_vol: r.pick(&[0.004, 0.006, 0.008, 0.010, 0.012, 0.016]),
                entry_vol_fraction: r.pick(&[0.5, 0.75, 1.]),
                max_bar_range: r.pick(&[0.02, 0.03, 0.04, 0.05, 0.06]),
                exit_buffer: r.pick(&[0., 0.003, 0.005, 0.01, 0.015]),
                entry_confirm_hours: r.pick(&[1, 2, 3, 6, 12]),
            },
        },
        lp_budget: capital * r.pick(&[0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.65]),
        hedge_deadband_usd: r.pick(&[10., 12., 15., 20.]),
    }
}
fn acceptable(o: &Outcome, max_dd: f64) -> bool {
    o.pnl > 0. && o.max_drawdown_pct <= max_dd * 100. && o.model_rejections.is_empty()
}
fn persist(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    File::open(path.parent().context("progress parent")?)?.sync_all()?;
    Ok(())
}
fn recover_log(path: &Path, next_index: u64, completed: u64) -> Result<()> {
    if !path.exists() {
        ensure!(completed == 0, "completed training log is missing");
        return Ok(());
    }
    let mut reader = BufReader::new(File::open(path)?);
    let mut valid_bytes = 0;
    let mut committed = 0;
    let mut previous = None;
    loop {
        let mut line = vec![];
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let record: Record = serde_json::from_slice(&line)
            .context("corrupt completed trial record; preserved for inspection")?;
        if record.index >= next_index {
            break;
        }
        ensure!(
            previous.is_none_or(|p| record.index > p),
            "duplicate or unordered committed trial"
        );
        previous = Some(record.index);
        valid_bytes += line.len() as u64;
        committed += 1;
    }
    ensure!(
        committed == completed,
        "training progress/log mismatch; preserve both files"
    );
    let file = OpenOptions::new().write(true).open(path)?;
    if file.metadata()?.len() != valid_bytes {
        file.set_len(valid_bytes)?;
        file.sync_all()?;
    }
    Ok(())
}
fn binding(base: &Config, data: &Path, seed: u64, target: f64) -> Result<String> {
    let mut bytes = serde_json::to_vec(&(VERSION, base, seed, target))?;
    // 源码变化也必须另开一轮，防止使用新账本口径续写旧收益记录。
    for source in [
        include_bytes!("training.rs").as_slice(),
        include_bytes!("simulation.rs"),
        include_bytes!("verification.rs"),
        include_bytes!("data.rs"),
        include_bytes!("../regime.rs"),
        include_bytes!("../dlmm.rs"),
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
            alloy::primitives::keccak256(std::fs::read(data.join(name))?).as_slice(),
        );
    }
    Ok(alloy::primitives::keccak256(bytes).to_string())
}
/// A 10h budget means measured computation time, excluding time the process was stopped.
#[allow(clippy::too_many_arguments)]
pub fn train(
    base: &Config,
    path: &Path,
    output: &Path,
    hours: f64,
    target: f64,
    seed: u64,
    max_candidates: Option<u64>,
) -> Result<()> {
    ensure!(base.mode == Mode::Paper, "training requires a paper config");
    ensure!(
        base.hyperliquid.leverage == 3,
        "training keeps the agreed 3x hedge leverage"
    );
    ensure!(
        hours.is_finite() && hours > 0. && target.is_finite() && target > 0.,
        "invalid training budget/target"
    );
    let data = load(path)?;
    ensure!(
        data.end - data.start == 184 * 24 * HOUR,
        "target training requires the frozen 184-day window"
    );
    std::fs::create_dir_all(output)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(output.join("training.lock"))?;
    lock.try_lock_exclusive()
        .context("another training process owns this directory")?;
    let id = binding(base, path, seed, target)?;
    let progress_path = output.join("progress.json");
    let mut progress = if progress_path.exists() {
        let p: Progress = serde_json::from_slice(&std::fs::read(&progress_path)?)?;
        ensure!(
            p.binding == id,
            "training data/config/seed/target changed; use a new output directory"
        );
        p
    } else {
        Progress {
            schema: VERSION,
            binding: id,
            next_index: 0,
            completed: 0,
            rejected_budget: 0,
            compute_seconds: 0.,
            status: "running".into(),
            target_return: target,
            best: None,
        }
    };
    let records_path = output.join("trials.jsonl");
    // 在候选落盘、进度更新之间崩溃时，截去尚未提交到进度文件的尾部并重算。
    // 不能仅跳过它，否则可能漏掉当时尚未保存的最优候选。
    recover_log(&records_path, progress.next_index, progress.completed)?;
    let mut records = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&records_path)?;
    let start = Instant::now();
    let previous_seconds = progress.compute_seconds;
    let initial_count = progress.completed;
    let t1 = data.start + 92 * 24 * HOUR;
    let t2 = data.start + 122 * 24 * HOUR;
    let apr = 0.4; // 用户要求固定 40%，不能让搜索调高利率来达到目标。
    progress.status = "running".into();
    persist(&progress_path, &progress)?;
    while previous_seconds + start.elapsed().as_secs_f64() < hours * 3600.
        && max_candidates.is_none_or(|n| progress.completed - initial_count < n)
    {
        let index = progress.next_index;
        let trial = candidate(seed, index, base.strategy.total_capital);
        progress.next_index += 1;
        let c = trial_config(base, &trial);
        c.validate()?;
        let native_sol = native_reserve(trial.parameters.width);
        // Every split starts independently funded; no free rent capital at its start price.
        let affordable = [data.start, t1].iter().all(|t| {
            let b = &data.pool[((t - data.start) / HOUR) as usize];
            native_sol * b.open <= c.strategy.reserve
        });
        if !affordable {
            progress.rejected_budget += 1;
            continue;
        }
        let signals = cached_signals(&c, &data);
        let replay = |lo, hi, costs, rate, curve| {
            simulate_account(
                &c,
                &data,
                &signals,
                lo,
                hi,
                rate,
                costs,
                curve,
                AccountModel { native_sol },
            )
        };
        let train = replay(data.start, t1, 1., apr, false)?;
        let validation = replay(t1, t2, 1., apr, false)?;
        let train_double_cost = replay(data.start, t1, 2., apr, false)?;
        let validation_double_cost = replay(t1, t2, 2., apr, false)?;
        let development_passed = [
            &train,
            &validation,
            &train_double_cost,
            &validation_double_cost,
        ]
        .iter()
        .all(|o| acceptable(o, c.strategy.max_drawdown))
            && train.active_hours >= 100.
            && validation.active_hours >= 24.;
        let score = (train_double_cost.pnl / 92.).min(validation_double_cost.pnl / 30.) * 122.;
        let record = Record {
            index,
            trial,
            native_sol,
            train,
            validation,
            train_double_cost,
            validation_double_cost,
            score,
            development_passed,
        };
        serde_json::to_writer(&mut records, &record)?;
        records.write_all(b"\n")?;
        records.sync_data()?;
        progress.completed += 1;
        let improves = progress.best.as_ref().is_none_or(|b| {
            (record.development_passed && !b.record.development_passed)
                || (record.development_passed == b.record.development_passed
                    && record.score > b.record.score)
        });
        if improves {
            let mut full = replay(data.start, data.end, 1., apr, true)?;
            let mut later_62_days = replay(t2, data.end, 1., apr, true)?;
            let double_cost = replay(data.start, data.end, 2., apr, false)?;
            let apr_30_percent = replay(data.start, data.end, 1., 0.3, false)?;
            let reached = record.development_passed
                && full.pnl / c.strategy.total_capital >= target
                && [&full, &later_62_days, &double_cost, &apr_30_percent]
                    .iter()
                    .all(|o| acceptable(o, c.strategy.max_drawdown));
            println!(
                "SOL 候选 {index}：半年净收益 {:.3}% / 目标 {:.1}%；后62天 {:.3} USD；回撤 {:.2}%；开发验证 {}",
                100. * full.pnl / c.strategy.total_capital,
                100. * target,
                later_62_days.pnl,
                full.max_drawdown_pct,
                record.development_passed
            );
            let fine_passed = if reached {
                let v = super::verify_account(
                    &c,
                    path,
                    &output.join(format!("fine-{index}.json")),
                    apr,
                    native_sol,
                )?;
                [
                    "full",
                    "holdout",
                    "low_first_stress",
                    "double_cost_low_first",
                    "apr_30_low_first",
                ]
                .iter()
                .all(|k| {
                    let o: Outcome =
                        serde_json::from_value(v[*k].clone()).expect("typed verification outcome");
                    acceptable(&o, c.strategy.max_drawdown)
                        && (*k != "full" || o.pnl / c.strategy.total_capital >= target)
                })
            } else {
                false
            };
            // 大曲线只在最优候选变化时写一次，进度文件保持小体积。
            persist(
                &output.join(format!("curves-{index}.json")),
                &json!({"full":full.equity_curve,"later_62_days":later_62_days.equity_curve}),
            )?;
            full.equity_curve.clear();
            later_62_days.equity_curve.clear();
            progress.best = Some(Best {
                record,
                full,
                later_62_days,
                double_cost,
                apr_30_percent,
                target_reached_in_hourly_model: reached,
                target_passes_fine_model: fine_passed,
            });
            std::fs::write(output.join("best-paper.toml"), toml::to_string_pretty(&c)?)?;
        }
        progress.compute_seconds = previous_seconds + start.elapsed().as_secs_f64();
        persist(&progress_path, &progress)?;
        if progress.completed.is_multiple_of(25) {
            println!(
                "已计算 {} 组，累计 {:.1} 分钟；资金不足剔除 {} 组",
                progress.completed,
                progress.compute_seconds / 60.,
                progress.rejected_budget
            );
        }
        // 达到小时模型目标也必须继续做细粒度验证；本程序不自动启用实盘。
        if progress
            .best
            .as_ref()
            .is_some_and(|b| b.target_passes_fine_model)
        {
            progress.status = "model_target_candidate_requires_forward_validation".into();
            break;
        }
    }
    progress.compute_seconds = previous_seconds + start.elapsed().as_secs_f64();
    if progress.status == "running" {
        progress.status = if progress.compute_seconds >= hours * 3600. {
            "budget_completed_target_not_met"
        } else {
            "candidate_limit_reached"
        }
        .into();
    }
    persist(&progress_path, &progress)?;
    persist(
        &output.join("assumptions.json"),
        &json!({
            "apr":apr,"target_account_return":target,"capital":base.strategy.total_capital,
            "max_drawdown":base.strategy.max_drawdown,"leverage":3,
            "selection":"first92d train + next30d validation, worst daily PnL under doubled costs; last62d excluded from ranking, but previously inspected and not blind",
            "native_reserve":"SDK size formula, assumed 6960 lamports per byte incl128 byte overhead plus0.05 SOL buffer; purchased at each split start, valued every bar, roundtrip swap costs charged",
            "limits":["LP APR is assumed, not observed per-bin fees", "hourly OHLC cannot prove maker fills or true intrabar liquidation", "10% isolated-equity guard is a conservative screening assumption, not historical exchange maintenance margin", "no automatic borrowing or cross-chain transfer", "5m proxy replay and fresh forward paper validation required before adoption"]
        }),
    )?;
    println!(
        "训练阶段结束：{}，累计 {:.1} 分钟，{} 组。",
        progress.status,
        progress.compute_seconds / 60.,
        progress.completed
    );
    Ok(())
}

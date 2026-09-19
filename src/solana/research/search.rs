//! Parameter selection uses development data only; every search result is retained.
use super::{HOUR, Outcome, cached_signals, load, simulate};
use crate::{
    config::LayerConfig,
    solana::{config::Config, regime},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parameters {
    pub width: f64,
    pub hedge_ratio: f64,
    pub cooldown_hours: u64,
    pub regime: regime::Config,
}
pub fn configure(base: &Config, p: &Parameters) -> Config {
    let mut c = base.clone();
    // Broad DLMM ranges need refundable account rent. Reserve it within $200,
    // rather than silently using the hedge collateral or pretending rent is free.
    c.strategy.lp_budget = 100.;
    c.strategy.hedge_collateral = 40.;
    c.strategy.reserve = 60.;
    c.solana.max_rent_sol = 0.5;
    c.strategy.ema_fast_hours = 48;
    c.strategy.ema_slow_hours = 144;
    c.strategy.vol_pause_ratio = 2.5;
    c.strategy.fast_drop_1h = 0.03;
    c.strategy.layers = vec![LayerConfig {
        name: "core".into(),
        weight: 1.,
        half_width: p.width,
    }];
    c.strategy.inside_hedge_ratio = p.hedge_ratio;
    c.strategy.cooldown_hours = p.cooldown_hours;
    c.regime = Some(p.regime.clone());
    c
}
#[derive(Clone, Serialize)]
struct Candidate {
    parameters: Parameters,
    train: Outcome,
    validation: Outcome,
    train_double_cost: Outcome,
    validation_double_cost: Outcome,
    score: f64,
}
pub fn run(base: &Config, path: &Path, output: &Path, apr: f64, grid: Option<&Path>) -> Result<()> {
    ensure!(
        apr.is_finite() && (0.0..=1.).contains(&apr),
        "APR must be a fraction, e.g. 0.4"
    );
    let started = std::time::Instant::now();
    let data = load(path)?;
    let train_end = data.start + 92 * 24 * HOUR; // March18 -> June18, fixed before searching.
    let validation_end = data.start + 122 * 24 * HOUR; // June18 -> July18; later prices never enter parameter ranking.
    let mut candidates = vec![];
    let mut count = 0;
    let mut parameters = vec![];
    for entry in [48, 96, 144] {
        for exit in [48, 96] {
            for momentum in [0.0, 0.01] {
                let regime = regime::Config {
                    ema_entry_hours: entry,
                    ema_exit_hours: exit,
                    momentum_hours: 24,
                    min_momentum: momentum,
                    max_hourly_vol: 0.012,
                    entry_vol_fraction: 1.,
                    max_momentum: 1.,
                    max_bar_range: 0.05,
                    exit_buffer: 0.005,
                    entry_confirm_hours: 3,
                };
                let initial = Parameters {
                    width: 0.025,
                    hedge_ratio: 0.5,
                    cooldown_hours: 24,
                    regime,
                };
                for width in [0.04, 0.07, 0.10, 0.15] {
                    for hedge_ratio in [0.25, 0.5, 1.] {
                        for cooldown_hours in [6, 24, 72] {
                            for confirm in [1, 3, 6] {
                                let mut p = initial.clone();
                                p.width = width;
                                p.hedge_ratio = hedge_ratio;
                                p.cooldown_hours = cooldown_hours;
                                p.regime.entry_confirm_hours = confirm;
                                parameters.push(p);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(grid) = grid {
        parameters = serde_json::from_slice(&std::fs::read(grid)?)?;
    }
    ensure!(!parameters.is_empty(), "empty parameter grid");
    let mut cache = std::collections::HashMap::new();
    for p in parameters {
        let c = configure(base, &p);
        c.validate()?;
        let mut key = p.regime.clone();
        key.entry_confirm_hours = 1;
        let key = serde_json::to_string(&key)?;
        let signals = cache
            .entry(key)
            .or_insert_with(|| cached_signals(&c, &data));
        let train = simulate(&c, &data, signals, data.start, train_end, apr, 1., false)?;
        let validation = simulate(
            &c,
            &data,
            signals,
            train_end,
            validation_end,
            apr,
            1.,
            false,
        )?;
        let train_double_cost =
            simulate(&c, &data, signals, data.start, train_end, apr, 2., false)?;
        let validation_double_cost = simulate(
            &c,
            &data,
            signals,
            train_end,
            validation_end,
            apr,
            2.,
            false,
        )?;
        // Reward the weaker development segment after doubled costs. This avoids
        // selecting a tiny positive result whose entire margin disappears in execution.
        let score = (train_double_cost.pnl / 92.).min(validation_double_cost.pnl / 30.) * 122.;
        candidates.push(Candidate {
            parameters: p,
            train,
            validation,
            train_double_cost,
            validation_double_cost,
            score,
        });
        count += 1;
        if count % 250 == 0 {
            println!(
                "SOL research: {count} parameter sets; holdout not accessed in selection; elapsed {:.1}s",
                started.elapsed().as_secs_f64()
            );
        }
    }
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let feasible = |x: &&Candidate| {
        x.train.pnl > 0.
            && x.validation.pnl > 0.
            && x.train_double_cost.pnl > 0.
            && x.validation_double_cost.pnl > 0.
            && x.train.active_hours >= 100.
            && x.train.max_drawdown_pct < 5.
            && x.validation.max_drawdown_pct < 5.
    };
    let best = candidates.iter().find(feasible).unwrap_or(&candidates[0]);
    let c = configure(base, &best.parameters);
    let sig = cached_signals(&c, &data);
    let heldout = simulate(&c, &data, &sig, validation_end, data.end, apr, 1., true)?;
    let full = simulate(&c, &data, &sig, data.start, data.end, apr, 1., true)?;
    let double_cost = simulate(&c, &data, &sig, data.start, data.end, apr, 2., false)?;
    let low_apr = simulate(&c, &data, &sig, data.start, data.end, apr * 0.75, 1., false)?;
    let qualifies = best.train.pnl > 0.
        && best.validation.pnl > 0.
        && heldout.pnl > 0.
        && full.pnl > 0.
        && full.max_drawdown_pct < 5.
        && heldout.max_drawdown_pct < 5.;
    let result = json!({"start_ms":data.start,"end_ms":data.end,"apr":apr,"parameter_sets":count,
        "elapsed_seconds":started.elapsed().as_secs_f64(),"selection":"train first92d + validation next30d, maximize weaker daily PnL after doubled costs; final62d excluded from ranking. Earlier research iterations inspected other candidates there, so this is not a fully blind holdout.",
        "six_month_profitable":full.pnl>0.,"passes_strict_validation":qualifies,"selected":best,"holdout":heldout,"full":full,"double_cost":double_cost,"apr_30_percent":low_apr,
        "all_candidates":candidates,"assumptions":["40% APR is hypothetical, not observed DLMM fees","fees only for whole-bar in-range actual principal; cash periods zero",
        "hourly prices; trade at next observed open using closed indicators; no maker fill benefit assumed",
        "terminal LP/hedge liquidation charged; funding settlement assigned to pre-decision hourly inventory",
        "native rent/gas reserve initially treated as fixed USD; separate rent feasibility and intrabar tests required"]});
    std::fs::create_dir_all(output.parent().context("output parent")?)?;
    std::fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    let mut selected = c.clone();
    selected.mode = crate::config::Mode::Paper;
    selected.state_dir = "data/solana-regime-paper".into();
    selected.logging.directory = "data/logs/solana-regime".into();
    std::fs::write(
        output.with_file_name("selected-regime.toml"),
        toml::to_string_pretty(&selected)?,
    )?;
    println!(
        "SOL research completed: strict_validation={qualifies}, full PnL={:.4}, holdout={:.4}, selected={}",
        full.pnl,
        heldout.pnl,
        serde_json::to_string(&best.parameters)?
    );
    Ok(())
}

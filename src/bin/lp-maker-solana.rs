use anyhow::Result;
use clap::{Parser, Subcommand};
use lp_maker::solana::{config::Config, runner};
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Solana Meteora DLMM + Hyperliquid; independent state and execution")]
struct Cli {
    #[arg(long, default_value = "config/solana.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Check,
    Monitor {
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        execute: bool,
    },
    Backtest {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Research {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 0.4)]
        apr: f64,
        #[arg(long)]
        grid: Option<PathBuf>,
    },
    VerifyResearch {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 0.4)]
        apr: f64,
        /// 将该数量原生 SOL 储备计入总账户净值及成本。
        #[arg(long)]
        native_reserve_sol: Option<f64>,
    },
    /// 可恢复的离线训练；固定 40% LP APR，默认计算 10 小时。
    Train {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 10.)]
        hours: f64,
        #[arg(long, default_value_t = 0.25)]
        target_return: f64,
        #[arg(long, default_value_t = 250919)]
        seed: u64,
        #[arg(long)]
        max_candidates: Option<u64>,
    },
    Status,
    Reconcile,
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = Config::load(cli.config)?;
    let _guard = lp_maker::logging::init(&c.logging)?;
    match cli.command {
        Command::Check => println!("Solana config valid; no keys read, no network or transactions"),
        Command::Monitor { seconds } => runner::monitor(c, seconds).await?,
        Command::Run { once, execute } => runner::run(c, once, execute).await?,
        Command::Backtest { data, output } => {
            lp_maker::solana::backtest::run(&c, &data, &output)?;
            println!("Backtest saved: {}", output.display());
        }
        Command::Research {
            data,
            output,
            apr,
            grid,
        } => {
            lp_maker::solana::research::run(&c, &data, &output, apr, grid.as_deref())?;
        }
        Command::VerifyResearch {
            data,
            output,
            apr,
            native_reserve_sol,
        } => {
            if let Some(native_sol) = native_reserve_sol {
                lp_maker::solana::research::verify_account(&c, &data, &output, apr, native_sol)?;
            } else {
                lp_maker::solana::research::verify(&c, &data, &output, apr)?;
            }
        }
        Command::Train {
            data,
            output,
            hours,
            target_return,
            seed,
            max_candidates,
        } => {
            lp_maker::solana::research::training::train(
                &c,
                &data,
                &output,
                hours,
                target_return,
                seed,
                max_candidates,
            )?;
        }
        Command::Status => {
            let store = lp_maker::store::Store::readonly(&c.state_dir);
            let mut v = serde_json::json!({"checkpoint":store.read::<serde_json::Value>("solana_checkpoint.json")?,"pending":store.read::<serde_json::Value>("solana_pending.json")?,"hl_pending":store.pending()?,"positions":store.read::<serde_json::Value>("solana_positions.json")?});
            lp_maker::store::redact_signatures(&mut v);
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Command::Reconcile => {
            let store = std::sync::Arc::new(lp_maker::store::Store::open(&c.state_dir)?);
            runner::bind(&store, &c)?;
            let b = lp_maker::solana::bridge::Bridge::start(&c, false).await?;
            lp_maker::solana::journal::reconcile(&b, &store).await?;
            lp_maker::hyperliquid::hedge::reconcile(&c.hyperliquid, store).await?;
        }
    }
    Ok(())
}

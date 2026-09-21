//! 操作者显式申请一次「按第一仓入场」。不删除历史，也不修改共用策略状态机。
//! 许可在发送任何建仓交易前落盘为已使用，崩溃重启不能再次豁免冷却。
use super::{Live, transport_independent_fingerprint};
use crate::{
    config::{Config, Mode},
    domain::{Decision, LiquidityVenue, LpIntent, MarketFrame, Portfolio},
    hyperliquid::{Client, journal::Orders},
    liquidity::uniswap_v3::tx::Executor,
    recovery,
    store::Store,
    strategy::{EntryHistory, Phase, Strategy},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

const FILE: &str = "entry_rearm.json";
const REASON: &str = "manual_entry_rearm:";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Armed,
    Consumed,
    Canceled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Permit {
    pub schema: u32,
    pub id: String,
    pub binding: String,
    pub requested_ms: u64,
    pub status: Status,
    pub updated_ms: u64,
}
fn binding(c: &Config) -> Result<String> {
    let value = serde_json::to_string(&json!({"mode":c.mode,"liquidity":c.liquidity,
        "strategy":c.strategy,"hyperliquid":c.hyperliquid}))?;
    Ok(
        alloy::primitives::keccak256(serde_json::to_vec(&transport_independent_fingerprint(
            &value,
        )?)?)
        .to_string(),
    )
}
fn read(store: &Store, c: &Config) -> Result<Option<Permit>> {
    let p = store.read::<Permit>(FILE)?;
    if let Some(p) = &p {
        ensure!(
            p.schema == 1 && p.binding == binding(c)?,
            "entry rearm belongs to another configuration or schema"
        );
    }
    Ok(p)
}
fn flat(p: &Portfolio) -> bool {
    p.positions.is_empty()
        && p.wallet_base.is_finite()
        && p.short_base.is_finite()
        && p.wallet_base.abs() <= 1e-8
        && p.short_base.abs() <= 1e-8
}
fn settled(store: &Store) -> Result<()> {
    ensure!(
        store.pending()?.is_none()
            && store.read::<Value>("workflow.json")?.is_none()
            && store.read::<Value>("hedge_order.json")?.is_none(),
        "重新入场要求所有未决交易、挂单和调仓流程已完成；保留原记录并先正常对账"
    );
    ensure!(
        store
            .read::<Orders>("orders.json")?
            .unwrap_or_default()
            .values()
            .all(|o| o.terminal),
        "订单台账仍有未决订单，不能重新入场"
    );
    Ok(())
}

/// CLI 只核对远端状态并写入本地许可，不下单、不撤单、不平仓。
pub async fn request(c: Config, store: Arc<Store>, execute: bool) -> Result<Permit> {
    ensure!(
        c.mode == Mode::Live && execute,
        "rearm-entry requires live config and --execute"
    );
    let saved_config = store
        .read::<String>("config.json")?
        .context("missing original config binding")?;
    let current = serde_json::to_string(&json!({"mode":c.mode,"liquidity":c.liquidity,
        "strategy":c.strategy,"hyperliquid":c.hyperliquid}))?;
    ensure!(
        transport_independent_fingerprint(&saved_config)?
            == transport_independent_fingerprint(&current)?,
        "configuration changed; refusing entry rearm"
    );
    ensure!(
        store
            .read::<recovery::Checkpoint>("checkpoint.json")?
            .is_some(),
        "missing original checkpoint"
    );
    settled(&store)?;
    let (strategy, _) = recovery::load(&store, &c)?;
    ensure!(
        strategy.phase == Phase::Paused,
        "仅允许在 Paused 空仓状态申请重新入场，不解除 Halted 熔断"
    );
    let venue = crate::liquidity::connect(c.liquidity.clone())?;
    venue.validate().await?;
    let evm = Executor::new(venue, store.clone())?;
    evm.nonce.refresh().await?.available()?;
    evm.verify_inventory().await?;
    let mut hl = Client::new(c.hyperliquid.clone(), store.clone())?;
    hl.enable_signing().await?; // 验证 API wallet 对主账户的权限；后续仅调用只读接口。
    let live = Live {
        liquidity: Arc::new(evm),
        hl,
        store: store.clone(),
        cfg: c.clone(),
    };
    live.sync_orders(false).await?; // 不撤销任何遗留挂单；有挂单就阻断。
    let actual = live.portfolio().await?;
    arm(&store, &c, &strategy, &actual)
}

/// 调用者必须先核对真实链上仓位、钱包、交易所订单、nonce 和合约持仓。
pub fn arm(store: &Store, c: &Config, strategy: &Strategy, actual: &Portfolio) -> Result<Permit> {
    ensure!(
        c.mode == Mode::Live,
        "entry rearm is an EVM live operator action"
    );
    ensure!(
        strategy.phase == Phase::Paused,
        "entry rearm requires Paused; Halted is never reset"
    );
    ensure!(flat(actual), "重新入场要求 LP、WETH 库存和合约均为空仓");
    settled(store)?;
    if let Some(p) = read(store, c)?
        && p.status == Status::Armed
    {
        return Ok(p); // 重复执行申请命令不叠加许可，也不覆盖原备份。
    }
    let checkpoint = store
        .read::<recovery::Checkpoint>("checkpoint.json")?
        .context("missing checkpoint")?;
    ensure!(
        checkpoint.mode == Mode::Live && checkpoint.schema == 1,
        "invalid live checkpoint"
    );
    let p = Permit {
        schema: 1,
        id: uuid::Uuid::new_v4().simple().to_string(),
        binding: binding(c)?,
        requested_ms: crate::now_ms(),
        status: Status::Armed,
        updated_ms: crate::now_ms(),
    };
    store.write(&format!("entry_rearm_backup_{}.json", p.id), &checkpoint)?;
    store.write(FILE, &p)?;
    store.event("entry_rearm_armed", json!({"permit":p,"strategy":strategy,
        "note":"explicit operator request; waive waiting for one entry only; preserve history, peak equity and all market checks"}))?;
    tracing::info!(request_id=%p.id, "已核对空仓，下一次入场按第一仓规则检查；仅本次跳过冷却与健康小时等待");
    Ok(p)
}

/// 仅 EVM 运行器使用。临时采用首仓等待规则，立即还原真实建仓历史。
pub fn evaluate(
    store: &Store,
    c: &Config,
    strategy: &mut Strategy,
    frame: &MarketFrame,
) -> Result<Decision> {
    let Some(mut permit) = read(store, c)? else {
        return Ok(strategy.evaluate_with_continuity(&c.strategy, frame));
    };
    if permit.status != Status::Armed {
        return Ok(strategy.evaluate_with_continuity(&c.strategy, frame));
    }
    ensure!(
        c.mode == Mode::Live,
        "entry rearm cannot be used in paper mode"
    );
    if strategy.phase != Phase::Paused || !flat(&frame.portfolio) {
        permit.status = Status::Canceled;
        permit.updated_ms = crate::now_ms();
        store.write(FILE, &permit)?;
        store.event(
            "entry_rearm_canceled",
            json!({"permit":permit,"reason":"phase changed or inventory no longer flat"}),
        )?;
        return Ok(strategy.evaluate_with_continuity(&c.strategy, frame));
    }
    settled(store)?;
    let mut trial = strategy.clone();
    let history = trial.entry_history.clone();
    trial.entry_history = EntryHistory::Initial;
    let mut decision = trial.evaluate_with_continuity(&c.strategy, frame);
    trial.entry_history = history; // 历史不是「从未建仓」；只豁免这一次的等待。
    *strategy = trial;
    if matches!(decision.lp, LpIntent::Deploy { .. }) {
        decision
            .reasons
            .retain(|r| !r.starts_with("initial_entry:"));
        decision.reasons.push(format!("{REASON}{}", permit.id));
    }
    if strategy.phase == Phase::Halted {
        permit.status = Status::Canceled;
        permit.updated_ms = crate::now_ms();
        store.write(FILE, &permit)?;
        store.event(
            "entry_rearm_canceled",
            json!({"permit":permit,"reason":"drawdown halt"}),
        )?;
    }
    Ok(decision)
}

/// 保证金检查后、检查点提交和真实交易前消费许可。失败重启仍按原冷却恢复。
pub fn consume(store: &Store, c: &Config, decision: &Decision) -> Result<()> {
    if !matches!(decision.lp, LpIntent::Deploy { .. }) {
        return Ok(());
    }
    let Some(id) = decision.reasons.iter().find_map(|r| r.strip_prefix(REASON)) else {
        return Ok(());
    };
    let mut p = read(store, c)?.context("missing entry rearm permit")?;
    ensure!(
        p.id == id && p.status == Status::Armed,
        "entry rearm already consumed or replaced"
    );
    p.status = Status::Consumed;
    p.updated_ms = crate::now_ms();
    store.write(FILE, &p)?;
    store.event("entry_rearm_consumed", &p)?;
    tracing::info!(request_id=%p.id, "本次首仓许可已使用，后续重新建仓恢复原冷却与健康小时规则");
    Ok(())
}

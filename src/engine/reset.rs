//! 显式人工退出：先处理真实资产，最后才归档并清空本地状态。
//! 与自动策略隔离；任何失败保留维护标记和交易台账，再运行本命令只按真实余额继续。
use crate::{
    config::{Config, Mode},
    domain::{HedgeVenue, LiquidityVenue},
    hyperliquid::{Client, journal, orders},
    liquidity::uniswap_v3::{dust::BaseDust, tx::Executor},
    store::Store,
};
use alloy::primitives::U256;
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

const MARKER: &str = "manual_reset.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum Step {
    Reconcile,
    CancelOrders,
    RemoveLp,
    SellBase,
    CloseHedge,
}

#[derive(Debug, Serialize)]
struct Proof {
    lp_ids: Vec<String>,
    base_raw: String,
    base_dust: Option<BaseDust>,
    hedge_size: String,
    open_orders: usize,
    pending: bool,
    unresolved_orders: usize,
}
impl Proof {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.lp_ids.is_empty()
                && Decimal::from_str(&self.hedge_size)?.is_zero()
                && self.open_orders == 0
                && !self.pending
                && self.unresolved_orders == 0,
            "仍有 LP/代币/合约/挂单或未决记录，未清空状态、未重启；请查看退出日志后再运行同一命令"
        );
        let base = U256::from_str_radix(&self.base_raw, 10)?;
        if !base.is_zero() {
            self.base_dust
                .as_ref()
                .context("基础币余额非零且没有本次核对的尾差证据，保留原状态")?
                .validate(base)?;
        } else {
            ensure!(
                self.base_dust.is_none(),
                "zero balance has inconsistent dust evidence"
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
trait Actions {
    async fn perform(&self, step: Step) -> Result<()>;
    async fn observe(&self) -> Result<Proof>;
}

async fn flatten(a: &impl Actions, store: &Store) -> Result<Option<BaseDust>> {
    let mut marker = store
        .read::<Value>(MARKER)?
        .context("missing reset marker")?;
    for step in [
        Step::Reconcile,
        Step::CancelOrders,
        Step::RemoveLp,
        Step::SellBase,
        Step::CloseHedge,
    ] {
        marker["stage"] = serde_json::to_value(step)?;
        marker["updated_ms"] = json!(crate::now_ms());
        store.write(MARKER, &marker)?;
        tracing::info!(?step, "手工退出阶段开始；完成真实平仓前保留全部记录");
        a.perform(step).await?;
    }
    // 连续两次读取真实余额；IOC 的成功回执不等于仓位已经全部成交。
    let mut retained_dust = None;
    for _ in 0..2 {
        let proof = a.observe().await?;
        store.event("manual_exit_verification", &proof)?;
        store.write(
            "manual_reset_proof.json",
            &json!({"observed_ms":crate::now_ms(),"proof":proof}),
        )?;
        proof.validate()?;
        retained_dust = proof.base_dust;
    }
    marker["stage"] = json!("verified_flat");
    marker["retained_base_dust"] = serde_json::to_value(&retained_dust)?;
    store.write(MARKER, &marker)?;
    Ok(retained_dust)
}

struct LiveExit<'a> {
    cfg: &'a Config,
    evm: Executor,
    hl: Client,
    store: Arc<Store>,
}

/// 只处理配置的对冲币。混用账户不能被一次重置误平其他策略，且不能清掉它们的台账。
fn hedge_size(account: &Value, coin: &str) -> Result<Decimal> {
    let mut size = Decimal::ZERO;
    let mut seen = false;
    for row in account["assetPositions"]
        .as_array()
        .context("missing assetPositions")?
    {
        let p = &row["position"];
        let amount = Decimal::from_str(p["szi"].as_str().context("invalid position size")?)?;
        let symbol = p["coin"].as_str().context("invalid position coin")?;
        if symbol == coin {
            ensure!(!seen, "duplicate hedge position");
            seen = true;
            size = amount;
        } else {
            ensure!(
                amount.is_zero(),
                "Hyperliquid 存在其他币种 {symbol} 的持仓；本命令不处理其他策略，已停止"
            );
        }
    }
    Ok(size)
}
fn scoped_orders(open: &Value, coin: &str) -> Result<Vec<u64>> {
    open.as_array()
        .context("invalid open orders")?
        .iter()
        .map(|o| {
            ensure!(
                o["coin"] == coin,
                "Hyperliquid 存在其他币种挂单；本命令不处理其他策略，已停止"
            );
            o["oid"].as_u64().context("invalid order id")
        })
        .collect()
}

fn terminal_observation(response: &Value, oid: u64, coin: &str) -> Result<Option<bool>> {
    if response["status"] == "unknownOid" {
        return Ok(None);
    }
    ensure!(
        response["status"] == "order",
        "unexpected exit order response"
    );
    let o = &response["order"];
    ensure!(
        o["order"]["oid"].as_u64() == Some(oid) && o["order"]["coin"] == coin,
        "exit order identity mismatch"
    );
    let status = o["status"].as_str().context("missing order status")?;
    // triggered 结束的是条件单父单；后续仍须检查当前挂单和真实仓位，不能据此认定空仓。
    ensure!(
        status == "open" || status == "triggered" || journal::terminal_status(status),
        "unknown order status"
    );
    Ok(Some(status != "open"))
}

impl LiveExit<'_> {
    async fn order_terminal(&self, oid: u64) -> Result<bool> {
        for attempt in 0..5 {
            let response = self.hl.order_status(json!(oid)).await?;
            self.store.event(
                "manual_exit_order_status",
                json!({"oid":oid,"attempt":attempt+1,"response":response}),
            )?;
            if let Some(terminal) =
                terminal_observation(&response, oid, &self.cfg.hyperliquid.hedge_coin)?
            {
                return Ok(terminal);
            }
            if attempt < 4 {
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
            }
        }
        anyhow::bail!("订单 {oid} 状态未知，保留记录，不能当作已撤单")
    }

    async fn cancel_orders(&self) -> Result<()> {
        let open = self.hl.open_orders().await?;
        let mut ids = self
            .store
            .read::<Vec<u64>>("manual_exit_orders.json")?
            .unwrap_or_default();
        ids.extend(scoped_orders(&open, &self.cfg.hyperliquid.hedge_coin)?);
        ids.sort_unstable();
        ids.dedup();
        // 先落盘 OID，再撤单；重试仍会检查原订单终态，不只依赖空的挂单列表。
        self.store.write("manual_exit_orders.json", &ids)?;
        for oid in ids {
            if !self.order_terminal(oid).await? {
                self.hl
                    .cancel_oid(&self.cfg.hyperliquid.hedge_coin, oid)
                    .await?;
                let mut terminal = false;
                for attempt in 0..5 {
                    if self.order_terminal(oid).await? {
                        terminal = true;
                        break;
                    }
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                    }
                }
                ensure!(terminal, "订单 {oid} 撤销尚未确认；保留状态后重试");
            }
        }
        ensure!(
            scoped_orders(
                &self.hl.open_orders().await?,
                &self.cfg.hyperliquid.hedge_coin
            )?
            .is_empty(),
            "仍有挂单，退出已暂停"
        );
        journal::refresh(&self.hl, &self.store).await?;
        ensure!(
            unresolved(&self.store)? == 0,
            "仍有本地订单未确认终态，保留原记录"
        );
        self.store
            .write("hedge_order.json", &Option::<Value>::None)?;
        Ok(())
    }

    async fn remove_lp(&self) -> Result<()> {
        let ids = self.evm.venue.exit_token_ids(self.evm.owner()).await?;
        let old = self.evm.ids()?;
        let registry: BTreeMap<String, String> = ids
            .into_iter()
            .map(|id| {
                let layer = old
                    .iter()
                    .find(|(_, v)| v == &id)
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| format!("manual_exit_{id}"));
                (layer, id)
            })
            .collect();
        self.store.event(
            "manual_exit_inventory",
            json!({"previous":old,"actual":registry}),
        )?;
        // 临时标签仅供手工退出使用，不能进入自动策略；维护标记会拦住普通 run。
        self.store.write("nfts.json", &registry)?;
        for (layer, id) in registry {
            let snap = self.evm.venue.execution_snapshot().await?;
            let positions = self
                .evm
                .venue
                .position_observations(&self.evm.owner().to_string(), &[(layer, id.clone())], &snap)
                .await?;
            let (position, _) = positions.first().context("missing exit position")?;
            tracing::info!(token_id = id, "退出并领取当前池 LP，等待链上确认");
            self.evm.remove(position).await?;
        }
        ensure!(
            self.evm
                .venue
                .exit_token_ids(self.evm.owner())
                .await?
                .is_empty(),
            "仍有当前池 NFT，未完成退出"
        );
        Ok(())
    }

    async fn close_hedge(&self) -> Result<()> {
        let coin = &self.cfg.hyperliquid.hedge_coin;
        let (_, asset) = self.hl.asset(coin).await?;
        for _ in 0..3 {
            let size = hedge_size(&self.hl.perp_account().await?, coin)?;
            if size.is_zero() {
                return Ok(());
            }
            ensure!(
                scoped_orders(&self.hl.open_orders().await?, coin)?.is_empty(),
                "平仓期间出现新挂单，已停止"
            );
            let (bid, ask, time) = self.hl.book(coin).await?;
            crate::runtime::fresh(
                time,
                crate::now_ms(),
                self.cfg.strategy.max_data_age_seconds,
            )?;
            let buy = size.is_sign_negative();
            let slip = f64::from(self.cfg.hyperliquid.emergency_slippage_bps) / 10000.0;
            let price = orders::price(
                if buy {
                    ask * (1.0 + slip)
                } else {
                    bid * (1.0 - slip)
                },
                asset.sz_decimals,
                buy,
            )?;
            let id = orders::cloid();
            tracing::info!(coin,size=%size,"只减仓 IOC 平掉真实合约余额；可能产生吃单手续费");
            self.hl
                .order(
                    coin,
                    buy,
                    &price,
                    &size.abs().normalize().to_string(),
                    "Ioc",
                    true,
                    &id,
                )
                .await?;
            crate::hyperliquid::order_recovery::resolve(&self.hl, &self.store, &id, true).await?;
            // 仅在上一单终态已确认后，重新读取真实剩余仓位，最多补平两次。
        }
        ensure!(
            hedge_size(&self.hl.perp_account().await?, coin)?.is_zero(),
            "IOC 后仍有合约余仓，保留状态后重试"
        );
        Ok(())
    }
}

fn unresolved(store: &Store) -> Result<usize> {
    Ok(store
        .read::<journal::Orders>("orders.json")?
        .unwrap_or_default()
        .values()
        .filter(|o| !o.terminal)
        .count())
}

#[async_trait::async_trait]
impl Actions for LiveExit<'_> {
    async fn perform(&self, step: Step) -> Result<()> {
        match step {
            Step::Reconcile => {
                super::reconcile(self.cfg, self.store.clone()).await?;
                self.evm.nonce.refresh().await?.available()?;
                hedge_size(
                    &self.hl.perp_account().await?,
                    &self.cfg.hyperliquid.hedge_coin,
                )?;
                scoped_orders(
                    &self.hl.open_orders().await?,
                    &self.cfg.hyperliquid.hedge_coin,
                )?;
                Ok(())
            }
            Step::CancelOrders => self.cancel_orders().await,
            Step::RemoveLp => self.remove_lp().await,
            Step::SellBase => {
                if self
                    .evm
                    .venue
                    .balance(self.evm.venue.base, self.evm.owner())
                    .await?
                    > U256::ZERO
                {
                    tracing::info!(
                        "核对并兑换基础代币；严格限额的微小尾差保留并记录，原生 Gas 币保留"
                    );
                    self.evm.sell_all_base().await?;
                }
                Ok(())
            }
            Step::CloseHedge => self.close_hedge().await,
        }
    }
    async fn observe(&self) -> Result<Proof> {
        self.evm.nonce.refresh().await?.available()?;
        journal::refresh(&self.hl, &self.store).await?;
        let base = self
            .evm
            .venue
            .balance(self.evm.venue.base, self.evm.owner())
            .await?;
        let base_dust = if base.is_zero() {
            None
        } else {
            let snapshot = self.evm.venue.fresh_snapshot().await?;
            crate::runtime::fresh(
                snapshot.time_ms,
                crate::now_ms(),
                self.cfg.strategy.max_data_age_seconds,
            )?;
            BaseDust::assess(&self.cfg.liquidity, &snapshot, base)?
        };
        let proof = Proof {
            lp_ids: self.evm.venue.exit_token_ids(self.evm.owner()).await?,
            base_raw: base.to_string(),
            base_dust,
            hedge_size: hedge_size(
                &self.hl.perp_account().await?,
                &self.cfg.hyperliquid.hedge_coin,
            )?
            .to_string(),
            open_orders: scoped_orders(
                &self.hl.open_orders().await?,
                &self.cfg.hyperliquid.hedge_coin,
            )?
            .len(),
            pending: self.store.pending()?.is_some(),
            unresolved_orders: unresolved(&self.store)?,
        };
        Ok(proof)
    }
}

pub async fn request(cfg: Config, store: Arc<Store>, execute: bool) -> Result<Value> {
    ensure!(
        cfg.mode == Mode::Live && execute,
        "reset-flat requires live config AND --execute"
    );
    ensure!(store.is_writable(), "reset requires exclusive state lock");
    validate_state_directory(Path::new(&cfg.state_dir))?;
    let current = serde_json::to_string(
        &json!({"mode":cfg.mode,"liquidity":cfg.liquidity,"strategy":cfg.strategy,"hyperliquid":cfg.hyperliquid}),
    )?;
    let binding = super::transport_independent_fingerprint(&current)?;
    if let Some(saved) = store.read::<String>("config.json")? {
        ensure!(
            super::transport_independent_fingerprint(&saved)? == binding,
            "reset config differs from saved state"
        );
    }
    if let Some(marker) = store.read::<Value>(MARKER)? {
        ensure!(
            marker["binding"] == binding,
            "reset belongs to another configuration"
        );
    } else {
        store.write(
            MARKER,
            &json!({"binding":binding,"stage":"started","started_ms":crate::now_ms()}),
        )?;
    }
    let venue = crate::liquidity::connect(cfg.liquidity.clone())?;
    venue.validate().await?;
    let evm = Executor::new(venue, store.clone())?;
    let mut hl = Client::new(cfg.hyperliquid.clone(), store.clone())?;
    hl.enable_signing().await?;
    let retained_dust = flatten(
        &LiveExit {
            cfg: &cfg,
            evm,
            hl,
            store: store.clone(),
        },
        &store,
    )
    .await?;
    let backup = archive_and_clear(&store, Path::new(&cfg.state_dir))?;
    tracing::info!(backup=%backup.display(), retained_base_dust=?retained_dust,
        "已确认 LP、合约及挂单退出；基础币余额为零或已核对微小尾差，旧记录完整归档，可按第一仓规则启动");
    Ok(
        json!({"status":if retained_dust.is_some(){"flat_with_base_dust_and_reset"}else{"flat_and_reset"},"retained_base_dust":retained_dust,"backup":backup,"state_dir":cfg.state_dir,"pool":cfg.liquidity.pool,"hedge_coin":cfg.hyperliquid.hedge_coin}),
    )
}

fn validate_state_directory(root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    ensure!(
        !cwd.starts_with(&root) && !root.join("Cargo.toml").exists() && !root.join(".git").exists(),
        "reset requires a dedicated state directory, not the project or its parent"
    );
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            ensure!(
                entry.file_name() == "event_archive" || entry.file_name() == "order_archive",
                "state directory contains an unexpected subdirectory; refusing to clear another strategy or its backups"
            );
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(to, fs::Permissions::from_mode(0o700))?;
    }
    for item in fs::read_dir(from)? {
        let item = item?;
        let kind = item.file_type()?;
        ensure!(!kind.is_symlink(), "state contains symlink; refusing reset");
        if item.file_name() == "process.lock" {
            continue;
        }
        let dst = to.join(item.file_name());
        if kind.is_dir() {
            copy_tree(&item.path(), &dst)?;
        } else {
            ensure!(kind.is_file(), "unexpected state file type");
            fs::copy(item.path(), &dst)?;
            fs::File::open(dst)?.sync_all()?;
        }
    }
    fs::File::open(to)?.sync_all()?;
    Ok(())
}

fn archive_and_clear(store: &Store, root: &Path) -> Result<PathBuf> {
    ensure!(
        store.is_writable() && store.pending()?.is_none() && unresolved(store)? == 0,
        "unsettled state cannot be reset"
    );
    ensure!(
        store
            .read::<Value>(MARKER)?
            .is_some_and(|m| m["stage"] == "verified_flat"),
        "flat verification required before reset"
    );
    let root = root.canonicalize()?;
    let parent = root.parent().context("state directory parent")?;
    let name = root
        .file_name()
        .context("state directory name")?
        .to_string_lossy();
    let backup = parent.join(format!(
        "{name}_reset_backup_{}",
        uuid::Uuid::new_v4().simple()
    ));
    // 完整复制并 fsync 成功后才开始清理。维护标记最后删除，进程锁文件始终原地保留。
    copy_tree(&root, &backup)?;
    fs::File::open(parent)?.sync_all()?;
    for item in fs::read_dir(&root)? {
        let item = item?;
        if item.file_name() == "process.lock"
            || item.file_name() == MARKER
            || item.file_name() == super::migration::MARKER
        {
            continue;
        }
        if item.file_type()?.is_dir() {
            fs::remove_dir_all(item.path())?;
        } else {
            fs::remove_file(item.path())?;
        }
    }
    fs::remove_file(root.join(MARKER))?;
    fs::File::open(root)?.sync_all()?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Fake {
        calls: Mutex<Vec<Step>>,
        failure: Option<Step>,
        flat: bool,
    }
    #[async_trait::async_trait]
    impl Actions for Fake {
        async fn perform(&self, step: Step) -> Result<()> {
            self.calls.lock().unwrap().push(step);
            ensure!(self.failure != Some(step), "injected uncertain operation");
            Ok(())
        }
        async fn observe(&self) -> Result<Proof> {
            Ok(proof(if self.flat { "0" } else { "0.0001" }))
        }
    }
    fn proof(size: &str) -> Proof {
        Proof {
            lp_ids: vec![],
            base_raw: "0".into(),
            base_dust: None,
            hedge_size: size.into(),
            open_orders: 0,
            pending: false,
            unresolved_orders: 0,
        }
    }
    #[test]
    fn unknown_or_wrong_order_never_proves_cancellation() {
        assert_eq!(
            terminal_observation(&json!({"status":"unknownOid"}), 7, "ETH").unwrap(),
            None
        );
        let mut response =
            json!({"status":"order","order":{"status":"open","order":{"oid":7,"coin":"ETH"}}});
        assert_eq!(
            terminal_observation(&response, 7, "ETH").unwrap(),
            Some(false)
        );
        response["order"]["status"] = json!("canceled");
        assert_eq!(
            terminal_observation(&response, 7, "ETH").unwrap(),
            Some(true)
        );
        assert!(terminal_observation(&response, 8, "ETH").is_err());
        assert!(terminal_observation(&response, 7, "SOL").is_err());
    }
    #[tokio::test]
    async fn maintenance_marker_blocks_strategy_before_any_network_or_new_position() {
        let (_dir, s) = setup();
        let c = Config::load("config/paper-200.toml").unwrap();
        let e = super::super::run(c, Arc::new(s), true, false, false)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("reset-flat"));
    }
    #[tokio::test]
    async fn reset_requires_explicit_live_execution_before_any_state_change() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(Store::open(dir.path()).unwrap());
        let mut c = Config::load("config/paper-200.toml").unwrap();
        assert!(request(c.clone(), s.clone(), true).await.is_err());
        c.mode = Mode::Live;
        assert!(request(c, s.clone(), false).await.is_err());
        assert!(s.read::<Value>(MARKER).unwrap().is_none());
    }
    fn setup() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state")).unwrap();
        store
            .write(MARKER, &json!({"stage":"started","binding":"test"}))
            .unwrap();
        store
            .write("checkpoint.json", &json!({"preserve":"old"}))
            .unwrap();
        (dir, store)
    }
    #[tokio::test]
    async fn every_failed_stage_preserves_checkpoint_and_blocks_reset() {
        for step in [
            Step::Reconcile,
            Step::CancelOrders,
            Step::RemoveLp,
            Step::SellBase,
            Step::CloseHedge,
        ] {
            let (dir, s) = setup();
            let a = Fake {
                calls: Mutex::default(),
                failure: Some(step),
                flat: true,
            };
            assert!(flatten(&a, &s).await.is_err());
            assert_eq!(a.calls.lock().unwrap().last(), Some(&step));
            assert!(archive_and_clear(&s, &dir.path().join("state")).is_err());
            assert_eq!(
                s.read::<Value>("checkpoint.json").unwrap().unwrap()["preserve"],
                "old"
            );
        }
    }
    #[tokio::test]
    async fn partial_fill_does_not_clear_state() {
        let (dir, s) = setup();
        let a = Fake {
            calls: Mutex::default(),
            failure: None,
            flat: false,
        };
        assert!(flatten(&a, &s).await.is_err());
        assert!(archive_and_clear(&s, &dir.path().join("state")).is_err());
        assert!(s.read::<Value>(MARKER).unwrap().is_some());
    }
    #[tokio::test]
    async fn verified_exit_archives_all_records_then_clears_under_original_lock() {
        let (dir, s) = setup();
        let a = Fake {
            calls: Mutex::default(),
            failure: None,
            flat: true,
        };
        flatten(&a, &s).await.unwrap();
        assert_eq!(
            *a.calls.lock().unwrap(),
            vec![
                Step::Reconcile,
                Step::CancelOrders,
                Step::RemoveLp,
                Step::SellBase,
                Step::CloseHedge
            ]
        );
        let root = dir.path().join("state");
        let backup = archive_and_clear(&s, &root).unwrap();
        assert!(backup.join("checkpoint.json").exists());
        assert!(backup.join("manual_reset_proof.json").exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert!(Store::open(&root).is_err());
        drop(s);
        assert!(Store::open(root).is_ok());
    }
    #[tokio::test]
    async fn config_migration_guard_survives_reset_and_is_also_archived() {
        let (dir, s) = setup();
        let guard = json!({"schema":1,"profile":"in_progress"});
        s.write(super::super::migration::MARKER, &guard).unwrap();
        let a = Fake {
            calls: Mutex::default(),
            failure: None,
            flat: true,
        };
        flatten(&a, &s).await.unwrap();
        let backup = archive_and_clear(&s, &dir.path().join("state")).unwrap();
        assert_eq!(
            s.read::<Value>(super::super::migration::MARKER).unwrap(),
            Some(guard)
        );
        assert!(backup.join(super::super::migration::MARKER).is_file());
        assert_eq!(fs::read_dir(dir.path().join("state")).unwrap().count(), 2);
    }
    #[test]
    fn flat_proof_rejects_unproven_dust_pending_orders_and_unclaimed_lp() {
        let mut p = proof("0");
        assert!(p.validate().is_ok());
        p.base_raw = "1".into();
        assert!(p.validate().is_err());
        let mut p = proof("0");
        p.lp_ids.push("1229735".into());
        assert!(p.validate().is_err());
        let mut p = proof("0");
        p.pending = true;
        assert!(p.validate().is_err());
        let mut p = proof("0");
        p.open_orders = 1;
        assert!(p.validate().is_err());
        let mut p = proof("0");
        p.unresolved_orders = 1;
        assert!(p.validate().is_err());
    }
    fn dust_proof() -> Proof {
        let mut p = proof("0");
        p.base_raw = "17".into();
        p.base_dust = Some(BaseDust {
            raw_base: p.base_raw.clone(),
            raw_quote_ceiling: "1".into(),
            base_decimals: 18,
            quote_decimals: 6,
            sqrt_price_x96: "3953120541360100857610261".into(),
            base_is_token0: true,
            block: 256,
            block_hash: "0xcanonical".into(),
            time_ms: crate::now_ms(),
        });
        p
    }
    #[test]
    fn dust_evidence_must_match_balance_and_does_not_relax_other_checks() {
        assert!(dust_proof().validate().is_ok());
        let mut p = dust_proof();
        p.base_raw = "18".into();
        assert!(p.validate().is_err());
        let mut p = dust_proof();
        p.base_dust.as_mut().unwrap().raw_quote_ceiling = "0".into();
        assert!(p.validate().is_err());
        let mut p = dust_proof();
        p.base_raw = "0".into();
        assert!(p.validate().is_err());
        for kind in 0..5 {
            let mut p = dust_proof();
            match kind {
                0 => p.pending = true,
                1 => p.open_orders = 1,
                2 => p.hedge_size = "-0.0001".into(),
                3 => p.lp_ids.push("7".into()),
                _ => p.unresolved_orders = 1,
            }
            assert!(p.validate().is_err());
        }
    }
    struct Observations(Mutex<std::collections::VecDeque<Proof>>);
    #[async_trait::async_trait]
    impl Actions for Observations {
        async fn perform(&self, _: Step) -> Result<()> {
            Ok(())
        }
        async fn observe(&self) -> Result<Proof> {
            Ok(self.0.lock().unwrap().pop_front().unwrap())
        }
    }
    #[tokio::test]
    async fn verified_dust_is_archived_as_nonzero_balance_with_both_observations() {
        let (dir, s) = setup();
        let a = Observations(Mutex::new([dust_proof(), dust_proof()].into()));
        let retained = flatten(&a, &s).await.unwrap().unwrap();
        assert_eq!(retained.raw_base, "17");
        assert!(a.0.lock().unwrap().is_empty());
        let backup = archive_and_clear(&s, &dir.path().join("state")).unwrap();
        let proof: Value =
            serde_json::from_slice(&fs::read(backup.join("manual_reset_proof.json")).unwrap())
                .unwrap();
        assert_eq!(proof["proof"]["base_raw"], "17");
        assert_eq!(proof["proof"]["base_dust"]["raw_quote_ceiling"], "1");
        let events = fs::read_to_string(backup.join("events.jsonl")).unwrap();
        assert_eq!(
            events
                .lines()
                .filter(|l| l.contains("manual_exit_verification"))
                .count(),
            2
        );
    }
    #[tokio::test]
    async fn second_observation_with_larger_balance_stops_reset_and_keeps_state() {
        let (dir, s) = setup();
        let mut grown = dust_proof();
        // 即使伪造“证据”使余额相等，超出估值限额仍不得清空状态。
        grown.base_raw = "1000000000".into();
        grown.base_dust.as_mut().unwrap().raw_base = grown.base_raw.clone();
        let a = Observations(Mutex::new([dust_proof(), grown].into()));
        assert!(flatten(&a, &s).await.is_err());
        assert!(a.0.lock().unwrap().is_empty());
        assert!(archive_and_clear(&s, &dir.path().join("state")).is_err());
        assert!(s.read::<Value>("checkpoint.json").unwrap().is_some());
        assert_ne!(
            s.read::<Value>(MARKER).unwrap().unwrap()["stage"],
            "verified_flat"
        );
    }
    #[test]
    fn reset_cannot_target_project_or_parent_of_other_strategy_states() {
        assert!(validate_state_directory(Path::new(".")).is_err());
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("other-strategy")).unwrap();
        assert!(validate_state_directory(dir.path()).is_err());
        assert!(validate_state_directory(&dir.path().join("other-strategy")).is_ok());
    }
    #[test]
    fn scope_rejects_other_strategies_and_preserves_exact_position_size() {
        assert_eq!(
            hedge_size(
                &json!({"assetPositions":[{"position":{"coin":"ETH","szi":"-0.00010000"}}]}),
                "ETH"
            )
            .unwrap()
            .abs()
            .normalize()
            .to_string(),
            "0.0001"
        );
        assert!(
            hedge_size(
                &json!({"assetPositions":[{"position":{"coin":"SOL","szi":"1"}}]}),
                "ETH"
            )
            .is_err()
        );
        assert!(scoped_orders(&json!([{"coin":"SOL","oid":1}]), "ETH").is_err());
        assert!(hedge_size(&json!({}), "ETH").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn failed_backup_never_deletes_active_records() {
        let (dir, s) = setup();
        s.write(MARKER, &json!({"stage":"verified_flat"})).unwrap();
        std::os::unix::fs::symlink("/missing", dir.path().join("state/link")).unwrap();
        assert!(archive_and_clear(&s, &dir.path().join("state")).is_err());
        assert!(dir.path().join("state/checkpoint.json").exists());
        assert!(s.read::<Value>(MARKER).unwrap().is_some());
    }
}

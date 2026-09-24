//! Robinhood 策略切换：旧参数负责退出，新参数只在确认空仓后启用。
//! 不删除未决交易来“解决”恢复问题；复用 reset 的对账、平仓及双重空仓验证。
use crate::{
    config::{Config, Mode},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, future::Future, io::Write, path::Path, sync::Arc};

/// reset 清理时保留此标记，避免空仓之后、配置写入之前崩溃而重启旧策略。
pub const MARKER: &str = "strategy_migration.json";
const PROFILE: &str = "robinhood-eth-persistent-dd5-200";
const TEMPLATE: &str = include_str!("../../config/eth-persistent-dd5-paper.toml");
const BAND_PROFILE: &str = "robinhood-eth-band-200-v1";
const BAND_TEMPLATE: &str = include_str!("../../config/eth-band-paper.toml");

fn binding(c: &Config) -> Result<Value> {
    super::transport_independent_fingerprint(&serde_json::to_string(&json!({
        "mode":c.mode, "liquidity":c.liquidity, "strategy":c.strategy,
        "hyperliquid":c.hyperliquid
    }))?)
}

struct Plan {
    band: bool,
    exit: Config,
    target: Config,
    marker: Value,
}

#[cfg(test)]
fn plan(path: &Path, current: &Config, store: &Store) -> Result<Plan> {
    plan_for(path, current, store, false)
}
fn plan_for(path: &Path, current: &Config, store: &Store, band: bool) -> Result<Plan> {
    let profile = if band { BAND_PROFILE } else { PROFILE };
    let template: Config = toml::from_str(if band { BAND_TEMPLATE } else { TEMPLATE })?;
    ensure!(
        current.mode == Mode::Live
            && current.liquidity.chain_id == template.liquidity.chain_id
            && current
                .liquidity
                .pool
                .eq_ignore_ascii_case(&template.liquidity.pool)
            && current.hyperliquid.mainnet
            && current.hyperliquid.hedge_coin == "ETH",
        "此切换命令仅支持 Robinhood WETH/USDG 池与主网 Hyperliquid ETH 实盘"
    );
    // RPC、主账户、API 钱包环境变量名和状态目录均沿用服务器配置。
    let mut target = current.clone();
    target.strategy = template.strategy;
    target.hyperliquid.leverage = 3;
    target.hyperliquid.cross_margin = true;
    target.validate()?;

    let config_path = path.canonicalize().context("定位待切换配置文件")?;
    let previous = store.read::<Value>(MARKER)?;
    if let Some(m) = &previous {
        ensure!(
            m["schema"] == 1
                && m["profile"] == profile
                && m["config_path"] == json!(config_path)
                && m["target_binding"] == binding(&target)?,
            "存在另一项未完成迁移；保留记录并使用原配置继续"
        );
    }
    let saved = store
        .read::<String>("config.json")?
        .map(|s| super::transport_independent_fingerprint(&s))
        .transpose()?;
    let reset = store.read::<Value>("manual_reset.json")?;
    // 清理过程中可能已经删除 config.json，此时维护标记仍保留原绑定。
    let old = saved
        .as_ref()
        .or_else(|| reset.as_ref().map(|v| &v["binding"]))
        .or_else(|| previous.as_ref().map(|v| &v["exit_binding"]));
    let mut exit = current.clone();
    if let Some(old) = old {
        exit.strategy = serde_json::from_value(old["strategy"].clone())
            .context("旧状态缺少可恢复的策略参数")?;
        exit.hyperliquid.cross_margin =
            serde_json::from_value(old["hyperliquid"]["cross_margin"].clone())?;
        exit.hyperliquid.leverage = serde_json::from_value(old["hyperliquid"]["leverage"].clone())?;
        // 只允许策略和保证金模式变化，不能顺便把另一个账户/链的记录当作本账户处理。
        ensure!(
            binding(&exit)? == *old,
            "旧状态与当前账户/链/交易参数不一致；未发送交易，不能自动迁移"
        );
        if let Some(m) = &previous {
            ensure!(m["exit_binding"] == *old, "迁移与旧状态绑定不一致");
        }
        if let Some(m) = &reset {
            ensure!(m["binding"] == *old, "退出流程与旧状态绑定不一致");
        }
    } else if Path::new(&current.state_dir).exists() {
        // 没有任何绑定时只能是新建目录，拒绝猜测残存仓位台账的来源。
        for entry in fs::read_dir(&current.state_dir)? {
            ensure!(
                entry?.file_name() == "process.lock",
                "状态目录有记录但缺少配置绑定；请恢复备份后对账，不要删除交易记录"
            );
        }
    }
    exit.validate()?;
    let marker = json!({"schema":1,"profile":profile,"config_path":config_path,
        "exit_binding":binding(&exit)?,"target_binding":binding(&target)?});
    Ok(Plan {
        band,
        exit,
        target,
        marker,
    })
}

fn summary(c: &Config) -> Value {
    if let Some(band) = crate::strategy::eth::band::config(&c.strategy) {
        return json!({"profile":BAND_PROFILE,"state_dir":c.state_dir,"total_capital_usd":200,"lp_budget_usd":120,"hedge_collateral_usd":60,"reserve_usd":20,
            "band":band,"hyperliquid_leverage":3,"cross_margin":true,"hard_drawdown_halt":0.05,
            "research_warning":"baseline 5.59%; failed cost stress; live outcomes are not guaranteed"});
    }
    json!({"profile":PROFILE,"state_dir":c.state_dir,"total_capital_usd":200,
        "lp_budget_usd":120,"hedge_collateral_usd":60,"reserve_usd":20,
        "lp_range":"P/10 .. 10P","inventory_short_ratio":0.5,
        "hyperliquid_leverage":3,"cross_margin":true,"drawdown_halt":0.05,
        "first_entry":"skip waiting only; risk, data and collateral checks still required"})
}

pub async fn request(
    path: &Path,
    current: Config,
    store: Arc<Store>,
    execute: bool,
) -> Result<Value> {
    request_for(path, current, store, execute, false).await
}
pub async fn request_band(
    path: &Path,
    current: Config,
    store: Arc<Store>,
    execute: bool,
) -> Result<Value> {
    request_for(path, current, store, execute, true).await
}
async fn request_for(
    path: &Path,
    current: Config,
    store: Arc<Store>,
    execute: bool,
    band: bool,
) -> Result<Value> {
    let p = plan_for(path, &current, &store, band)?;
    if !execute {
        return Ok(json!({"status":"preview_only","target":summary(&p.target),
            "steps":["reconcile old state","cancel ETH orders","remove this pool LP and collect fees",
                "sell wallet WETH to USDG; retain native gas","close ETH hedge","verify flat twice",
                "archive and clear old state","install new configuration"],
            "note":"execute requires exclusive state lock and both signing keys; no transactions sent by preview"}));
    }
    execute_plan(path, p, store, |c, s| super::reset::request(c, s, true)).await
}

fn read_regular(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "配置文件必须为普通文件，不能是符号链接"
    );
    Ok(fs::read(path)?)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

async fn execute_plan<F, Fut>(path: &Path, p: Plan, store: Arc<Store>, exit: F) -> Result<Value>
where
    F: FnOnce(Config, Arc<Store>) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    ensure!(store.is_writable(), "切换策略需要独占状态锁");
    let original = read_regular(path)?;
    let disk: Config = toml::from_str(std::str::from_utf8(&original)?)?;
    let latest = plan_for(path, &disk, &store, p.band)?;
    ensure!(
        latest.marker == p.marker
            && serde_json::to_value(&latest.target)? == serde_json::to_value(&p.target)?,
        "读取配置后文件发生变化；未执行平仓，请重新运行命令"
    );
    let content = format!(
        "# Robinhood ETH 策略；由显式迁移命令在确认空仓后安装。\n{}",
        toml::to_string_pretty(&p.target)?
    );
    let path = path.canonicalize()?;
    let parent = path.parent().context("配置父目录")?;
    // 提前测试同目录写入/同步能力；配置替换采用原子 rename，不产生半份 TOML。
    let staged = parent.join(format!(
        ".lp-maker-switch-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    write_new(&staged, content.as_bytes())?;
    let result = async {
        let root = Path::new(&p.exit.state_dir).canonicalize()?;
        let backup = root.parent().context("状态父目录")?.join("operator-logs")
            .join(format!("strategy-switch-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&backup)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o700))?;
        }
        write_new(&backup.join("config-before.toml"), &original)?;
        write_new(&backup.join("exit-config.toml"), toml::to_string_pretty(&p.exit)?.as_bytes())?;
        fs::File::open(&backup)?.sync_all()?;
        fs::File::open(backup.parent().context("备份父目录")?)?.sync_all()?;
        store.write(MARKER, &p.marker)?;
        tracing::info!(backup=%backup.display(), "开始切换策略；先按已保存的旧参数退出，全部确认后才安装新参数");
        let flat = exit(p.exit, store.clone()).await?;
        ensure!(flat["status"] == "flat_and_reset" || flat["status"] == "flat_with_base_dust_and_reset",
            "退出结果尚未确认空仓；不更新策略");
        for entry in fs::read_dir(&root)? {
            let name = entry?.file_name();
            ensure!(name == "process.lock" || name == MARKER,
                "旧状态清理尚未完成；保留迁移标记，不更新策略");
        }
        ensure!(store.read::<Value>(MARKER)?.as_ref() == Some(&p.marker),
            "迁移标记缺失或发生变化；不更新策略");
        ensure!(read_regular(&path)? == original,
            "平仓期间配置文件被修改；已保留空仓备份及迁移标记，请核对后重新执行切换命令");
        fs::rename(&staged, &path)?;
        fs::File::open(parent)?.sync_all()?;
        // 配置已经持久化，才解除策略启动禁令。仍持有原来的 process.lock。
        fs::remove_file(root.join(MARKER))?;
        fs::File::open(&root)?.sync_all()?;
        tracing::info!("旧仓位已退出、旧状态已归档，新策略配置已安装；可按第一仓规则启动");
        Ok(json!({"status":"switched_flat","config":path,"config_backup":backup,
            "exit_result":flat,"target":summary(&p.target)}))
    }.await;
    if staged.exists() {
        let _ = fs::remove_file(staged);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn setup() -> (tempfile::TempDir, PathBuf, Config, Arc<Store>) {
        let d = tempfile::tempdir().unwrap();
        let mut c = Config::load("config/robinhood.toml").unwrap();
        c.mode = Mode::Live;
        c.hyperliquid.account = Some("0x1111111111111111111111111111111111111111".into());
        c.state_dir = d.path().join("state").to_string_lossy().into();
        let path = d.path().join("local.toml");
        fs::write(&path, toml::to_string_pretty(&c).unwrap()).unwrap();
        let s = Arc::new(Store::open(&c.state_dir).unwrap());
        s.write(
            "config.json",
            &serde_json::to_string(&json!({"mode":c.mode,
            "liquidity":c.liquidity,"hyperliquid":c.hyperliquid,"strategy":c.strategy}))
            .unwrap(),
        )
        .unwrap();
        (d, path, c, s)
    }

    fn fake_clear(c: &Config, s: &Store) {
        assert!(s.read::<Value>(MARKER).unwrap().is_some());
        for e in fs::read_dir(&c.state_dir).unwrap() {
            let e = e.unwrap();
            if e.file_name() != "process.lock" && e.file_name() != MARKER {
                fs::remove_file(e.path()).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn bounded_switch_exits_old_profile_and_installs_only_reviewed_budget() {
        let (_d, path, c, store) = setup();
        let p = plan_for(&path, &c, &store, true).unwrap();
        assert_eq!(p.marker["profile"], BAND_PROFILE);
        assert!(p.exit.strategy.eth_persistent.is_none());
        assert!(crate::strategy::eth::band::config(&p.target.strategy).is_some());
        let original = fs::read(&path).unwrap();
        let seen = path.clone();
        execute_plan(&path, p, store.clone(), |old, s| async move {
            assert_eq!(fs::read(&seen)?, original);
            assert!(crate::strategy::eth::band::config(&old.strategy).is_none());
            fake_clear(&old, &s);
            Ok(json!({"status":"flat_and_reset"}))
        })
        .await
        .unwrap();
        let installed = Config::load(&path).unwrap();
        assert_eq!(installed.strategy.hedge_deadband_usd, 15.);
        assert_eq!(installed.strategy.cooldown_hours, 3);
        assert_eq!(installed.strategy.resume_healthy_hours, 4);
        assert_eq!(
            installed
                .strategy
                .eth_persistent
                .as_ref()
                .unwrap()
                .hedge_interval_hours,
            2
        );
        assert_eq!(
            crate::strategy::eth::band::config(&installed.strategy)
                .unwrap()
                .upper_width,
            0.08
        );
        assert_eq!(fs::read_dir(&c.state_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn bounded_failed_exit_retains_records_and_does_not_allow_other_migration() {
        let (_d, path, c, store) = setup();
        let original = fs::read(&path).unwrap();
        let p = plan_for(&path, &c, &store, true).unwrap();
        assert!(
            execute_plan(&path, p, store.clone(), |_, s| async move {
                s.write("pending.json", &json!({"hash":"uncertain"}))?;
                anyhow::bail!("partial exchange fill")
            })
            .await
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(store.pending().unwrap().is_some());
        assert!(plan_for(&path, &c, &store, true).is_ok());
        assert!(plan_for(&path, &c, &store, false).is_err());
    }

    #[test]
    fn recover_old_economics_but_preserve_endpoints_and_identity() {
        let (_d, path, c, s) = setup();
        let mut changed = plan(&path, &c, &s).unwrap().target;
        changed.liquidity.rpc_url = "https://new.example".into();
        changed.hyperliquid.http_url = "https://new-hl.example".into();
        let p = plan(&path, &changed, &s).unwrap();
        assert!(p.exit.strategy.eth_persistent.is_none());
        assert_eq!(p.exit.hyperliquid.cross_margin, c.hyperliquid.cross_margin);
        assert_eq!(p.exit.liquidity.rpc_url, changed.liquidity.rpc_url);
        assert_eq!(p.target.hyperliquid.account, c.hyperliquid.account);
        assert_eq!(p.target.state_dir, c.state_dir);
        assert!(p.target.hyperliquid.cross_margin);
        assert_eq!(p.target.strategy.lp_budget, 120.0);
        changed.hyperliquid.account = Some("0x2222222222222222222222222222222222222222".into());
        assert!(plan(&path, &changed, &s).is_err());
    }

    #[tokio::test]
    async fn failed_exit_retains_old_config_pending_and_startup_guard() {
        let (_d, path, c, s) = setup();
        let original = fs::read(&path).unwrap();
        let p = plan(&path, &c, &s).unwrap();
        let result = execute_plan(&path, p, s.clone(), |_, s| async move {
            s.write("pending.json", &json!({"hash":"unknown"}))?;
            anyhow::bail!("uncertain receipt")
        })
        .await;
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(s.pending().unwrap().is_some());
        assert!(s.read::<Value>(MARKER).unwrap().is_some());
        let error = super::super::run(c, s, true, true, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("迁移"));
    }

    #[tokio::test]
    async fn successful_switch_keeps_lock_and_installs_only_after_exit() {
        let (_d, path, c, s) = setup();
        let p = plan(&path, &c, &s).unwrap();
        let seen = path.clone();
        execute_plan(&path, p, s.clone(), |old, s| async move {
            assert!(Config::load(&seen)?.strategy.eth_persistent.is_none());
            assert!(old.strategy.eth_persistent.is_none());
            assert!(Store::open(&old.state_dir).is_err());
            fake_clear(&old, &s);
            Ok(json!({"status":"flat_and_reset"}))
        })
        .await
        .unwrap();
        let new = Config::load(&path).unwrap();
        assert!(new.strategy.eth_persistent.is_some());
        assert_eq!(new.strategy.total_capital, 200.0);
        assert_eq!(new.strategy.max_drawdown, 0.05);
        assert!(new.hyperliquid.cross_margin);
        assert!(s.read::<Value>(MARKER).unwrap().is_none());
        assert!(Store::open(&c.state_dir).is_err());
        assert_eq!(fs::read_dir(&c.state_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn concurrent_config_edit_blocks_commit_and_retry_uses_saved_binding() {
        let (_d, path, c, s) = setup();
        let p = plan(&path, &c, &s).unwrap();
        let edited = path.clone();
        let result = execute_plan(&path, p, s.clone(), |old, s| async move {
            fake_clear(&old, &s);
            let text = fs::read_to_string(&edited)? + "\n# operator edit\n";
            fs::write(&edited, text)?;
            Ok(json!({"status":"flat_and_reset"}))
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("被修改"));
        assert!(fs::read_to_string(&path).unwrap().contains("operator edit"));
        assert!(s.read::<Value>(MARKER).unwrap().is_some());
        let p = plan(&path, &Config::load(&path).unwrap(), &s).unwrap();
        execute_plan(&path, p, s.clone(), |old, s| async move {
            fake_clear(&old, &s);
            Ok(json!({"status":"flat_with_base_dust_and_reset"}))
        })
        .await
        .unwrap();
        assert!(
            Config::load(&path)
                .unwrap()
                .strategy
                .eth_persistent
                .is_some()
        );
    }

    #[tokio::test]
    async fn preview_is_read_only_and_bindingless_records_are_rejected() {
        let (_d, path, c, s) = setup();
        let original = fs::read(&path).unwrap();
        let preview = request(
            &path,
            c.clone(),
            Arc::new(Store::readonly(&c.state_dir)),
            false,
        )
        .await
        .unwrap();
        assert_eq!(preview["status"], "preview_only");
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(s.read::<Value>(MARKER).unwrap().is_none());
        fs::remove_file(Path::new(&c.state_dir).join("config.json")).unwrap();
        s.write("checkpoint.json", &json!({"positions":["existing"]}))
            .unwrap();
        assert!(plan(&path, &c, &s).is_err());
    }

    #[test]
    fn partial_reset_can_recover_binding_without_config_file() {
        let (_d, path, c, s) = setup();
        s.write(
            "manual_reset.json",
            &json!({"binding":binding(&c).unwrap(),"stage":"verified_flat"}),
        )
        .unwrap();
        fs::remove_file(Path::new(&c.state_dir).join("config.json")).unwrap();
        let target = plan(&path, &c, &s).unwrap().target;
        assert!(
            plan(&path, &target, &s)
                .unwrap()
                .exit
                .strategy
                .eth_persistent
                .is_none()
        );
    }
}

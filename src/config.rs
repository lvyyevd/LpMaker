use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Paper,
    Live,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub state_dir: String,
    pub poll_seconds: u64,
    pub hyperliquid: HyperliquidConfig,
    pub liquidity: LiquidityConfig,
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub websocket: WebSocketConfig,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidConfig {
    pub http_url: String,
    pub ws_url: String,
    pub mainnet: bool,
    pub coins: Vec<String>,
    pub hedge_coin: String,
    pub private_key_env: String,
    pub account: Option<String>,
    pub vault: Option<String>,
    pub leverage: u32,
    pub cross_margin: bool,
    pub maker_wait_seconds: u64,
    pub emergency_slippage_bps: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiquidityConfig {
    pub kind: String,
    pub chain_id: u64,
    pub rpc_url: String,
    /// Historical reads only; transaction submission always uses rpc_url.
    pub archive_rpc_url: Option<String>,
    /// 同一进程内，同地址的所有 EVM 请求共享间隔；旧配置默认最多约 4 次/秒。
    #[serde(default = "default_rpc_interval")]
    pub rpc_min_interval_ms: u64,
    #[serde(default = "default_evm_ws")]
    pub ws_url: String,
    /// Public LP wallet address for monitoring without loading a private key.
    pub owner: Option<String>,
    #[serde(default = "default_nonce_refresh")]
    pub nonce_refresh_seconds: u64,
    #[serde(default = "default_tx_stale")]
    pub pending_warn_seconds: u64,
    pub pool: String,
    pub factory: String,
    pub position_manager: String,
    pub swap_router: String,
    pub base_token: String,
    pub quote_token: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub fee: u32,
    pub private_key_env: String,
    pub confirmations: u64,
    pub slippage_bps: u32,
    pub deadline_seconds: u64,
    pub max_gas_native: f64,
    /// Extra fee-cap headroom; 10_000 bps means 100% above the estimate.
    #[serde(default = "default_gas_fee_buffer")]
    pub gas_fee_buffer_bps: u32,
    /// Extra gas units; 2_000 bps means 20% above eth_estimateGas.
    #[serde(default = "default_gas_limit_buffer")]
    pub gas_limit_buffer_bps: u32,
    #[serde(default = "default_quote_age")]
    pub max_quote_age_seconds: u64,
}
fn default_gas_fee_buffer() -> u32 {
    10_000
}
fn default_rpc_interval() -> u64 {
    250
}
fn default_gas_limit_buffer() -> u32 {
    2_000
}
fn default_quote_age() -> u64 {
    60
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub read_retry_seconds: u64,
    pub observation_timeout_seconds: u64,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            read_retry_seconds: 5,
            observation_timeout_seconds: 45,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub event_segment_bytes: u64,
    pub event_retained_segments: usize,
    pub terminal_orders_keep: usize,
    pub order_archive_max_bytes: u64,
    pub min_free_bytes: u64,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            event_segment_bytes: 16 * 1024 * 1024,
            event_retained_segments: 32,
            terminal_orders_keep: 1000,
            order_archive_max_bytes: 256 * 1024 * 1024,
            min_free_bytes: 256 * 1024 * 1024,
        }
    }
}
fn default_evm_ws() -> String {
    "wss://robinhood-rpc.publicnode.com".into()
}
fn default_nonce_refresh() -> u64 {
    30
}
fn default_tx_stale() -> u64 {
    120
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSocketConfig {
    pub heartbeat_seconds: u64,
    pub heartbeat_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub write_timeout_seconds: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_seconds: u64,
}
impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            heartbeat_seconds: 20,
            heartbeat_timeout_seconds: 60,
            idle_timeout_seconds: 300,
            connect_timeout_seconds: 15,
            write_timeout_seconds: 10,
            reconnect_initial_ms: 1000,
            reconnect_max_seconds: 30,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitoringConfig {
    /// Human-readable status printing only; independent of observation refreshes.
    pub hyperliquid_interval_seconds: u64,
    // 旧字段名继续序列化，避免旧配置/工具失效；新链可使用不带链名的别名。
    #[serde(alias = "liquidity_interval_seconds")]
    pub robinhood_interval_seconds: u64,
    pub hyperliquid_refresh_seconds: u64,
    #[serde(alias = "liquidity_refresh_seconds")]
    pub robinhood_refresh_seconds: u64,
    /// 已停用成交量统计；保留旧字段只为兼容已有配置，运行器不会启动补数。
    pub volume_refresh_seconds: u64,
    pub volume_window_seconds: u64,
    pub backfill_blocks: u64,
    pub refresh_timeout_seconds: u64,
}
impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            hyperliquid_interval_seconds: 60,
            robinhood_interval_seconds: 60,
            hyperliquid_refresh_seconds: 30,
            robinhood_refresh_seconds: 15,
            volume_refresh_seconds: 60,
            volume_window_seconds: 300,
            backfill_blocks: 6000,
            refresh_timeout_seconds: 45,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub directory: String,
    pub retained_files: usize,
    pub level: String,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            directory: "data/logs".into(),
            retained_files: 14,
            level: "info".into(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyConfig {
    /// 显式选择 ETH 长持 LP 策略；缺省时保留旧策略和配置指纹。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eth_persistent: Option<crate::strategy::eth::PersistentConfig>,
    pub total_capital: f64,
    pub lp_budget: f64,
    pub hedge_collateral: f64,
    pub reserve: f64,
    pub inside_hedge_ratio: f64,
    pub hedge_deadband_usd: f64,
    pub max_drawdown: f64,
    pub max_basis_bps: f64,
    pub max_data_age_seconds: u64,
    pub fast_drop_1h: f64,
    pub vol_short_hours: usize,
    pub vol_long_hours: usize,
    pub vol_pause_ratio: f64,
    pub vol_resume_ratio: f64,
    pub ema_fast_hours: usize,
    pub ema_slow_hours: usize,
    pub resume_healthy_hours: u32,
    pub cooldown_hours: u64,
    pub recovery_fraction: f64,
    pub recovery_step_hours: u64,
    pub breakout_buffer: f64,
    pub breakout_confirm_bars: u32,
    pub decision_bar_seconds: u64,
    pub hedge_release_buffer: f64,
    pub layers: Vec<LayerConfig>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayerConfig {
    pub name: String,
    pub weight: f64,
    pub half_width: f64,
}
impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let c: Self = toml::from_str(&std::fs::read_to_string(path).context("read config")?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=40_000).contains(&self.liquidity.gas_fee_buffer_bps)
                && (1..=10_000).contains(&self.liquidity.gas_limit_buffer_bps),
            "gas fee buffer must be 1..=40000 bps; gas limit buffer must be 1..=10000 bps"
        );
        ensure!(
            self.runtime.read_retry_seconds > 0
                && self.runtime.read_retry_seconds <= 300
                && self.runtime.observation_timeout_seconds > 0
                && self.runtime.observation_timeout_seconds <= self.strategy.max_data_age_seconds
                && self.liquidity.max_quote_age_seconds > 0,
            "invalid observation/retry limits"
        );
        ensure!(
            self.storage.event_segment_bytes >= 1024
                && self.storage.event_retained_segments > 0
                && self.storage.terminal_orders_keep > 0
                && self.storage.order_archive_max_bytes >= 1024,
            "invalid storage retention limits"
        );
        validate_url(&self.hyperliquid.http_url, &["https", "http"])?;
        validate_url(&self.hyperliquid.ws_url, &["wss", "ws"])?;
        validate_url(&self.liquidity.rpc_url, &["https", "http", "wss", "ws"])?;
        ensure!(
            (1..=5_000).contains(&self.liquidity.rpc_min_interval_ms),
            "rpc_min_interval_ms must be 1..=5000"
        );
        validate_url(&self.liquidity.ws_url, &["wss", "ws"])?;
        if let Some(url) = &self.liquidity.archive_rpc_url {
            validate_url(url, &["http", "https", "ws", "wss"])?;
        }
        let w = &self.websocket;
        ensure!(
            w.heartbeat_seconds > 0
                && w.heartbeat_timeout_seconds > w.heartbeat_seconds
                && w.idle_timeout_seconds > w.heartbeat_seconds
                && w.connect_timeout_seconds > 0
                && w.write_timeout_seconds > 0
                && w.reconnect_initial_ms > 0
                && w.reconnect_initial_ms <= w.reconnect_max_seconds.saturating_mul(1000),
            "invalid websocket lifecycle settings"
        );
        ensure!(
            self.monitoring.hyperliquid_interval_seconds > 0
                && self.monitoring.robinhood_interval_seconds > 0
                && self.monitoring.hyperliquid_refresh_seconds > 0
                && self.monitoring.robinhood_refresh_seconds > 0
                && self.monitoring.refresh_timeout_seconds > 0
                && self.liquidity.nonce_refresh_seconds > 0
                && self.liquidity.pending_warn_seconds > 0
                && self.logging.retained_files > 0,
            "invalid monitoring/lifecycle settings"
        );
        for a in [
            &self.hyperliquid.account,
            &self.hyperliquid.vault,
            &self.liquidity.owner,
        ]
        .into_iter()
        .flatten()
        {
            let _: alloy::primitives::Address = a.parse().context("invalid account address")?;
        }
        let s = &self.strategy;
        for v in [
            s.total_capital,
            s.lp_budget,
            s.hedge_collateral,
            s.hedge_deadband_usd,
            s.max_basis_bps,
            self.liquidity.max_gas_native,
        ] {
            ensure!(
                v.is_finite() && v > 0.0,
                "positive finite budget/limit required"
            );
        }
        ensure!(s.reserve.is_finite() && s.reserve >= 0.0, "invalid reserve");
        ensure!(
            s.lp_budget + s.hedge_collateral + s.reserve <= s.total_capital + 1e-6,
            "budgets exceed total capital"
        );
        for v in [s.max_drawdown, s.fast_drop_1h, s.recovery_fraction] {
            ensure!(
                v.is_finite() && v > 0.0 && v <= 1.0,
                "invalid fractional limit"
            );
        }
        ensure!(
            (0.0..=1.0).contains(&s.inside_hedge_ratio),
            "invalid hedge ratio"
        );
        ensure!(
            s.vol_short_hours >= 2
                && s.vol_long_hours > s.vol_short_hours
                && s.vol_long_hours < 4000,
            "invalid volatility windows"
        );
        ensure!(
            s.ema_fast_hours > 1 && s.ema_slow_hours > s.ema_fast_hours && s.ema_slow_hours < 4000,
            "invalid EMA windows"
        );
        ensure!(
            s.vol_resume_ratio > 0.0
                && s.vol_pause_ratio > s.vol_resume_ratio
                && s.vol_pause_ratio.is_finite(),
            "invalid volatility hysteresis"
        );
        ensure!(
            s.breakout_buffer.is_finite() && (0.0..0.2).contains(&s.breakout_buffer),
            "invalid breakout buffer"
        );
        ensure!(
            s.hedge_release_buffer.is_finite() && (0.0..0.2).contains(&s.hedge_release_buffer),
            "invalid hedge release"
        );
        ensure!(
            s.resume_healthy_hours > 0
                && s.recovery_step_hours > 0
                && s.breakout_confirm_bars > 0
                && s.decision_bar_seconds > 0,
            "zero confirmation period"
        );
        ensure!(
            self.poll_seconds > 0 && s.max_data_age_seconds > self.poll_seconds,
            "invalid poll/stale interval"
        );
        ensure!(
            !s.layers.is_empty()
                && s.layers.iter().all(|x| x.weight.is_finite()
                    && x.weight > 0.0
                    && x.half_width.is_finite()
                    && x.half_width > 0.001
                    && (x.half_width < 1.0 || (s.eth_persistent.is_some() && x.half_width <= 9.0))),
            "invalid layers"
        );
        ensure!(
            (s.layers.iter().map(|x| x.weight).sum::<f64>() - 1.0).abs() < 1e-8,
            "layer weights must sum to one"
        );
        let mut names = std::collections::HashSet::new();
        ensure!(
            s.layers.iter().all(|l| names.insert(&l.name)),
            "duplicate layer name"
        );
        ensure!(
            self.hyperliquid.leverage > 0 && self.hyperliquid.leverage <= 3,
            "strategy leverage capped at 3x"
        );
        if let Some(profile) = &s.eth_persistent {
            profile.validate(self)?;
        }
        ensure!(
            self.hyperliquid.maker_wait_seconds > 0,
            "maker wait must be positive"
        );
        ensure!(
            self.hyperliquid.emergency_slippage_bps > 0
                && self.hyperliquid.emergency_slippage_bps <= 100
                && self.liquidity.slippage_bps > 0
                && self.liquidity.slippage_bps <= 100,
            "slippage must be in 1..100 bps"
        );
        ensure!(
            self.liquidity.kind == "uniswap_v3",
            "unsupported liquidity adapter"
        );
        ensure!(
            self.liquidity.confirmations > 0 && self.liquidity.deadline_seconds >= 30,
            "invalid chain finality/deadline"
        );
        for a in [
            &self.liquidity.pool,
            &self.liquidity.factory,
            &self.liquidity.position_manager,
            &self.liquidity.swap_router,
            &self.liquidity.base_token,
            &self.liquidity.quote_token,
        ] {
            let _: alloy::primitives::Address = a.parse().context("invalid contract address")?;
        }
        ensure!(
            self.liquidity.base_token.to_lowercase() != self.liquidity.quote_token.to_lowercase(),
            "same pool tokens"
        );
        ensure!(
            self.liquidity.base_decimals <= 24
                && self.liquidity.quote_decimals <= 24
                && self.liquidity.fee < 1_000_000,
            "invalid token configuration"
        );
        if self.mode == Mode::Live {
            ensure!(
                self.hyperliquid.account.is_some(),
                "live requires Hyperliquid account owner address"
            );
        }
        crate::liquidity::chains::validate_pool(&self.liquidity)?;
        if let Some(pool) = crate::liquidity::chains::pool(&self.liquidity) {
            ensure!(
                self.hyperliquid.hedge_coin == pool.hedge_coin,
                "pool base asset must match Hyperliquid hedge coin"
            );
        }
        Ok(())
    }
}

pub fn validate_url(url: &str, schemes: &[&str]) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid endpoint URL")?;
    ensure!(
        schemes.contains(&parsed.scheme()) && parsed.host_str().is_some(),
        "unsupported endpoint protocol"
    );
    Ok(())
}

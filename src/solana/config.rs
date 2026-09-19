use crate::config::{HyperliquidConfig, LoggingConfig, Mode, StrategyConfig, WebSocketConfig};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub state_dir: String,
    pub poll_seconds: u64,
    pub report_seconds: u64,
    pub adapter_path: String,
    pub solana: SolanaConfig,
    pub hyperliquid: HyperliquidConfig,
    pub strategy: StrategyConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime: Option<super::regime::Config>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub websocket: WebSocketConfig,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SolanaConfig {
    pub pool: String,
    pub owner: Option<String>,
    pub rpc_url_env: String,
    pub grpc_url_env: String,
    pub grpc_token_env: String,
    pub private_key_env: String,
    pub require_grpc: bool,
    pub slippage_bps: u32,
    pub max_transaction_fee_sol: f64,
    pub priority_fee_microlamports: u64,
    pub min_native_sol: f64,
    pub max_rent_sol: f64,
    pub idle_timeout_seconds: u64,
}
impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let c: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(regime) = &self.regime {
            regime.validate()?;
        }
        let s = &self.strategy;
        ensure!(
            self.solana.pool == super::POOL && self.hyperliquid.hedge_coin == "SOL",
            "unsupported Solana pool/hedge pair"
        );
        ensure!(
            self.hyperliquid.coins.iter().any(|c| c == "SOL"),
            "SOL subscription required"
        );
        ensure!(
            self.poll_seconds > 0
                && s.max_data_age_seconds > self.poll_seconds
                && self.report_seconds > 0,
            "invalid intervals"
        );
        ensure!(
            (1..=3).contains(&self.hyperliquid.leverage) && !self.hyperliquid.cross_margin,
            "use 1..3x isolated SOL hedge"
        );
        ensure!(
            s.lp_budget / f64::from(self.hyperliquid.leverage) <= s.hedge_collateral * 0.9,
            "full LP inventory exceeds configured hedge margin capacity"
        );
        ensure!(
            self.hyperliquid.maker_wait_seconds > 0
                && (1..=100).contains(&self.hyperliquid.emergency_slippage_bps),
            "invalid hedge execution limits"
        );
        for x in [
            s.total_capital,
            s.lp_budget,
            s.hedge_collateral,
            s.hedge_deadband_usd,
            s.max_basis_bps,
            self.solana.max_transaction_fee_sol,
            self.solana.min_native_sol,
            self.solana.max_rent_sol,
        ] {
            ensure!(x.is_finite() && x > 0., "positive finite budget required");
        }
        ensure!(
            s.reserve.is_finite()
                && s.reserve >= 0.
                && s.lp_budget + s.hedge_collateral + s.reserve <= s.total_capital + 1e-6,
            "invalid capital allocation"
        );
        for x in [s.max_drawdown, s.fast_drop_1h, s.recovery_fraction] {
            ensure!(x.is_finite() && x > 0. && x <= 1., "invalid risk fraction");
        }
        ensure!(
            (0.0..=1.0).contains(&s.inside_hedge_ratio),
            "invalid hedge ratio"
        );
        ensure!(
            s.vol_short_hours >= 2
                && s.vol_long_hours > s.vol_short_hours
                && s.vol_long_hours < 4000
                && s.ema_fast_hours > 1
                && s.ema_slow_hours > s.ema_fast_hours
                && s.ema_slow_hours < 4000,
            "invalid indicator windows"
        );
        ensure!(
            s.vol_resume_ratio > 0.
                && s.vol_pause_ratio > s.vol_resume_ratio
                && s.vol_pause_ratio.is_finite(),
            "invalid volatility limits"
        );
        ensure!(
            (0.0..0.2).contains(&s.breakout_buffer)
                && (0.0..0.2).contains(&s.hedge_release_buffer)
                && s.breakout_confirm_bars > 0
                && s.decision_bar_seconds > 0
                && s.resume_healthy_hours > 0
                && s.cooldown_hours > 0
                && s.recovery_step_hours > 0,
            "invalid recovery/breakout limits"
        );
        let mut names = std::collections::BTreeSet::new();
        ensure!(
            !s.layers.is_empty()
                && (s.layers.iter().map(|l| l.weight).sum::<f64>() - 1.).abs() < 1e-8
                && s.layers.iter().all(|l| names.insert(&l.name)
                    && l.weight.is_finite()
                    && l.weight > 0.
                    && l.half_width.is_finite()
                    && l.half_width > 0.001
                    && l.half_width < 0.25),
            "invalid DLMM layers"
        );
        ensure!(
            (1..=100).contains(&self.solana.slippage_bps)
                && self.solana.idle_timeout_seconds >= 60
                && self.solana.priority_fee_microlamports <= 1_000_000,
            "invalid Solana limits"
        );
        for name in [
            &self.solana.rpc_url_env,
            &self.solana.grpc_url_env,
            &self.solana.grpc_token_env,
            &self.solana.private_key_env,
        ] {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "use environment variable names, not secrets"
            );
        }
        crate::config::validate_url(&self.hyperliquid.http_url, &["https", "http"])?;
        crate::config::validate_url(&self.hyperliquid.ws_url, &["wss", "ws"])?;
        for a in [&self.hyperliquid.account, &self.hyperliquid.vault]
            .into_iter()
            .flatten()
        {
            let _: alloy::primitives::Address = a.parse()?;
        }
        if self.mode == Mode::Live {
            ensure!(
                self.solana.owner.is_some()
                    && self.hyperliquid.account.is_some()
                    && self.solana.require_grpc,
                "live needs dedicated owners and gRPC"
            );
        }
        Ok(())
    }
    // 端点、token 环境变量名与日志参数不改变经济身份；不读取、更不保存凭据。
    pub fn fingerprint(&self) -> serde_json::Value {
        let mut value = serde_json::json!({"schema":1,"protocol":"solana_meteora_dlmm","mode":self.mode,"pool":self.solana.pool,"owner":self.solana.owner,"strategy":self.strategy,"hedge":{"mainnet":self.hyperliquid.mainnet,"account":self.hyperliquid.account,"vault":self.hyperliquid.vault,"coin":self.hyperliquid.hedge_coin,"leverage":self.hyperliquid.leverage,"cross":self.hyperliquid.cross_margin}});
        // None must produce the exact old fingerprint, so existing SOL checkpoints still restart.
        if let Some(regime) = &self.regime {
            value["regime"] = serde_json::json!(regime);
        }
        value
    }
}

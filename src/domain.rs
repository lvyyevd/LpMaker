use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub open_ms: u64,
    pub close_ms: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub block: u64,
    pub block_hash: String,
    pub time_ms: u64,
    pub price: f64,
    pub tick: i32,
    pub tick_spacing: i32,
    pub liquidity: String,
    pub sqrt_price_x96: String,
    pub base_is_token0: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LpPosition {
    pub layer: String,
    pub token_id: Option<String>,
    pub lower: f64,
    pub upper: f64,
    pub liquidity: f64,
    pub raw_liquidity: String,
    #[serde(default)]
    pub unclaimed_base: f64,
    #[serde(default)]
    pub unclaimed_quote: f64,
    pub base: f64,
    pub quote: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Portfolio {
    pub positions: Vec<LpPosition>,
    pub wallet_base: f64,
    pub wallet_quote: f64,
    pub short_base: f64,
    pub hedge_equity: f64,
    pub reserve: f64,
}
impl Portfolio {
    pub fn base(&self) -> f64 {
        self.wallet_base + self.positions.iter().map(|p| p.base).sum::<f64>()
    }
    pub fn equity(&self, p: f64) -> f64 {
        self.base() * p
            + self.wallet_quote
            + self.positions.iter().map(|p| p.quote).sum::<f64>()
            + self.hedge_equity
            + self.reserve
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketFrame {
    pub now_ms: u64,
    pub pool: PoolSnapshot,
    pub hedge_price: f64,
    pub hedge_time_ms: u64,
    pub candles: Vec<Candle>,
    pub portfolio: Portfolio,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum LpIntent {
    Hold,
    ExitToQuote,
    Deploy { fraction: f64 },
    Recenter { layers: Vec<String> },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub state: String,
    pub reasons: Vec<String>,
    pub lp: LpIntent,
    pub target_short_base: f64,
    pub emergency: bool,
}

// Platform-neutral boundary: adapters own token ordering, ABI and transaction details.
#[async_trait]
pub trait LiquidityVenue: Send + Sync {
    async fn validate(&self) -> Result<()>;
    async fn snapshot(&self) -> Result<PoolSnapshot>;
    async fn positions(&self, owner: &str, ids: &[(String, String)]) -> Result<Vec<LpPosition>>;
}
#[allow(clippy::too_many_arguments)] // Mirrors the exchange order fields.
#[async_trait]
pub trait HedgeVenue: Send + Sync {
    async fn market(&self, coin: &str) -> Result<(f64, u64)>;
    async fn account(&self) -> Result<serde_json::Value>;
    async fn order(
        &self,
        coin: &str,
        buy: bool,
        price: &str,
        size: &str,
        tif: &str,
        reduce_only: bool,
        cloid: &str,
    ) -> Result<serde_json::Value>;
    async fn cancel(&self, coin: &str, cloid: &str) -> Result<serde_json::Value>;
}

/// Execution capability implemented by each LP platform. Strategy code never sees
/// an EVM signer, ABI, token ordering, router address, or NFT receipt.
#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    async fn refresh_execution_state(&self) -> Result<()> {
        Ok(())
    }
    async fn snapshot(&self) -> Result<PoolSnapshot>;
    async fn current_positions(&self) -> Result<Vec<LpPosition>>;
    async fn wallet_balances(&self) -> Result<(f64, f64)>;
    /// 建仓换币前询问协议实际需要的基础币；默认保留原来的等值配比。
    /// tick 间距较大的池可覆盖此方法，避免对齐后所需数量与 50/50 假设不同。
    fn mint_base_requirement(
        &self,
        value: f64,
        _width: f64,
        snapshot: &PoolSnapshot,
    ) -> Result<f64> {
        Ok(value / 2.0 / snapshot.price)
    }
    async fn mint_layer(&self, layer: &str, value: f64, width: f64) -> Result<()>;
    async fn increase_position(&self, position: &LpPosition, value: f64) -> Result<()>;
    async fn remove_position(&self, position: &LpPosition) -> Result<()>;
    async fn swap_inventory(&self, sell_base: bool, amount: f64) -> Result<()>;
}

//! Solana 与 EVM 的状态、签名、交易生命周期完全分开；只复用纯策略和 Hyperliquid。
pub mod backtest;
pub mod bridge;
pub mod config;
pub mod dlmm;
pub mod journal;
pub mod performance;
pub mod runner;
pub const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
pub mod regime;
pub mod research;

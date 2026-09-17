pub mod config;
pub mod domain;
pub mod engine;
pub mod evm;
pub mod hyperliquid;
pub mod logging;
pub mod math;
pub mod monitor;
pub mod recovery;
pub mod store;
pub mod strategy;
pub mod stream;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

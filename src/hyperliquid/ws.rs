pub use crate::stream::Event;
use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

pub fn subscriptions(coins: &[String], user: Option<&str>) -> Vec<Value> {
    let mut out = vec![json!({"type":"allMids"})];
    for c in coins {
        for t in ["l2Book", "trades", "activeAssetCtx"] {
            out.push(json!({"type":t,"coin":c}));
        }
        out.push(json!({"type":"candle","coin":c,"interval":"1h"}));
    }
    if let Some(u) = user {
        for t in [
            "orderUpdates",
            "userFills",
            "userFundings",
            "clearinghouseState",
            "openOrders",
        ] {
            out.push(json!({"type":t,"user":u}));
        }
    }
    out
}
pub async fn listen(
    url: &str,
    subscriptions: Vec<Value>,
    tx: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    listen_with_config(url, subscriptions, Default::default(), tx, shutdown).await
}
pub async fn listen_with_config(
    url: &str,
    subscriptions: Vec<Value>,
    config: crate::config::WebSocketConfig,
    tx: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    crate::stream::listen(
        url,
        crate::stream::Protocol::Hyperliquid,
        subscriptions,
        config,
        tx,
        shutdown,
    )
    .await
}

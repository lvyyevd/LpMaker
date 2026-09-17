//! Supervised WebSocket transport shared by Hyperliquid and EVM feeds.
use crate::config::WebSocketConfig;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub received_ms: u64,
    pub channel: String,
    pub data: Value,
}
#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Hyperliquid,
    Ethereum,
}
impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hyperliquid => "hyperliquid",
            Self::Ethereum => "evm",
        }
    }
}
async fn emit(tx: &mpsc::Sender<Event>, channel: &str, data: Value) -> Result<()> {
    // Overload must not stall heartbeat/shutdown. Reconnection requests reconciliation.
    tx.try_send(Event {
        received_ms: crate::now_ms(),
        channel: channel.into(),
        data,
    })
    .context("stream consumer closed or overloaded")
}
pub async fn listen(
    url: &str,
    protocol: Protocol,
    subscriptions: Vec<Value>,
    c: WebSocketConfig,
    tx: mpsc::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    crate::config::validate_url(url, &["ws", "wss"])?;
    let mut delay = c.reconnect_initial_ms;
    let mut generation = 0u64;
    loop {
        if *shutdown.borrow() || tx.is_closed() {
            return Ok(());
        }
        generation += 1;
        let started = Instant::now();
        tracing::info!(venue = protocol.name(), generation, "websocket connecting");
        let result = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            r = session(url, protocol, &subscriptions, &c, &tx, generation) => r,
        };
        if *shutdown.borrow() || tx.is_closed() {
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(c.heartbeat_timeout_seconds) {
            delay = c.reconnect_initial_ms;
        }
        let error = result
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "stream ended".into());
        tracing::warn!(venue=protocol.name(), generation, retry_ms=delay, %error, "websocket reconnect scheduled");
        let _ = emit(
            &tx,
            "disconnected",
            json!({"generation":generation,"retry_ms":delay,"error":error}),
        )
        .await;
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tx.closed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
        }
        delay = delay.saturating_mul(2).min(c.reconnect_max_seconds * 1000);
    }
}
async fn session(
    url: &str,
    protocol: Protocol,
    subscriptions: &[Value],
    c: &WebSocketConfig,
    tx: &mpsc::Sender<Event>,
    generation: u64,
) -> Result<()> {
    let (mut socket, _) = timeout(
        Duration::from_secs(c.connect_timeout_seconds),
        connect_async(url),
    )
    .await
    .context("websocket handshake timed out")??;
    let write_timeout = Duration::from_secs(c.write_timeout_seconds);
    for (i, s) in subscriptions.iter().enumerate() {
        let request = match protocol {
            Protocol::Hyperliquid => json!({"method":"subscribe","subscription":s}),
            Protocol::Ethereum => {
                json!({"jsonrpc":"2.0","id":i+1,"method":"eth_subscribe","params":s})
            }
        };
        timeout(
            write_timeout,
            socket.send(Message::Text(request.to_string().into())),
        )
        .await??;
    }
    emit(
        tx,
        "connected",
        json!({"generation":generation,"reconcile_required":true}),
    )
    .await?;
    tracing::info!(
        venue = protocol.name(),
        generation,
        subscriptions = subscriptions.len(),
        "websocket connected; subscriptions sent"
    );
    let mut ids = HashMap::<String, String>::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(c.heartbeat_seconds));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_frame = Instant::now();
    let mut last_data = Instant::now();
    loop {
        let frame_deadline = last_frame + Duration::from_secs(c.heartbeat_timeout_seconds);
        let data_deadline = last_data + Duration::from_secs(c.idle_timeout_seconds);
        tokio::select! {
            _ = tokio::time::sleep_until(frame_deadline) => bail!("websocket heartbeat timeout"),
            _ = tokio::time::sleep_until(data_deadline) => bail!("websocket data idle timeout (pong is not market data)"),
            _ = tx.closed() => return Ok(()),
            _ = heartbeat.tick() => {
                let ping = match protocol { Protocol::Hyperliquid => Message::Text(json!({"method":"ping"}).to_string().into()),
                    Protocol::Ethereum => Message::Ping(Vec::new().into()) };
                timeout(write_timeout, socket.send(ping)).await??;
                tracing::debug!(venue=protocol.name(), "websocket heartbeat sent");
            },
            msg = socket.next() => {
                let msg = msg.context("websocket EOF")??;
                last_frame = Instant::now();
                match msg {
                    Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t).context("invalid websocket JSON")?;
                        ensure!(v.get("error").is_none(), "websocket RPC error: {}", v["error"]);
                        let (channel, data) = match protocol {
                            Protocol::Hyperliquid => {
                                let channel = v["channel"].as_str().context("missing Hyperliquid channel")?;
                                ensure!(channel != "error", "Hyperliquid subscription rejected: {}", v["data"]);
                                (channel.to_string(), v["data"].clone())
                            },
                            Protocol::Ethereum => {
                                if let Some(id) = v["id"].as_u64() {
                                    let s = subscriptions.get(id.checked_sub(1).context("invalid subscription id")? as usize)
                                        .context("unknown subscription acknowledgement")?;
                                    let name = s[0].as_str().context("subscription type")?.to_string();
                                    let subscription = v["result"].as_str().context("missing subscription result")?.to_string();
                                    ids.insert(subscription.clone(), name.clone());
                                    ("subscriptionResponse".into(), json!({"type":name,"id":subscription}))
                                } else {
                                    ensure!(v["method"] == "eth_subscription", "unexpected Ethereum websocket message");
                                    let id = v["params"]["subscription"].as_str().context("subscription id")?;
                                    (ids.get(id).context("event for unknown subscription")?.clone(), v["params"]["result"].clone())
                                }
                            },
                        };
                        if channel != "pong" && channel != "subscriptionResponse" { last_data = Instant::now(); }
                        if channel == "subscriptionResponse" { tracing::info!(venue=protocol.name(), subscription=%data, "websocket subscription acknowledged"); }
                        emit(tx, &channel, data).await?;
                    },
                    Message::Ping(p) => { timeout(write_timeout, socket.send(Message::Pong(p))).await??; },
                    Message::Close(_) => bail!("server closed websocket"),
                    _ => {},
                }
            }
        }
    }
}

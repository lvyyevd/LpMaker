use alloy::primitives::{Address, U256, keccak256};
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug)]
pub struct RpcError {
    pub method: String,
    pub code: i64,
    pub message: String,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC {} ({}): {}", self.method, self.code, self.message)
    }
}
impl std::error::Error for RpcError {}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
    id: Arc<AtomicU64>,
}
impl Rpc {
    pub fn new(url: String) -> Result<Self> {
        crate::config::validate_url(&url, &["http", "https", "ws", "wss"])?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            url,
            id: Arc::new(AtomicU64::new(1)),
        })
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let attempts = if method == "eth_sendRawTransaction" {
            1
        } else {
            3
        };
        for i in 0..attempts {
            let start = std::time::Instant::now();
            tracing::debug!(method, id, attempt = i + 1, "RPC request started");
            let response =
                tokio::time::timeout(Duration::from_secs(20), self.transport(&request)).await;
            let v = match response {
                Ok(Ok(v)) => v,
                other => {
                    let error = match other {
                        Ok(Err(e)) => e,
                        Err(e) => e.into(),
                        _ => unreachable!(),
                    };
                    tracing::warn!(method, id, attempt=i+1, elapsed_ms=start.elapsed().as_millis(), error=%error, "RPC transport failed");
                    if i + 1 == attempts {
                        return if method == "eth_sendRawTransaction" {
                            Err(error)
                        } else {
                            Err(crate::runtime::ReadUnavailable(format!(
                                "RPC {method} transport unavailable: {error:#}"
                            ))
                            .into())
                        };
                    }
                    tokio::time::sleep(Duration::from_secs(1 << i)).await;
                    continue;
                }
            };
            ensure!(v["id"] == id, "RPC response id mismatch");
            if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
                return Err(RpcError {
                    method: method.into(),
                    code: error["code"].as_i64().context("invalid RPC error code")?,
                    message: error["message"]
                        .as_str()
                        .context("invalid RPC error message")?
                        .into(),
                }
                .into());
            }
            tracing::debug!(
                method,
                id,
                elapsed_ms = start.elapsed().as_millis(),
                "RPC response received"
            );
            return v.get("result").cloned().context("missing RPC result");
        }
        unreachable!()
    }
    async fn transport(&self, request: &Value) -> Result<Value> {
        if self.url.starts_with("ws://") || self.url.starts_with("wss://") {
            let (mut socket, _) = connect_async(&self.url).await?;
            socket
                .send(Message::Text(request.to_string().into()))
                .await?;
            while let Some(message) = socket.next().await {
                match message? {
                    Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t)?;
                        if v["id"] == request["id"] {
                            return Ok(v);
                        }
                    }
                    Message::Ping(p) => socket.send(Message::Pong(p)).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            anyhow::bail!("websocket RPC closed before response");
        }
        let r = self
            .client
            .post(&self.url)
            .json(request)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        Ok(r.error_for_status()
            .map_err(reqwest::Error::without_url)?
            .json()
            .await?)
    }
    pub async fn call_bytes(&self, to: Address, data: Vec<u8>, block: &str) -> Result<Vec<u8>> {
        let v = self
            .request(
                "eth_call",
                json!([{"to":to,"data":format!("0x{}",hex::encode(data))},block]),
            )
            .await?;
        Ok(hex::decode(
            v.as_str().context("call result")?.trim_start_matches("0x"),
        )?)
    }
    pub async fn words(
        &self,
        to: Address,
        signature: &str,
        args: &[U256],
        block: &str,
    ) -> Result<Vec<U256>> {
        let mut data = keccak256(signature.as_bytes())[..4].to_vec();
        for a in args {
            data.extend(a.to_be_bytes::<32>());
        }
        let bytes = self
            .call_bytes(to, data, block)
            .await
            .with_context(|| format!("eth_call {signature} at {to}"))?;
        ensure!(
            bytes.len() % 32 == 0 && !bytes.is_empty(),
            "invalid ABI response for {signature}"
        );
        Ok(bytes.chunks(32).map(U256::from_be_slice).collect())
    }
    pub async fn block_number(&self) -> Result<u64> {
        hex_u64(&self.request("eth_blockNumber", json!([])).await?)
    }
}
pub fn hex_u64(v: &Value) -> Result<u64> {
    Ok(u64::from_str_radix(
        v.as_str()
            .context("expected hex number")?
            .trim_start_matches("0x"),
        16,
    )?)
}
pub fn address_word(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}
pub fn word_address(w: U256) -> Address {
    Address::from_slice(&w.to_be_bytes::<32>()[12..])
}
pub fn signed_tick(w: U256) -> i32 {
    let v = (w & U256::from(0xffffffu32)).to::<u32>() as i32;
    if v & 0x800000 != 0 { v - 0x1000000 } else { v }
}
pub fn tick_word(t: i32) -> U256 {
    if t < 0 {
        U256::MAX - U256::from((-t - 1) as u32)
    } else {
        U256::from(t as u32)
    }
}

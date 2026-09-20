mod limit;
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

#[derive(Debug)]
struct RateLimited {
    retry_after: Option<Duration>,
}
impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EVM RPC 节点限流（HTTP 429 或 JSON-RPC 频率限制）")
    }
}
impl std::error::Error for RateLimited {}

fn limited_error(method: &str, hint: crate::runtime::RetryAfter) -> anyhow::Error {
    let error = anyhow::Error::new(RateLimited { retry_after: None }).context(hint);
    if method == "eth_sendRawTransaction" {
        error // 广播保留未知结果语义，不可自动重发，也不可按只读失败重启交易流程。
    } else {
        error.context(crate::runtime::ReadUnavailable(format!(
            "RPC {method} 暂不可用：共享限流退避"
        )))
    }
}
fn rpc_rate_limit(code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    code == 429
        || (((-32099..=-32000).contains(&code) || code == -32603)
            && [
                "rate limit",
                "too many requests",
                "request limit exceeded",
                "requests per second",
                "rps limit",
            ]
            .iter()
            .any(|needle| message.contains(needle)))
}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
    id: Arc<AtomicU64>,
    gate: Arc<limit::Gate>,
}
impl Rpc {
    pub fn new(url: String) -> Result<Self> {
        Self::with_min_interval(url, 250)
    }
    pub fn with_min_interval(url: String, min_interval_ms: u64) -> Result<Self> {
        crate::config::validate_url(&url, &["http", "https", "ws", "wss"])?;
        ensure!(
            (1..=5_000).contains(&min_interval_ms),
            "invalid RPC request interval"
        );
        let gate = limit::Gate::shared(&url, Duration::from_millis(min_interval_ms))?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            url,
            id: Arc::new(AtomicU64::new(1)),
            gate,
        })
    }
    pub fn cooldown_remaining(&self) -> Duration {
        self.gate.remaining()
    }
    pub fn request_count(&self) -> u64 {
        self.gate.request_count()
    }
    fn on_rate_limit(&self, method: &str, retry_after: Option<Duration>) -> anyhow::Error {
        let hint = self.gate.limited(retry_after);
        tracing::warn!(
            method,
            retry_seconds = self.cooldown_remaining().as_secs_f64().ceil() as u64,
            "EVM RPC 节点限流；同地址的读取与广播统一退避，原交易记录保留"
        );
        limited_error(method, hint)
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
            // 排队也计入单次请求的 20 秒预算，慢查询不能无限阻塞 nonce 或广播。
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            let permit = match tokio::time::timeout_at(deadline, self.gate.acquire()).await {
                Ok(result) => result.map_err(|hint| limited_error(method, hint))?,
                Err(_) => {
                    let message = format!("RPC {method} queue deadline exceeded before dispatch");
                    return if method == "eth_sendRawTransaction" {
                        Err(anyhow::anyhow!(message))
                    } else {
                        Err(crate::runtime::ReadUnavailable(message).into())
                    };
                }
            };
            tracing::debug!(method, id, attempt = i + 1, "RPC request started");
            self.gate.request_started();
            let response = tokio::time::timeout_at(deadline, self.transport(&request)).await;
            let v = match response {
                Ok(Ok(v)) => v,
                other => {
                    let error = match other {
                        Ok(Err(e)) => e,
                        Err(e) => e.into(),
                        _ => unreachable!(),
                    };
                    if let Some(limited) = error.downcast_ref::<RateLimited>() {
                        return Err(self.on_rate_limit(method, limited.retry_after));
                    }
                    drop(permit);
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
                let error = RpcError {
                    method: method.into(),
                    code: error["code"].as_i64().context("invalid RPC error code")?,
                    message: error["message"]
                        .as_str()
                        .context("invalid RPC error message")?
                        .into(),
                };
                if rpc_rate_limit(error.code, &error.message) {
                    return Err(self.on_rate_limit(method, None));
                }
                return Err(error.into());
            }
            let result = v.get("result").cloned().context("missing RPC result")?;
            self.gate.succeeded();
            tracing::debug!(
                method,
                id,
                elapsed_ms = start.elapsed().as_millis(),
                "RPC response received"
            );
            return Ok(result);
        }
        unreachable!()
    }
    async fn transport(&self, request: &Value) -> Result<Value> {
        if self.url.starts_with("ws://") || self.url.starts_with("wss://") {
            let (mut socket, _) = connect_async(&self.url).await.map_err(|error| {
                if let tokio_tungstenite::tungstenite::Error::Http(response) = &error {
                    if response.status().as_u16() == 429 {
                        return anyhow::Error::new(RateLimited {
                            retry_after: limit::retry_after(
                                response
                                    .headers()
                                    .get("retry-after")
                                    .and_then(|v| v.to_str().ok()),
                            ),
                        });
                    }
                    return anyhow::anyhow!("websocket RPC handshake HTTP {}", response.status());
                }
                error.into()
            })?;
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
        if r.status().as_u16() == 429 {
            return Err(RateLimited {
                retry_after: limit::retry_after(
                    r.headers().get("retry-after").and_then(|v| v.to_str().ok()),
                ),
            }
            .into());
        }
        Ok(r.error_for_status()
            .map_err(reqwest::Error::without_url)?
            .json()
            .await
            .map_err(reqwest::Error::without_url)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn queue_wait_is_bounded_and_does_not_make_a_write_retryable() {
        // 持有发送许可，模拟另一条 RPC 卡住；本测试不会建立网络连接。
        let rpc = Rpc::new("http://127.0.0.1:1/queue-timeout-fixture".into()).unwrap();
        let _permit = rpc.gate.acquire().await.unwrap();
        for (method, retryable) in [("eth_blockNumber", true), ("eth_sendRawTransaction", false)] {
            let start = tokio::time::Instant::now();
            let error = rpc.request(method, json!([])).await.unwrap_err();
            assert_eq!(start.elapsed(), Duration::from_secs(20));
            assert!(format!("{error:#}").contains("before dispatch"));
            assert_eq!(crate::runtime::retryable(&error), retryable);
        }
    }
}

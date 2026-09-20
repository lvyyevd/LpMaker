//! 限流测试只连接本机模拟节点，不读取账户、不调用公共 RPC。
mod support;
use lp_maker::{
    config::Config,
    evm::rpc::{Rpc, RpcError},
    runtime,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::Instant,
};

struct HttpMock {
    url: String,
    requests: Arc<Mutex<Vec<Instant>>>,
    task: JoinHandle<()>,
}
impl Drop for HttpMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HttpMock {
    async fn start(status: u16, retry_after: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/rpc-test-secret", listener.local_addr().unwrap());
        let header = retry_after
            .map(|v| format!("Retry-After: {v}\r\n"))
            .unwrap_or_default();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let header = header.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let (body_start, body_len) = loop {
                        let mut part = [0u8; 4096];
                        let n = socket.read(&mut part).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&part[..n]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let len = headers
                                .lines()
                                .find_map(|s| {
                                    s.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|s| s.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            break (end + 4, len);
                        }
                    };
                    while bytes.len() < body_start + body_len {
                        let mut part = [0u8; 4096];
                        let n = socket.read(&mut part).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&part[..n]);
                    }
                    seen.lock().unwrap().push(Instant::now());
                    let body = if status == 200 {
                        let request: Value =
                            serde_json::from_slice(&bytes[body_start..body_start + body_len])
                                .unwrap();
                        json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"}).to_string()
                    } else {
                        "provider error containing rpc-test-secret".into()
                    };
                    let reason = if status == 200 {
                        "OK"
                    } else {
                        "Too Many Requests"
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\n{header}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

#[tokio::test]
async fn concurrent_reads_and_recreated_clients_share_one_429_cooldown() {
    let mock = HttpMock::start(429, Some("120")).await;
    let clients: Vec<_> = (0..8)
        .map(|_| Rpc::new(mock.url.clone()).unwrap())
        .collect();
    let errors = futures_util::future::join_all(
        clients
            .iter()
            .map(|rpc| rpc.request("eth_blockNumber", json!([]))),
    )
    .await;
    for error in errors {
        let error = error.unwrap_err();
        assert!(runtime::retryable(&error));
        assert!(runtime::retry_delay(&error, Duration::from_secs(5)) > Duration::from_secs(119));
        assert!(!format!("{error:#}").contains("rpc-test-secret"));
    }
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    drop(clients);
    let rebuilt = Rpc::new(mock.url.clone()).unwrap();
    assert!(rebuilt.cooldown_remaining() > Duration::from_secs(119));
    assert!(rebuilt.request("eth_chainId", json!([])).await.is_err());
    // HTTP 与 WS 的同地址也不能各自绕过退避。
    let ws = Rpc::new(mock.url.replace("http://", "ws://")).unwrap();
    assert!(ws.request("eth_chainId", json!([])).await.is_err());
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn http_date_and_websocket_handshake_429_use_server_retry_after() {
    let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(600));
    let mock = HttpMock::start(429, Some(&date)).await;
    let rpc = Rpc::new(mock.url.replace("http://", "ws://")).unwrap();
    let error = rpc.request("eth_chainId", json!([])).await.unwrap_err();
    assert!(runtime::retryable(&error));
    assert!(rpc.cooldown_remaining() > Duration::from_secs(598));
    assert!(!format!("{error:#}").contains("rpc-test-secret"));
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn write_limit_is_not_retryable_and_never_rebroadcasts() {
    let mock = HttpMock::start(429, None).await;
    let rpc = Rpc::new(mock.url.clone()).unwrap();
    let error = rpc
        .request("eth_sendRawTransaction", json!(["0xfake"]))
        .await
        .unwrap_err();
    assert!(!runtime::retryable(&error));
    assert!(rpc.cooldown_remaining() > Duration::from_secs(29));
    let error = rpc
        .request("eth_sendRawTransaction", json!(["0xfake"]))
        .await
        .unwrap_err();
    assert!(!runtime::retryable(&error));
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn json_rpc_rate_errors_back_off_but_log_range_and_simulation_errors_do_not() {
    for (code, message, limited) in [
        (-32005, "rate limit exceeded", true),
        (429, "request rejected", true),
        (-32000, "Too many requests", true),
        (-32005, "query exceeds maximum block range", false),
        (3, "execution reverted: rate limit", false),
        (-32000, "max fee per gas less than block base fee", false),
    ] {
        let mock =
            support::rpc::Mock::start(move |_| Err(json!({"code":code,"message":message}))).await;
        let rpc = Rpc::new(mock.url.clone()).unwrap();
        let error = rpc.request("eth_call", json!([])).await.unwrap_err();
        assert_eq!(runtime::retryable(&error), limited, "{message}");
        assert_eq!(!rpc.cooldown_remaining().is_zero(), limited);
        if !limited {
            assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, code);
        }
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn aggregate_request_spacing_is_shared_but_other_endpoints_keep_working() {
    let limited = HttpMock::start(429, None).await;
    let rpc = Rpc::new(limited.url.clone()).unwrap();
    assert!(rpc.block_number().await.is_err());
    let good = HttpMock::start(200, None).await;
    let clients: Vec<_> = (0..6)
        .map(|_| Rpc::with_min_interval(good.url.clone(), 40).unwrap())
        .collect();
    let results = futures_util::future::join_all(clients.iter().map(Rpc::block_number)).await;
    assert!(results.into_iter().all(|r| r.unwrap() == 1));
    let times = good.requests.lock().unwrap();
    assert_eq!(times.len(), 6);
    // 容忍操作系统调度延迟；至少阻止多个客户端同时发送一批请求。
    assert!(times.last().unwrap().duration_since(times[0]) >= Duration::from_millis(190));
    assert_eq!(limited.requests.lock().unwrap().len(), 1);
}

#[test]
fn old_config_has_conservative_defaults_and_invalid_intervals_are_rejected() {
    let mut c = Config::load("config/paper-200.toml").unwrap();
    assert_eq!(c.liquidity.rpc_min_interval_ms, 250);
    assert_eq!(c.monitoring.volume_refresh_seconds, 60);
    c.liquidity.rpc_min_interval_ms = 0;
    assert!(c.validate().is_err());
    c.liquidity.rpc_min_interval_ms = 500;
    c.monitoring.volume_refresh_seconds = 0; // 旧成交量配置已停用，不参与运行。
    assert!(c.validate().is_ok());
}

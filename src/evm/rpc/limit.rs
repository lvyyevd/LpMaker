//! EVM 请求共享节流与限流退避。重建 Rpc 不会清零；WebSocket 订阅和 Hyperliquid 独立。
use crate::runtime::RetryAfter;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::Instant,
};

pub(super) struct Gate {
    state: Mutex<State>,
    // 串行发送，确保收到 429 后，排队的请求会先检查退避，再决定是否发出。
    flight: Semaphore,
    started: AtomicU64,
}
struct State {
    spacing: Duration,
    next: Instant,
    cooldown: Option<Instant>,
    failures: u32,
    healthy_since: Option<Instant>,
}
impl Gate {
    fn new(spacing: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                spacing,
                next: Instant::now(),
                cooldown: None,
                failures: 0,
                healthy_since: None,
            }),
            flight: Semaphore::new(1),
            started: AtomicU64::new(0),
        }
    }
    pub(super) fn request_started(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn request_count(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
    }
    pub(super) fn shared(url: &str, spacing: Duration) -> anyhow::Result<Arc<Self>> {
        // 保留强引用：引擎在读失败后销毁全部客户端，也不能绕过已生效的退避。
        static GATES: OnceLock<Mutex<HashMap<String, Arc<Gate>>>> = OnceLock::new();
        let mut key = reqwest::Url::parse(url)?;
        key.set_fragment(None);
        // 相同端点的 HTTP/WS 请求共用额度；不同路径、端口和鉴权仍然隔离。
        match key.scheme() {
            "ws" => {
                let _ = key.set_scheme("http");
            }
            "wss" => {
                let _ = key.set_scheme("https");
            }
            _ => {}
        }
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .expect("RPC gates poisoned");
        let gate = gates
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Self::new(spacing)))
            .clone();
        let mut state = gate.state.lock().expect("RPC gate poisoned");
        // 不同模块使用同端点时采用较严格的间隔，避免一个模块突破另一个的限制。
        state.spacing = state.spacing.max(spacing);
        drop(state);
        Ok(gate)
    }
    fn cooldown(&self) -> Result<(), RetryAfter> {
        let state = self.state.lock().expect("RPC gate poisoned");
        match state.cooldown {
            Some(until) if until > Instant::now() => Err(RetryAfter(until)),
            _ => Ok(()),
        }
    }
    pub(super) fn remaining(&self) -> Duration {
        self.cooldown().err().map_or(Duration::ZERO, |hint| {
            hint.0.saturating_duration_since(Instant::now())
        })
    }
    pub(super) async fn acquire(&self) -> Result<SemaphorePermit<'_>, RetryAfter> {
        self.cooldown()?;
        let permit = self.flight.acquire().await.expect("RPC gate never closed");
        self.cooldown()?;
        let next = self.state.lock().expect("RPC gate poisoned").next;
        tokio::time::sleep_until(next).await;
        self.cooldown()?;
        let mut state = self.state.lock().expect("RPC gate poisoned");
        state.next = Instant::now() + state.spacing;
        Ok(permit)
    }
    pub(super) fn limited(&self, retry_after: Option<Duration>) -> RetryAfter {
        let mut state = self.state.lock().expect("RPC gate poisoned");
        let backoff = Duration::from_secs((30u64 << state.failures.min(4)).min(300));
        state.failures = state.failures.saturating_add(1);
        state.healthy_since = None;
        // 合法的 Retry-After 可以长于本地上限；不会提前探测服务商明确要求的等待期。
        let delay = backoff.max(retry_after.unwrap_or_default());
        let until = Instant::now() + delay;
        state.cooldown = Some(until);
        RetryAfter(until)
    }
    pub(super) fn succeeded(&self) {
        let mut state = self.state.lock().expect("RPC gate poisoned");
        if state.cooldown.take().is_some() {
            tracing::info!("EVM RPC 限流后首次请求成功；后续仍按共享间隔发送");
        }
        let healthy = *state.healthy_since.get_or_insert(Instant::now());
        // 单次探测成功不立即清除累计退避，防止节点反复限流时退回每 30 秒重试。
        if healthy.elapsed() >= Duration::from_secs(60) {
            state.failures = 0;
        }
    }
}

/// HTTP Retry-After 支持秒数与 HTTP 日期；不把包含地址/令牌的响应体写入日志。
pub(super) fn retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    let delay = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(SystemTime::now())
            .unwrap_or_default()
    };
    Instant::now().checked_add(delay).map(|_| delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn repeated_limits_wait_longer_and_one_success_does_not_reset_backoff() {
        let gate = Gate::new(Duration::from_millis(250));
        for seconds in [30, 60, 120, 240, 300, 300] {
            gate.limited(None);
            assert_eq!(gate.remaining(), Duration::from_secs(seconds));
            assert!(gate.acquire().await.is_err());
            tokio::time::advance(Duration::from_secs(seconds)).await;
            let permit = gate.acquire().await.unwrap();
            gate.succeeded();
            drop(permit);
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        gate.succeeded();
        gate.limited(None);
        assert_eq!(gate.remaining(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_and_cancelled_queue_preserve_spacing_and_release_permits() {
        let gate = Gate::new(Duration::from_millis(250));
        let permit = gate.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), gate.acquire())
                .await
                .is_err()
        );
        gate.limited(Some(Duration::from_secs(600)));
        drop(permit);
        assert_eq!(gate.remaining(), Duration::from_secs(600));
        tokio::time::advance(Duration::from_secs(600)).await;
        drop(gate.acquire().await.unwrap());
        let started = Instant::now();
        drop(gate.acquire().await.unwrap());
        assert_eq!(started.elapsed(), Duration::from_millis(250));
    }

    #[test]
    fn parses_retry_after_seconds_date_and_invalid_values() {
        assert_eq!(retry_after(Some("120")), Some(Duration::from_secs(120)));
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(601));
        let parsed = retry_after(Some(&date)).unwrap().as_secs();
        assert!((599..=601).contains(&parsed));
        for invalid in [
            None,
            Some("garbage"),
            Some("-1"),
            Some("18446744073709551615"),
        ] {
            assert!(retry_after(invalid).is_none());
        }
    }
}

//! Retry only failed observations, never an uncertain exchange write.
use crate::store::Store;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{fmt, future::Future, time::Duration};

#[derive(Debug)]
pub struct ReadUnavailable(pub String);
impl fmt::Display for ReadUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReadUnavailable {}
pub fn retryable(error: &anyhow::Error) -> bool {
    error.is::<ReadUnavailable>()
}

pub fn fresh(source_ms: u64, now_ms: u64, max_age_seconds: u64) -> Result<()> {
    if source_ms == 0
        || source_ms > now_ms.saturating_add(5_000)
        || now_ms.saturating_sub(source_ms) > max_age_seconds.saturating_mul(1000)
    {
        return Err(ReadUnavailable(
            "observation expired or timestamp invalid; refresh before acting".into(),
        )
        .into());
    }
    Ok(())
}
pub async fn observe<T>(seconds: u64, future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(seconds), future)
        .await
        .map_err(|_| ReadUnavailable("observation batch deadline exceeded".into()))?
}

pub fn status(store: &Store, state: &str, details: Value) -> Result<()> {
    store.update::<Value>("runtime_health.json", |v| {
        if !v.is_object() {
            *v = json!({});
        }
        v["status"] = json!(state);
        v["updated_ms"] = json!(crate::now_ms());
        v["details"] = details;
        if state == "running" {
            v["last_decision_ms"] = json!(crate::now_ms());
        }
        Ok(())
    })
}
pub async fn retry_reads<T, F, Fut>(
    store: &Store,
    delay: Duration,
    once: bool,
    mut task: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    loop {
        match task().await {
            Err(e) if !once && retryable(&e) => {
                let details = json!({"error":format!("{e:#}"),"retry_seconds":delay.as_secs(),
                    "action":"pause new operations; reload checkpoint and reconcile before resuming"});
                status(store, "degraded", details.clone())?;
                store.event("read_recovery_wait", &details)?;
                tracing::warn!(error=%format!("{e:#}"), "read unavailable; restart reconciliation after backoff");
                tokio::time::sleep(delay).await;
            }
            result => return result,
        }
    }
}
pub fn check_health(store: &Store, max_age_seconds: u64) -> Result<Value> {
    let v = store
        .read::<Value>("runtime_health.json")?
        .ok_or_else(|| anyhow::anyhow!("no runtime health record"))?;
    ensure!(
        v["status"] == "running",
        "runtime is not healthy: {}",
        v["status"]
    );
    fresh(
        v["last_decision_ms"].as_u64().unwrap_or(0),
        crate::now_ms(),
        max_age_seconds,
    )?;
    Ok(v)
}
pub async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! { _=term.recv()=>{}, _=tokio::signal::ctrl_c()=>{} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

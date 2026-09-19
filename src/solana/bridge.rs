//! 与官方 JS SDK 的窄接口。子进程不自动广播；超时销毁进程，未知交易由持久化签名核对。
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
struct Process {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    seq: u64,
}
pub struct Bridge(Mutex<Process>);
impl Bridge {
    pub async fn start(c: &super::config::Config, live: bool) -> Result<Self> {
        let mut child = Command::new("node")
            .arg(&c.adapter_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start Solana SDK adapter; run npm ci --prefix adapters/solana")?;
        let input = child.stdin.take().context("adapter stdin")?;
        let output = BufReader::new(child.stdout.take().context("adapter stdout")?).lines();
        let b = Self(Mutex::new(Process {
            child,
            input,
            output,
            seq: 0,
        }));
        b.call(json!({"method":"init","config":c.solana,"live":live}))
            .await?;
        Ok(b)
    }
    pub async fn call(&self, mut request: Value) -> Result<Value> {
        let mut p = self.0.lock().await;
        p.seq += 1;
        let id = p.seq;
        request["id"] = json!(id);
        let result=tokio::time::timeout(Duration::from_secs(55),async {
            let bytes=serde_json::to_vec(&request)?;p.input.write_all(&bytes).await?;p.input.write_all(b"\n").await?;p.input.flush().await?;
            let line=p.output.next_line().await?.context("Solana adapter stopped")?;
            ensure!(line.len()<4_000_000,"oversized adapter response");
            let response:Value=serde_json::from_str(&line).context("invalid adapter response")?;
            ensure!(response["id"]==id,"adapter sequence mismatch");
            ensure!(response["ok"]==true,"Solana adapter {} failed; RPC, simulation or account validation failed (credentials redacted)",request["method"]);
            Ok(response["value"].clone())
        }).await;
        match result {
            Ok(r) => r,
            Err(_) => {
                let _ = p.child.kill().await;
                bail!(
                    "Solana adapter timeout; pending transaction must be reconciled by original signature"
                )
            }
        }
    }
}

#[async_trait::async_trait]
pub trait Adapter: Send + Sync {
    async fn call(&self, request: Value) -> Result<Value>;
}
#[async_trait::async_trait]
impl Adapter for Bridge {
    async fn call(&self, request: Value) -> Result<Value> {
        Bridge::call(self, request).await
    }
}

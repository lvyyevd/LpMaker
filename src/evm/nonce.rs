//! Serial nonce lifecycle. Pending/unknown transactions are never replaced automatically.
use super::rpc::{Rpc, hex_u64};
use crate::store::Store;
use alloy::primitives::Address;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NonceState {
    pub chain_id: u64,
    pub owner: String,
    pub latest: u64,
    pub pending: u64,
    pub next_floor: u64,
    pub checked_ms: u64,
    pub inflight: Option<Value>,
}
impl NonceState {
    pub fn observe(&mut self, latest: u64, pending: u64, now: u64) -> Result<()> {
        ensure!(pending >= latest, "inconsistent RPC nonce snapshot");
        self.latest = latest;
        self.pending = pending;
        self.next_floor = self.next_floor.max(latest);
        self.checked_ms = now;
        Ok(())
    }
    pub fn available(&self) -> Result<u64> {
        ensure!(
            self.inflight.is_none(),
            "local transaction unresolved; reconcile before sending"
        );
        ensure!(
            self.pending == self.latest,
            "external pending transactions; nonce allocation blocked"
        );
        ensure!(
            self.latest >= self.next_floor,
            "RPC nonce regressed or submitted nonce unresolved; do not reuse nonce"
        );
        Ok(self.latest)
    }
}
pub struct NonceManager {
    rpc: Rpc,
    owner: Address,
    chain_id: u64,
    warn_ms: u64,
    store: Arc<Store>,
    lock: Mutex<()>,
}
impl NonceManager {
    pub fn new(
        rpc: Rpc,
        owner: Address,
        chain_id: u64,
        warn_seconds: u64,
        store: Arc<Store>,
    ) -> Self {
        Self {
            rpc,
            owner,
            chain_id,
            warn_ms: warn_seconds * 1000,
            store,
            lock: Mutex::new(()),
        }
    }
    fn load(&self) -> Result<NonceState> {
        let s = self
            .store
            .read::<NonceState>("evm_nonce.json")?
            .unwrap_or(NonceState {
                chain_id: self.chain_id,
                owner: self.owner.to_string(),
                ..Default::default()
            });
        ensure!(
            s.chain_id == self.chain_id && s.owner.eq_ignore_ascii_case(&self.owner.to_string()),
            "nonce state belongs to another chain/wallet"
        );
        Ok(s)
    }
    pub async fn refresh(&self) -> Result<NonceState> {
        let _guard = self.lock.lock().await;
        ensure!(
            hex_u64(&self.rpc.request("eth_chainId", json!([])).await?)? == self.chain_id,
            "nonce refresh chain mismatch"
        );
        let latest = hex_u64(
            &self
                .rpc
                .request("eth_getTransactionCount", json!([self.owner, "latest"]))
                .await?,
        )?;
        let pending = hex_u64(
            &self
                .rpc
                .request("eth_getTransactionCount", json!([self.owner, "pending"]))
                .await?,
        )?;
        let mut state = self.load()?;
        state.observe(latest, pending, crate::now_ms())?;
        // Recover the narrow crash window between pending.json and nonce-state writes.
        if let Some(op) = self.store.pending()?.filter(|p| p["venue"] == "evm") {
            ensure!(
                op["owner"]
                    .as_str()
                    .is_some_and(|o| o.eq_ignore_ascii_case(&self.owner.to_string())),
                "pending signer mismatch"
            );
            let n = op["nonce"].as_u64().context("pending nonce")?;
            state.next_floor = state
                .next_floor
                .max(n.checked_add(1).context("nonce overflow")?);
            if state.inflight.is_none() {
                state.inflight = Some(
                    json!({"nonce":n,"hash":op["hash"],"prepared_ms":op["prepared_ms"].as_u64().unwrap_or(crate::now_ms()),"status":"unresolved"}),
                );
            }
        }
        if let Some(op) = &state.inflight {
            let hash = op["hash"].as_str().context("inflight hash")?;
            let receipt = self
                .rpc
                .request("eth_getTransactionReceipt", json!([hash]))
                .await?;
            let age_ms = crate::now_ms()
                .saturating_sub(op["prepared_ms"].as_u64().unwrap_or(crate::now_ms()));
            if age_ms >= self.warn_ms {
                tracing::warn!(
                    hash,
                    age_ms,
                    latest,
                    pending,
                    receipt_seen = !receipt.is_null(),
                    "transaction unresolved; reconciliation required, no automatic replacement"
                );
            }
        }
        self.store.write("evm_nonce.json", &state)?;
        tracing::info!(owner=%self.owner, latest, pending, next_floor=state.next_floor, unresolved=state.inflight.is_some(), "EVM nonce refreshed");
        Ok(state)
    }
    pub async fn next(&self) -> Result<u64> {
        ensure!(
            self.store.pending()?.is_none(),
            "unresolved operation; reconcile before nonce allocation"
        );
        self.refresh().await?.available()
    }
    pub async fn prepared(&self, nonce: u64, hash: &str) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut state = self.load()?;
        ensure!(
            state.inflight.as_ref().is_none_or(|p| p["hash"] == hash),
            "another nonce already reserved"
        );
        state.next_floor = state
            .next_floor
            .max(nonce.checked_add(1).context("nonce overflow")?);
        state.inflight = Some(
            json!({"nonce":nonce,"hash":hash,"prepared_ms":crate::now_ms(),"status":"prepared"}),
        );
        self.store.write("evm_nonce.json", &state)?;
        self.store.event("evm_nonce_reserved", &state)
    }
    pub async fn confirmed(&self, hash: &str, receipt: &Value) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut state = self.load()?;
        if let Some(op) = &state.inflight {
            ensure!(op["hash"] == hash, "receipt does not match reserved nonce");
        }
        state.inflight = None;
        self.store.write("evm_nonce.json", &state)?;
        self.store.event("evm_nonce_confirmed", json!({"hash":hash,"status":receipt["status"],"block":receipt["blockNumber"],"next_floor":state.next_floor}))?;
        self.store.finish(receipt)
    }
}

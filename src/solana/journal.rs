//! Solana 签名唯一标识一次提交：先落盘再广播；未知结果不重签、不换 blockhash 盲发。
use super::bridge::Adapter;
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionRecord {
    pub address: String,
    pub since_ms: u64,
}
pub type Positions = BTreeMap<String, PositionRecord>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub prepared: Value,
    pub layer: Option<String>,
    pub operation: String,
}
pub fn resolved(status: &Value) -> Result<bool> {
    let Some(s) = status.get("status").filter(|s| !s.is_null()) else {
        return Ok(false);
    };
    if s["confirmationStatus"] != "confirmed" && s["confirmationStatus"] != "finalized" {
        return Ok(false);
    }
    Ok(true)
}
pub async fn reconcile(b: &impl Adapter, store: &Store) -> Result<()> {
    let Some(p) = store.read::<Pending>("solana_pending.json")? else {
        return Ok(());
    };
    let signature = p.prepared["signature"]
        .as_str()
        .context("pending signature")?;
    let status = b
        .call(json!({"method":"status","signature":signature}))
        .await?;
    store.write(
        "solana_last_confirmation.json",
        &json!({"signature":signature,"status":status,"observed_ms":crate::now_ms()}),
    )?;
    if !resolved(&status)? {
        bail!(
            "Solana transaction unresolved: {signature}; keep original signature, no automatic re-sign (even after blockhash expiry)"
        );
    }
    if !status["status"]["err"].is_null() {
        store.event(
            "solana_transaction_failed",
            json!({"signature":signature,"operation":p.operation}),
        )?;
        store.write("solana_pending.json", &Option::<Pending>::None)?;
        bail!(
            "Solana transaction failed on chain; reconcile incomplete workflow before further entry"
        );
    }
    apply_confirmed(store, &p)?;
    tracing::info!(%signature,operation=%p.operation,"Solana 交易已确认，更新持仓记录");
    store.event(
        "solana_transaction_confirmed",
        json!({"signature":signature,"operation":p.operation,"layer":p.layer}),
    )?;
    store.write("solana_pending.json", &Option::<Pending>::None)?;
    Ok(())
}
pub fn apply_confirmed(store: &Store, p: &Pending) -> Result<()> {
    if let Some(layer) = &p.layer {
        let address = p.prepared["position"]
            .as_str()
            .context("position address")?
            .to_string();
        store.update::<Positions>("solana_positions.json", |rows| {
            if p.operation == "mint" {
                rows.entry(layer.clone()).or_insert(PositionRecord {
                    address: address.clone(),
                    since_ms: crate::now_ms(),
                });
            }
            // Remove mapping only when the final remove/claim/close transaction is confirmed.
            if p.operation == "remove"
                && p.prepared["index"].as_u64().unwrap_or(0) + 1
                    == p.prepared["count"].as_u64().unwrap_or(0)
            {
                rows.remove(layer);
            }
            Ok(())
        })?;
    }
    Ok(())
}
pub async fn execute(
    b: &impl Adapter,
    store: &Store,
    mut intent: Value,
    layer: Option<&str>,
) -> Result<()> {
    ensure!(
        store.pending()?.is_none() && store.read::<Pending>("solana_pending.json")?.is_none(),
        "unresolved venue operation blocks new submission"
    );
    intent["method"] = json!("plan");
    let kind = intent["kind"]
        .as_str()
        .context("operation kind")?
        .to_owned();
    let plan = b.call(intent).await?;
    tracing::info!(operation=%kind,layer,transactions=%plan["count"],"Solana 操作计划已生成，等待逐笔模拟与确认");
    let count = plan["count"].as_u64().context("transaction count")?;
    store.event(
        "solana_plan",
        json!({"kind":kind,"layer":layer,"plan":plan}),
    )?;
    for index in 0..count {
        let prepared = b.call(json!({"method":"prepare","index":index})).await?;
        let p = Pending {
            prepared,
            layer: layer.map(str::to_owned),
            operation: kind.clone(),
        };
        store.write("solana_pending.json", &p)?;
        tracing::info!(signature=%p.prepared["signature"],operation=%kind,index,count,"Solana 签名已持久化，开始广播；未知结果保留原签名");
        store.event("solana_transaction_prepared",json!({"signature":p.prepared["signature"],"operation":kind,"index":index,"count":count,"fee_lamports":p.prepared["estimated_fee_lamports"],"rent_lamports":p.prepared["rent_lamports"]}))?;
        // Any error leaves signed bytes persisted; even an RPC rejection may race acceptance elsewhere.
        let response = b
            .call(json!({"method":"send","raw_transaction":p.prepared["raw_transaction"]}))
            .await?;
        ensure!(
            response["signature"] == p.prepared["signature"],
            "RPC signature mismatch; retain pending"
        );
        let mut confirmed = false;
        for _ in 0..40 {
            let status = b
                .call(json!({"method":"status","signature":p.prepared["signature"]}))
                .await?;
            if resolved(&status)? {
                reconcile(b, store).await?;
                confirmed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        ensure!(
            confirmed,
            "confirmation timeout; persisted signature blocks duplicate submission"
        );
        b.call(json!({"method":"ack","index":index})).await?;
    }
    Ok(())
}

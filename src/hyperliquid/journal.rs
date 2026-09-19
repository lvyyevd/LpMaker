//! Durable order intents and exchange observations. Absence is not proof of cancellation.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub type Orders = BTreeMap<String, Order>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Order {
    pub cloid: String,
    pub oid: Option<u64>,
    pub user: String,
    pub managed_hedge: bool,
    pub submitted_ms: u64,
    pub observed_ms: u64,
    pub wire: Value,
    pub status: String,
    pub terminal: bool,
    pub exchange: Value,
}
pub fn validate_new_ids(store: &Store, action: &Value) -> Result<()> {
    if action["type"] != "order" {
        return Ok(());
    }
    let ledger = store.read::<Orders>("orders.json")?.unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    for wire in action["orders"].as_array().context("order intents")? {
        let id = wire["c"].as_str().context("client ID required")?;
        ensure!(
            seen.insert(id.to_ascii_lowercase())
                && !ledger.keys().any(|old| old.eq_ignore_ascii_case(id))
                && store.archived_order(id)?.is_none(),
            "client order ID already used; reject before preparing a new mutation"
        );
    }
    Ok(())
}
pub fn prepare(store: &Store, action: &Value, user: &str, nonce: u64) -> Result<()> {
    if action["type"] != "order" {
        return Ok(());
    }
    let hedge = store
        .read::<Value>("hedge_order.json")?
        .unwrap_or(Value::Null);
    let pending = store.pending()?.unwrap_or(Value::Null);
    for wire in action["orders"]
        .as_array()
        .context("order intents missing")?
    {
        ensure!(
            store
                .archived_order(wire["c"].as_str().context("cloid")?)?
                .is_none(),
            "client order ID already archived; do not reuse or resubmit"
        );
    }
    store.update::<Orders>("orders.json", |ledger| {
        for wire in action["orders"]
            .as_array()
            .context("order intents missing")?
        {
            let cloid = wire["c"]
                .as_str()
                .context("durable order requires a cloid")?;
            if let Some(old) = ledger.get_mut(cloid) {
                if old.wire.is_null() && old.user.eq_ignore_ascii_case(user) {
                    old.wire = wire.clone();
                    old.submitted_ms = nonce;
                    continue;
                }
                ensure!(
                    old.user.eq_ignore_ascii_case(user)
                        && old.wire == *wire
                        && old.submitted_ms == nonce,
                    "client order ID already used; do not resubmit"
                );
                continue;
            }
            ledger.insert(
                cloid.into(),
                Order {
                    cloid: cloid.into(),
                    oid: None,
                    user: user.into(),
                    managed_hedge: hedge["cloid"] == cloid
                        || pending["hedge_intent"]["cloid"] == cloid,
                    submitted_ms: nonce,
                    observed_ms: 0,
                    wire: wire.clone(),
                    status: "prepared".into(),
                    terminal: false,
                    exchange: Value::Null,
                },
            );
        }
        Ok(())
    })
}
/// Import the previous release's strategy order pointer, without inventing order details.
pub fn import_legacy(store: &Store, user: &str) -> Result<()> {
    if let Some(h) = store.read::<Value>("hedge_order.json")? {
        let id = h["cloid"].as_str().context("invalid legacy hedge order")?;
        store.update::<Orders>("orders.json", |ledger| {
            ledger.entry(id.into()).or_insert(Order {
                cloid: id.into(),
                oid: None,
                user: user.into(),
                managed_hedge: true,
                submitted_ms: 0,
                observed_ms: 0,
                wire: Value::Null,
                status: "legacy_unverified".into(),
                terminal: false,
                exchange: Value::Null,
            });
            Ok(())
        })?;
    }
    Ok(())
}
pub fn acknowledged(store: &Store, action: &Value, response: &Value) -> Result<()> {
    if action["type"] != "order" {
        return Ok(());
    }
    let intents = action["orders"].as_array().context("order intents")?;
    let statuses = response["response"]["data"]["statuses"].as_array();
    ensure!(
        response["status"] == "err" || statuses.is_some_and(|s| s.len() == intents.len()),
        "order acknowledgement incomplete; pending retained"
    );
    store.update::<Orders>("orders.json", |ledger| {
        for (index, intent) in intents.iter().enumerate() {
            let id = intent["c"].as_str().context("client ID")?;
            let order = ledger.get_mut(id).context("order intent not persisted")?;
            let ack = statuses.map(|s| &s[index]).unwrap_or(response);
            let (status, terminal, oid) =
                if response["status"] == "err" || ack.get("error").is_some() {
                    ("rejected", true, None)
                } else if let Some(oid) = ack["resting"]["oid"].as_u64() {
                    ("open", false, Some(oid))
                } else if let Some(oid) = ack["filled"]["oid"].as_u64() {
                    ("filled", true, Some(oid))
                } else {
                    anyhow::bail!("unrecognized order acknowledgement; pending retained");
                };
            order.status = status.into();
            order.terminal = terminal;
            order.oid = oid;
            order.observed_ms = crate::now_ms();
            order.exchange = ack.clone();
        }
        Ok(())
    })
}
pub fn terminal_status(status: &str) -> bool {
    matches!(
        status,
        "filled"
            | "not_submitted"
            | "canceled"
            | "rejected"
            | "marginCanceled"
            | "vaultWithdrawalCanceled"
            | "openInterestCapCanceled"
            | "selfTradeCanceled"
            | "reduceOnlyCanceled"
            | "siblingFilledCanceled"
            | "delistedCanceled"
            | "liquidatedCanceled"
            | "scheduledCancel"
            | "tickRejected"
            | "minTradeNtlRejected"
            | "perpMarginRejected"
            | "reduceOnlyRejected"
            | "badAloPxRejected"
            | "iocCancelRejected"
            | "badTriggerPxRejected"
            | "marketOrderNoLiquidityRejected"
            | "positionIncreaseAtOpenInterestCapRejected"
            | "positionFlipAtOpenInterestCapRejected"
            | "tooAggressiveAtOpenInterestCapRejected"
            | "openInterestIncreaseRejected"
            | "insufficientSpotBalanceRejected"
            | "oracleRejected"
            | "perpMaxPositionRejected"
    )
}
/// Only a durable pre-dispatch state proves the network send never began.
pub fn not_submitted(store: &Store, action: &Value) -> Result<()> {
    if action["type"] == "order" {
        store.update::<Orders>("orders.json", |ledger| {
            for wire in action["orders"].as_array().context("order intents")? {
                let id = wire["c"].as_str().context("cloid")?;
                if let Some(o) = ledger.get_mut(id) {
                    ensure!(
                        o.oid.is_none()
                            && matches!(o.status.as_str(), "prepared" | "not_submitted"),
                        "pre-dispatch order has unexpected exchange evidence"
                    );
                    o.status = "not_submitted".into();
                    o.terminal = true;
                    o.observed_ms = crate::now_ms();
                }
            }
            Ok(())
        })?;
    }
    store.write("hedge_order.json", &Option::<Value>::None)
}
pub fn observe(store: &Store, id: &str, response: &Value) -> Result<()> {
    // Retain the raw observation even when it cannot resolve the order.
    store.update::<Orders>("orders.json", |ledger| {
        let o = ledger.get_mut(id).context("unknown local order")?;
        if response["status"] == "order" {
            let row = &response["order"]["order"];
            ensure!(
                row["cloid"].is_null()
                    || row["cloid"]
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case(id)),
                "order cloid mismatch"
            );
            let oid = row["oid"].as_u64().context("order oid missing")?;
            ensure!(
                o.oid.is_none_or(|old| old == oid),
                "order oid changed unexpectedly"
            );
            let status = response["order"]["status"]
                .as_str()
                .context("order status missing")?;
            ensure!(
                !o.terminal || o.status == status,
                "terminal order status cannot regress"
            );
            o.oid = Some(oid);
            o.status = status.into();
            o.terminal = terminal_status(&o.status);
        } else {
            // 一次索引缺失不能抹掉已有回执/成交证据。原始响应仍写入审计日志。
            if o.oid.is_some() || o.terminal {
                return Ok(());
            }
            o.status = "unknown".into();
            o.terminal = false;
        }
        o.observed_ms = crate::now_ms();
        o.exchange = response.clone();
        Ok(())
    })?;
    store.event("order_reconciled", json!({"cloid":id,"response":response}))?;
    ensure!(
        response["status"] == "order",
        "order {id} unknown; absence/expiry is not proof of no fill"
    );
    let status = response["order"]["status"]
        .as_str()
        .context("order status")?;
    ensure!(
        status == "open" || terminal_status(status),
        "unresolved order status {status}; no automatic replay"
    );
    Ok(())
}

pub async fn refresh(client: &super::Client, store: &Store) -> Result<()> {
    let user = client.user()?;
    import_legacy(store, user)?;
    let orders = store.read::<Orders>("orders.json")?.unwrap_or_default();
    for order in orders.values() {
        ensure!(
            order.user.eq_ignore_ascii_case(user),
            "order ledger belongs to another account"
        );
        if !order.terminal {
            super::order_recovery::resolve(client, store, &order.cloid, false).await?;
        }
    }
    Ok(())
}
/// Prove ownership before cancelling any stale strategy maker order. Manual orders block startup.
pub fn managed_open_orders(store: &Store, open: &Value, coin: &str) -> Result<Vec<String>> {
    let ledger = store.read::<Orders>("orders.json")?.unwrap_or_default();
    let mut ids = vec![];
    for row in open.as_array().context("open orders response")? {
        let oid = row["oid"].as_u64().context("open order oid")?;
        let order = ledger.values().find(|o| o.oid == Some(oid)).context(
            "untracked exchange order; recorded but will not cancel/adopt automatically",
        )?;
        ensure!(
            order.managed_hedge && row["coin"] == coin,
            "non-strategy or inconsistent open order; startup blocked"
        );
        if order.terminal {
            return Err(crate::runtime::ReadUnavailable(
                "exchange open orders lag a confirmed terminal order; retry reconciliation".into(),
            )
            .into());
        }
        ids.push(order.cloid.clone());
    }
    if !ledger
        .values()
        .filter(|o| !o.terminal)
        .all(|o| ids.contains(&o.cloid))
    {
        // 两次只读快照之间可能成交/撤销，也可能索引延迟；重新核对而不是推断终态。
        return Err(crate::runtime::ReadUnavailable(
            "local unresolved order absent from exchange open orders; retry reconciliation without new orders".into(),
        ).into());
    }
    Ok(ids)
}
pub fn compact(store: &Store) -> Result<()> {
    let pointer = store
        .read::<Value>("hedge_order.json")?
        .unwrap_or(Value::Null);
    let pending = store.pending()?.unwrap_or(Value::Null);
    // Never prune any ledger entry while a mutation still needs reconciliation.
    if !pending.is_null() {
        return Ok(());
    }
    store.update::<Orders>("orders.json", |ledger| {
        let mut terminal: Vec<_> = ledger
            .values()
            .filter(|o| o.terminal && pointer["cloid"] != o.cloid)
            .map(|o| (o.observed_ms, o.cloid.clone()))
            .collect();
        terminal.sort();
        let remove = terminal
            .len()
            .saturating_sub(store.policy.terminal_orders_keep);
        for (_, id) in terminal.into_iter().take(remove) {
            let order = ledger.get(&id).context("terminal order disappeared")?;
            store.archive_order(&id, &serde_json::to_value(order)?)?;
            ledger.remove(&id);
        }
        Ok(())
    })
}

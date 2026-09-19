//! 订单回执和查询索引可能短暂不同步。这里只重试读取，绝不重发交易。
use super::{Client, journal};
use crate::{runtime::ReadUnavailable, store::Store};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::time::Duration;

const ATTEMPTS: usize = 5;

/// 有交易所 OID 时优先查 OID；没有回执时仍可用持久化的 CLOID 找回订单。
/// 撤单后必须读到终态，不能把「挂单列表中消失」当作已撤单/未成交。
pub async fn resolve(
    client: &Client,
    store: &Store,
    id: &str,
    terminal_only: bool,
) -> Result<Value> {
    for attempt in 0..ATTEMPTS {
        let ledger = store
            .read::<journal::Orders>("orders.json")?
            .unwrap_or_default();
        let order = ledger
            .get(id)
            .context("order recovery requires a durable intent")?;
        ensure!(
            order.user.eq_ignore_ascii_case(client.user()?),
            "order ledger belongs to another account"
        );

        // 已持久化的成交/拒绝等终态足以阻止重发，后续账户读取会核对实际仓位。
        if order.terminal {
            ensure!(
                journal::terminal_status(&order.status),
                "invalid durable terminal status"
            );
            return Ok(json!({"status":"order","order":{"status":order.status,
                "order":{"oid":order.oid,"cloid":id}},"source":"durable_terminal"}));
        }
        let mut queries = Vec::new();
        if let Some(oid) = order.oid {
            queries.push(json!(oid));
        }
        queries.push(json!(id));
        let mut known_open = None;
        for query in queries {
            let response = client.order_status(query.clone()).await?;
            store.event(
                "order_status_query",
                json!({"cloid":id,"query":query,
                "attempt":attempt+1,"terminal_only":terminal_only,"response":response}),
            )?;
            if response["status"] == "order" {
                // 身份不符、未知状态等协议问题立即阻断，不能当作索引延迟重试。
                journal::observe(store, id, &response)?;
                if !terminal_only || response["order"]["status"] != "open" {
                    tracing::debug!(cloid=id, oid=?order.oid, status=%response["order"]["status"], "Hyperliquid 订单核对完成");
                    return Ok(response);
                }
                known_open = Some(response);
            } else {
                ensure!(
                    response["status"] == "unknownOid",
                    "unexpected order status response; order retained"
                );
            }
        }

        if known_open.is_none() {
            let open = client.open_orders().await?;
            store.write(
                "open_orders.json",
                &json!({"observed_ms":crate::now_ms(),
                "user":client.user()?,"orders":open}),
            )?;
            // 只有相同的已确认 OID，或相同 CLOID，才能证明是本程序的订单。
            // 不按币种/价格/数量猜测归属，也不接管手工订单。
            let matches: Vec<_> = open
                .as_array()
                .context("open orders response")?
                .iter()
                .filter(|row| {
                    order
                        .oid
                        .is_some_and(|oid| row["oid"].as_u64() == Some(oid))
                        || row["cloid"]
                            .as_str()
                            .is_some_and(|v| v.eq_ignore_ascii_case(id))
                })
                .collect();
            ensure!(
                matches.len() <= 1,
                "multiple open orders match one durable intent"
            );
            if let Some(row) = matches.first() {
                let response = json!({"status":"order","order":{"status":"open","order":row},
                    "source":"openOrders"});
                journal::observe(store, id, &response)?;
                if !terminal_only {
                    tracing::info!(cloid=id, oid=%row["oid"], "订单状态索引暂未同步，已通过当前挂单列表确认原订单");
                    return Ok(response);
                }
            }
        }
        if attempt + 1 < ATTEMPTS {
            let delay_ms = 250_u64 << attempt;
            tracing::info!(cloid=id, oid=?order.oid, attempt=attempt+1, delay_ms,
                terminal_only, "Hyperliquid 订单查询暂未确认，保留原订单并重试只读核对");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    // 连续运行时由外层恢复循环退避重查，期间禁止策略继续交易；单次命令报错。
    // 即使没有 pending.json，orders.json 和 hedge_order.json 也必须保留。
    let message = format!(
        "Hyperliquid order {id} remains unresolved after {ATTEMPTS} read attempts; retained original order, no resubmission; absence is not proof of no fill"
    );
    store.event(
        "order_reconciliation_wait",
        json!({"cloid":id,"terminal_only":terminal_only,"error":message}),
    )?;
    Err(ReadUnavailable(message).into())
}

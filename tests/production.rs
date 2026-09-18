use lp_maker::{
    config::StorageConfig,
    hyperliquid::journal::{self, Order, Orders},
    runtime,
    store::Store,
};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[tokio::test]
async fn read_failure_enters_degraded_state_then_retries_without_touching_pending() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .begin(json!({"venue":"fixture","hash":"unchanged"}))
        .unwrap();
    let attempts = AtomicUsize::new(0);
    let result = runtime::retry_reads(&store, Duration::from_millis(1), false, || async {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(runtime::ReadUnavailable("connection reset".into()).into());
        }
        assert_eq!(
            store.read::<Value>("runtime_health.json")?.unwrap()["status"],
            "degraded"
        );
        Ok(42)
    })
    .await
    .unwrap();
    assert_eq!(result, 42);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(store.pending().unwrap().unwrap()["hash"], "unchanged");
}

#[tokio::test]
async fn uncertain_write_and_state_errors_are_never_automatically_retried() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let attempts = AtomicUsize::new(0);
    let result: anyhow::Result<()> =
        runtime::retry_reads(&store, Duration::from_millis(1), false, || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("uncertain exchange result")
        })
        .await;
    assert!(result.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn observation_timeout_and_future_or_stale_timestamps_fail_closed() {
    let result: anyhow::Result<()> =
        runtime::observe(1, async { std::future::pending().await }).await;
    assert!(runtime::retryable(&result.unwrap_err()));
    assert!(runtime::fresh(1_000_000, 1_061_000, 60).is_err());
    assert!(runtime::fresh(1_010_000, 1_000_000, 60).is_err());
    assert!(runtime::fresh(1_000_000, 1_030_000, 60).is_ok());
}

#[test]
fn runtime_health_requires_recent_completed_decisions_and_running_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    runtime::status(&store, "starting", json!({})).unwrap();
    assert!(runtime::check_health(&store, 120).is_err());
    runtime::status(&store, "running", json!({})).unwrap();
    assert!(runtime::check_health(&store, 120).is_ok());
    store
        .write(
            "runtime_health.json",
            &json!({"status":"running","last_decision_ms":lp_maker::now_ms()-121_000}),
        )
        .unwrap();
    assert!(runtime::check_health(&store, 120).is_err());
    runtime::status(&store, "degraded", json!({})).unwrap();
    assert!(runtime::check_health(&store, 120).is_err());
    runtime::status(&store, "stopped", json!({})).unwrap();
    assert!(runtime::check_health(&store, 120).is_err());
}

fn row(n: u64, terminal: bool) -> Order {
    Order {
        cloid: format!("0x{n:032x}"),
        oid: Some(n),
        user: "fixture".into(),
        managed_hedge: true,
        submitted_ms: n,
        observed_ms: n,
        wire: json!({"c":format!("0x{n:032x}")}),
        status: if terminal { "filled" } else { "unknown" }.into(),
        terminal,
        exchange: json!({"proof":n}),
    }
}
fn tiny_policy() -> StorageConfig {
    StorageConfig {
        event_segment_bytes: 512,
        event_retained_segments: 2,
        terminal_orders_keep: 1,
        order_archive_max_bytes: 100_000,
        min_free_bytes: 0,
    }
}

#[test]
fn event_rotation_is_bounded_and_segments_remain_parseable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_policy(dir.path(), tiny_policy()).unwrap();
    for n in 0..60 {
        store
            .event("fixture", json!({"n":n,"text":"abcdefghij".repeat(10)}))
            .unwrap();
    }
    let archives: Vec<_> = std::fs::read_dir(dir.path().join("event_archive"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    assert!(archives.len() <= 2);
    for path in archives
        .into_iter()
        .chain([dir.path().join("events.jsonl")])
    {
        assert!(path.metadata().unwrap().len() <= 512);
        for line in std::fs::read_to_string(path).unwrap().lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }
}

#[test]
fn terminal_archive_survives_restart_preserves_unknown_and_prevents_id_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let first = row(1, true);
    {
        let store = Store::open_with_policy(dir.path(), tiny_policy()).unwrap();
        let ledger: Orders = [first.clone(), row(2, true), row(3, false)]
            .into_iter()
            .map(|r| (r.cloid.clone(), r))
            .collect();
        store.write("orders.json", &ledger).unwrap();
        // Crash boundary: archive copied but hot ledger removal was not yet committed.
        store
            .archive_order(&first.cloid, &serde_json::to_value(&first).unwrap())
            .unwrap();
    }
    let store = Store::open_with_policy(dir.path(), tiny_policy()).unwrap();
    journal::compact(&store).unwrap();
    let ledger = store.read::<Orders>("orders.json").unwrap().unwrap();
    assert_eq!(ledger.len(), 2);
    assert!(!ledger.contains_key(&first.cloid));
    assert!(!ledger[&row(3, false).cloid].terminal);
    assert!(store.archived_order(&first.cloid).unwrap().is_some());
    let action = json!({"type":"order","orders":[first.wire]});
    assert!(journal::prepare(&store, &action, "fixture", 100).is_err());
}

#[test]
fn archive_capacity_or_corruption_never_discards_hot_orders() {
    for corrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = tiny_policy();
        if !corrupt {
            policy.order_archive_max_bytes = 1;
        }
        let store = Store::open_with_policy(dir.path(), policy).unwrap();
        let first = row(1, true);
        let ledger: Orders = [first.clone(), row(2, true)]
            .into_iter()
            .map(|r| (r.cloid.clone(), r))
            .collect();
        store.write("orders.json", &ledger).unwrap();
        if corrupt {
            std::fs::create_dir(dir.path().join("order_archive")).unwrap();
            std::fs::write(
                dir.path()
                    .join(format!("order_archive/{}.json", first.cloid)),
                b"broken",
            )
            .unwrap();
        }
        assert!(journal::compact(&store).is_err());
        assert_eq!(
            store.read::<Orders>("orders.json").unwrap().unwrap().len(),
            2
        );
    }
}

#[test]
fn unresolved_operation_prevents_archive_pruning_and_low_disk_prevents_new_intent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_policy(dir.path(), tiny_policy()).unwrap();
    let ledger: Orders = [row(1, true), row(2, true)]
        .into_iter()
        .map(|r| (r.cloid.clone(), r))
        .collect();
    store.write("orders.json", &ledger).unwrap();
    store.begin(json!({"venue":"fixture"})).unwrap();
    journal::compact(&store).unwrap();
    assert_eq!(
        store.read::<Orders>("orders.json").unwrap().unwrap().len(),
        2
    );
    drop(store);
    let mut policy = tiny_policy();
    policy.min_free_bytes = u64::MAX;
    let store = Store::open_with_policy(dir.path(), policy).unwrap();
    store.write("pending.json", &Option::<Value>::None).unwrap();
    assert!(store.begin(json!({"venue":"new"})).is_err());
    assert!(store.pending().unwrap().is_none());
}

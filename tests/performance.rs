use lp_maker::{
    monitor::{
        display,
        performance::{FILE, History, Starts, WINDOW_MS},
    },
    store::Store,
};
use serde_json::{Value, json};

const START: u64 = 1_800_000_000_000;
const OWNER: &str = "0x0000000000000000000000000000000000000001";
const MANAGER: &str = "0x0000000000000000000000000000000000000002";
fn position(id: &str, capital: f64, base_fee: f64, quote_fee: f64) -> Value {
    json!({"token_id":id,"layer":"core","accounting_revision":"stable","principal_value_usdg":capital,
        "unclaimed_base":base_fee,"unclaimed_quote":quote_fee})
}
fn snapshot(seconds: u64, price: f64, positions: Vec<Value>) -> Value {
    json!({"mode":"live","positions_observed":true,
        "accounting_identity":{"chain_id":4663,"pool":"pool","manager":MANAGER,"owner":OWNER},
        "pool":{"time_ms":START+seconds*1000,"block":seconds+1000,"block_hash":format!("hash-{seconds}"),"price":price},"positions":positions})
}
fn observe(h: &mut History, v: &mut Value) {
    h.observe(
        v,
        v["pool"]["time_ms"].as_u64().unwrap(),
        WINDOW_MS,
        &Starts::default(),
    )
    .unwrap();
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-8, "{a} != {b}");
}

#[test]
fn one_hour_actual_fees_annualize_against_time_weighted_principal() {
    let mut h = History::default();
    observe(
        &mut h,
        &mut snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 5.0)]),
    );
    let mut end = snapshot(3600, 2000.0, vec![position("1", 200.0, 0.0, 5.015)]);
    observe(&mut h, &mut end);
    close(end["fee_apr_1h"]["fees_usdg"].as_f64().unwrap(), 0.015);
    close(
        end["fee_apr_1h"]["average_principal_usdg"]
            .as_f64()
            .unwrap(),
        150.0,
    );
    close(end["fee_apr_1h"]["apr_pct"].as_f64().unwrap(), 87.6);
    assert_eq!(end["fee_apr_1h"]["complete"], true);
    assert_eq!(end["positions"][0]["holding"]["seconds"], 3600);
}

#[test]
fn price_appreciation_of_old_weth_fees_is_not_fee_income() {
    let mut h = History::default();
    observe(
        &mut h,
        &mut snapshot(0, 2000.0, vec![position("1", 100.0, 0.01, 0.5)]),
    );
    let mut end = snapshot(3600, 4000.0, vec![position("1", 200.0, 0.01, 0.5)]);
    observe(&mut h, &mut end);
    assert_eq!(end["fee_apr_1h"]["fees_usdg"], 0.0);
    assert_eq!(end["fee_apr_1h"]["apr_pct"], 0.0);
}

#[test]
fn rolling_window_interpolates_boundary_and_drops_old_income() {
    let mut h = History::default();
    for (t, fee) in [(0, 0.0), (1800, 0.1), (3600, 0.12)] {
        observe(
            &mut h,
            &mut snapshot(t, 2000.0, vec![position("1", 100.0, 0.0, fee)]),
        );
    }
    let mut end = snapshot(4500, 2000.0, vec![position("1", 100.0, 0.0, 0.13)]);
    observe(&mut h, &mut end);
    // Window starts at t=900: half of first 0.1 plus 0.02 and 0.01.
    close(end["fee_apr_1h"]["fees_usdg"].as_f64().unwrap(), 0.08);
    assert_eq!(end["fee_apr_1h"]["observed_seconds"], 3600);
    close(end["fee_apr_1h"]["apr_pct"].as_f64().unwrap(), 700.8);
}

#[test]
fn partial_windows_are_labeled_and_sub_minute_samples_do_not_annualize() {
    let mut h = History::default();
    observe(
        &mut h,
        &mut snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 0.0)]),
    );
    let mut short = snapshot(15, 2000.0, vec![position("1", 100.0, 0.0, 0.0001)]);
    observe(&mut h, &mut short);
    assert!(short["fee_apr_1h"]["apr_pct"].is_null());
    let mut half = snapshot(1800, 2000.0, vec![position("1", 100.0, 0.0, 0.005)]);
    observe(&mut h, &mut half);
    assert_eq!(half["fee_apr_1h"]["status"], "partial");
    close(half["fee_apr_1h"]["apr_pct"].as_f64().unwrap(), 87.6);
    let printed = display::robinhood(&json!({"mode":"live","snapshot":half}));
    assert!(printed.contains("不足1小时，仅供参考"));
    assert!(printed.contains("持仓时长：至少 0小时30分00秒"));
}

#[test]
fn multiple_positions_use_common_coverage_and_capital_not_mean_apr() {
    let mut h = History::default();
    observe(
        &mut h,
        &mut snapshot(0, 2000.0, vec![position("1", 80.0, 0.0, 0.0)]),
    );
    observe(
        &mut h,
        &mut snapshot(
            1800,
            2000.0,
            vec![
                position("1", 80.0, 0.0, 0.004),
                position("2", 40.0, 0.0, 0.0),
            ],
        ),
    );
    let mut end = snapshot(
        3600,
        2000.0,
        vec![
            position("1", 80.0, 0.0, 0.008),
            position("2", 40.0, 0.0, 0.004),
        ],
    );
    observe(&mut h, &mut end);
    assert_eq!(end["fee_apr_1h"]["observed_seconds"], 1800);
    close(end["fee_apr_1h"]["fees_usdg"].as_f64().unwrap(), 0.008);
    close(end["fee_apr_1h"]["apr_pct"].as_f64().unwrap(), 116.8);
}

#[test]
fn collection_liquidity_change_counter_drop_and_reorg_break_fee_comparability() {
    for reason in ["revision", "counter", "reorg"] {
        let mut h = History::default();
        observe(
            &mut h,
            &mut snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 0.01)]),
        );
        let mut after = snapshot(60, 2000.0, vec![position("1", 100.0, 0.0, 0.02)]);
        match reason {
            "revision" => {
                after["positions"][0]["accounting_revision"] = json!("collected-or-increased")
            }
            "counter" => after["positions"][0]["unclaimed_quote"] = json!(0.0),
            _ => after["accounting_reorg"] = json!(true),
        }
        observe(&mut h, &mut after);
        assert!(after["fee_apr_1h"]["apr_pct"].is_null());
        assert_eq!(after["positions"][0]["holding"]["seconds"], 60);
        assert_eq!(h.positions["1"].samples.len(), 1);
    }
}

#[test]
fn restart_preserves_samples_and_holding_duration_but_long_gaps_restart_apr() {
    let temp = tempfile::tempdir().unwrap();
    let mut h = History::default();
    observe(
        &mut h,
        &mut snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 0.0)]),
    );
    observe(
        &mut h,
        &mut snapshot(30, 2000.0, vec![position("1", 100.0, 0.0, 0.0001)]),
    );
    {
        let store = Store::open(temp.path()).unwrap();
        store.write(FILE, &h).unwrap();
    }
    let mut loaded: History = Store::readonly(temp.path()).read(FILE).unwrap().unwrap();
    loaded.validate().unwrap();
    let mut after = snapshot(60, 2000.0, vec![position("1", 100.0, 0.0, 0.0002)]);
    loaded
        .observe(&mut after, START + 60_000, 60_000, &Starts::default())
        .unwrap();
    assert!(after["fee_apr_1h"]["apr_pct"].is_number());
    assert_eq!(after["positions"][0]["holding"]["seconds"], 60);
    let mut later = snapshot(600, 2000.0, vec![position("1", 100.0, 0.0, 0.002)]);
    loaded
        .observe(&mut later, START + 600_000, 60_000, &Starts::default())
        .unwrap();
    assert!(later["fee_apr_1h"]["apr_pct"].is_null());
    assert_eq!(later["positions"][0]["holding"]["seconds"], 600);
    assert_eq!(
        later["positions"][0]["fee_apr_1h"]["last_reset_reason"],
        "observation_gap"
    );
}

#[test]
fn repeated_blocks_unknown_positions_and_stale_observations_do_not_create_samples() {
    let mut h = History::default();
    let mut v = snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 0.0)]);
    observe(&mut h, &mut v);
    observe(&mut h, &mut v);
    assert_eq!(h.positions["1"].samples.len(), 1);
    assert!(
        h.observe(&mut v, START + 60_001, 60_000, &Starts::default())
            .is_err()
    );
    let mut unknown = snapshot(10, 2000.0, vec![]);
    unknown["positions_observed"] = json!(false);
    observe(&mut h, &mut unknown);
    assert_eq!(h.positions.len(), 1);
    let mut closed = snapshot(10, 2000.0, vec![]);
    observe(&mut h, &mut closed);
    assert!(h.positions.is_empty());
    observe(
        &mut h,
        &mut snapshot(20, 2000.0, vec![position("2", 100.0, 0.0, 0.0)]),
    );
    assert_eq!(h.positions["2"].since_ms, START + 20_000);
}

#[test]
fn confirmed_mint_journal_migrates_start_time_without_inventing_old_fee_samples() {
    use alloy::primitives::keccak256;
    let temp = tempfile::tempdir().unwrap();
    let receipt = json!({"kind":"operation_result","time_ms":START-3_600_000,"data":{"status":"0x1","logs":[
        {"address":MANAGER,"topics":[format!("{:#x}",keccak256("Transfer(address,address,uint256)")),
            format!("0x{}","0".repeat(64)),format!("0x{:0>64}",&OWNER[2..]),format!("0x{:064x}",1)]}
    ]}});
    std::fs::write(
        temp.path().join("events.jsonl"),
        format!("{receipt}\n{{truncated-record"),
    )
    .unwrap();
    let starts = Starts::load(temp.path()).unwrap();
    let mut h = History::default();
    let mut v = snapshot(0, 2000.0, vec![position("1", 100.0, 0.0, 0.5)]);
    h.observe(&mut v, START, 60_000, &starts).unwrap();
    assert_eq!(v["positions"][0]["holding"]["seconds"], 3600);
    assert_eq!(v["positions"][0]["holding"]["source"], "confirmed_mint");
    assert!(v["fee_apr_1h"]["apr_pct"].is_null());
    assert_eq!(v["fee_apr_1h"]["observed_seconds"], 0);
}

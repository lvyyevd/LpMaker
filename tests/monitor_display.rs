use lp_maker::monitor::display::{hyperliquid, robinhood};
use serde_json::json;

#[test]
fn connected_ws_and_old_empty_snapshot_do_not_claim_current_flat_inventory() {
    let report = json!({
        "mode":"live", "max_data_age_seconds":60, "snapshot_age_ms":858000,
        "ws":{"connected":true}, "ws_data_age_ms":100,
        "rpc_cooldown_seconds":120, "history_rpc_cooldown_seconds":0,
        "runtime":{"status":"degraded"},
        "snapshot":{"positions":[], "strategy":{"phase":"Active"}},
        "last_refresh_error":"429"
    });
    let rendered = robinhood(&report);
    assert!(rendered.contains("当前链上仓位待核对"));
    assert!(rendered.contains("上次观察时未发现仓位"));
    assert!(rendered.contains("上次策略状态：正常运行"));
    assert!(rendered.contains("主 RPC：节点限流，约 120 秒后允许重试"));
    assert!(rendered.contains("当前未恢复正常决策"));
    assert!(!rendered.contains("LP 仓位：未发现仓位"));
}

#[test]
fn unknown_account_is_distinct_from_confirmed_zero_and_uses_unified_collateral() {
    let unknown = hyperliquid(&json!({"account_configured":true}));
    assert!(unknown.contains("真实合约持仓：待获取"));
    assert!(unknown.contains("等待订单快照"));
    assert!(!unknown.contains("真实合约持仓：无"));

    let report = json!({
        "time_ms":100000, "account_configured":true,
        "collateral":{"account_mode":"unifiedAccount","coin":"ETH","equity_usdc":79.6,"available_short_usdc":59.7},
        "account":{"state":{"marginSummary":{"accountValue":"0"},"assetPositions":[
            {"position":{"coin":"ETH","szi":"-0.0001","positionValue":"0.2485","entryPx":"2485.6","marginUsed":"0.0828","unrealizedPnl":"0.000166","leverage":{"type":"isolated","value":3}}}
        ]}},
        "account_ws":{"openOrders":{"observed_ms":99500,"data":{"orders":[]}}}
    });
    let rendered = hyperliquid(&report);
    assert!(rendered.contains("已核对 USDC 权益：79.6000"));
    assert!(rendered.contains("可用于 ETH 开空：59.7000"));
    assert!(rendered.contains("ETH 空单 0.00010000"));
    assert!(rendered.contains("当前挂单：0 笔"));
    assert!(!rendered.contains("真实合约持仓：无"));
}

#[test]
fn hedge_residual_preserves_direction_and_age_without_claiming_a_loss() {
    let mut report = json!({
        "time_ms":100000, "max_data_age_seconds":60,
        "hedge_residual":{
            "observed_ms":99000, "coin":"ETH",
            "target":0.000052666271088176, "actual_short":0.0001,
            "residual_base":-0.000047333728911824, "residual_usd":0.11756988255763406,
            "reason":"within_hedge_deadband"
        }
    });
    let rendered = hyperliquid(&report);
    assert!(rendered.contains("空单多于目标 0.00004733 ETH，约 0.1176 USD"));
    assert!(rendered.contains("敞口差额，非盈亏"));
    assert!(rendered.contains("当时目标空单 0.00005267｜实际空单 0.00010000"));
    assert!(rendered.contains("允许偏差内，暂不调整"));
    assert!(rendered.contains("观察：1.0 秒前"));

    report["time_ms"] = json!(200000);
    assert!(hyperliquid(&report).contains("观察：101.0 秒前（已过期）"));
    report["hedge_residual"]["target"] = json!(0.02);
    report["hedge_residual"]["actual_short"] = json!(0.01);
    report["hedge_residual"]["residual_base"] = json!(0.01);
    report["hedge_residual"]["residual_usd"] = json!(24.85);
    report["hedge_residual"]["reason"] = json!("confirmed_execution_residual");
    let rendered = hyperliquid(&report);
    assert!(rendered.contains("空单少于目标 0.01000000 ETH，约 24.8500 USD"));
    assert!(!rendered.contains("允许偏差内"));

    report["hedge_residual"] = json!(null);
    assert!(!hyperliquid(&report).contains("对冲余量"));
}

#[test]
fn lp_fees_and_equity_change_keep_distinct_accounting_and_missing_values() {
    let mut report = json!({
        "mode":"live", "time_ms":1789711566959_u64,
        "max_data_age_seconds":60, "snapshot_age_ms":14827,
        "snapshot":{
            "pool":{"price":2484.7440269522854},
            "positions":[
                {"layer":"core","token_id":"1214297","lower":2299.4278949146887,"upper":2682.241032263312,
                 "price_position":{"status":"in_range","fraction":0.4840903144576033},
                 "principal_value_usdg":79.37366133457903,"unclaimed_fees_usdg":0.0009046169745452991},
                {"layer":"satellite","token_id":"1214301","lower":2434.5391938229777,"upper":2533.129540819557,
                 "price_position":{"status":"in_range","fraction":0.5092266602028451},
                 "principal_value_usdg":39.68306473402875,"unclaimed_fees_usdg":0.0017212150050350448}
            ],
            "returns":{"baseline_comparable":true,"equity_usd":388.36943570083446,"equity_change_usd":-0.049809299165644916},
            "strategy":{"entry_history":"established","phase":"Active","layers":{"core":{"protected":false},"satellite":{"protected":false}}}
        },
        "recent_volume":{"window_seconds":300,"volume_usdg":1343490.9273050036,"volume_weth":540.9726294984538,"swap_count":1306,"complete":true}
    });
    let rendered = robinhood(&report);
    assert!(rendered.contains("LP 合计：本金市值 119.0567 USDG｜待领取手续费 0.002626 USDG"));
    assert!(rendered.contains("相对基准变动：-0.0498 USD"));
    assert!(rendered.contains("不等于净利润"));
    assert!(!rendered.contains("池子成交量"));
    assert!(rendered.contains("已建仓，历史已保存"));

    report["snapshot"]["positions"][0]["unclaimed_fees_usdg"] = json!(null);
    report["snapshot"]["returns"]["baseline_comparable"] = json!(false);
    report["recent_volume"]["complete"] = json!(false);
    report["snapshot_age_ms"] = json!(60001);
    report["last_refresh_error"] = json!("RPC timeout");
    let rendered = robinhood(&report);
    assert!(rendered.contains("LP 合计：本金市值 119.0567 USDG｜待领取手续费 待获取"));
    assert!(rendered.contains("不可比较（账户资金口径已变化）"));
    assert!(!rendered.contains("-0.0498 USD"));
    assert!(!rendered.contains("成交量"));
    assert!(rendered.contains("已过期"));
    assert!(rendered.contains("RPC timeout"));

    report["mode"] = json!("paper");
    assert!(robinhood(&report).contains("待领取手续费 未模拟"));

    // A missing LP owner/strategy cache is not evidence of an empty account.
    report["mode"] = json!("live");
    report["snapshot"]["positions"] = json!([]);
    report["snapshot"]["positions_observed"] = json!(false);
    let unknown = robinhood(&report);
    assert!(unknown.contains("LP 仓位：待获取"));
    assert!(!unknown.contains("LP 合计："));
    report["snapshot"]["positions_observed"] = json!(true);
    let empty = robinhood(&report);
    assert!(empty.contains("本金市值 0.0000 USDG｜待领取手续费 0.000000 USDG"));
}

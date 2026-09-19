//! Read-only presentation. Missing observations must never look like zero balances/profit.
use super::denomination::value;
use serde_json::Value;

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n: &f64| n.is_finite())
}

fn amount(n: Option<f64>, precision: usize) -> String {
    let Some(n) = n.filter(|n| n.is_finite()) else {
        return "待获取".into();
    };
    // An empty f64 sum can be -0.0; this is not a negative balance or fee.
    let n = if n == 0.0 { 0.0 } else { n };
    let s = format!("{n:.precision$}");
    let (whole, fraction) = s.split_once('.').unwrap_or((&s, ""));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0
            && c != '-'
            && whole.as_bytes()[i - 1] != b'-'
            && (whole.len() - i).is_multiple_of(3)
        {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if !fraction.is_empty() {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

fn n(v: &Value, precision: usize) -> String {
    amount(number(v), precision)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("待获取")
}

fn age(ms: Option<u64>, report: &Value) -> String {
    ms.map_or_else(
        || "待获取".into(),
        |ms| {
            let stale = report["max_data_age_seconds"]
                .as_u64()
                .is_some_and(|limit| ms > limit.saturating_mul(1000));
            format!(
                "{:.1} 秒前{}",
                ms as f64 / 1000.0,
                if stale { "（已过期）" } else { "" }
            )
        },
    )
}

fn observed_age(time: &Value, report: &Value) -> String {
    age(
        time.as_u64()
            .zip(report["time_ms"].as_u64())
            .map(|(t, now)| now.saturating_sub(t)),
        report,
    )
}

fn connection(report: &Value) -> String {
    format!(
        "连接：{}｜最近消息：{}",
        if report["ws"]["connected"] == true {
            "已连接"
        } else {
            "未连接／重连中"
        },
        age(report["ws_data_age_ms"].as_u64(), report)
    )
}

fn refresh_notes(lines: &mut Vec<String>, report: &Value) {
    if report["refresh_pending"] == true {
        lines.push("快照正在刷新，以上为上次观察值。".into());
    }
    if let Some(error) = report["last_refresh_error"].as_str() {
        lines.push(format!("快照刷新失败（旧数据保留）：{error}"));
    }
}

fn phase(v: &Value) -> &str {
    match v.as_str() {
        Some("Active") => "正常运行",
        Some("Warmup") => "准备建仓",
        Some("Paused") => "暂停，等待恢复条件",
        Some("Recovering") => "分阶段恢复仓位",
        Some("Halted") => "风控停止新增仓位",
        _ => "待获取",
    }
}

fn duration(seconds: u64) -> String {
    format!(
        "{}小时{:02}分{:02}秒",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn fee_apr(apr: &Value, paper: bool, stale: bool, quote: &str) -> String {
    let label = "近1小时手续费 APR";
    if paper {
        return format!("{label}：未模拟 LP 手续费");
    }
    if stale {
        return format!("{label}：暂不可用（快照过期或刷新失败）");
    }
    let observed = duration(apr["observed_seconds"].as_u64().unwrap_or(0));
    let Some(pct) = number(&apr["apr_pct"]) else {
        return format!("{label}：待积累至少1分钟有效样本（已观察 {observed}）");
    };
    let coverage = if apr["complete"] == true {
        "完整1小时".into()
    } else {
        format!("仅观察 {observed}，不足1小时，仅供参考")
    };
    format!(
        "{label}：{pct:.2}%｜窗口新增手续费：{} {quote}｜平均LP本金：{} {quote}｜{coverage}",
        n(value(apr, "fees_quote", "fees_usdg"), 6),
        n(
            value(apr, "average_principal_quote", "average_principal_usdg"),
            4
        )
    )
}

/// 兼容旧调用者；有 market 元数据时按实际链/币种打印。
pub fn robinhood(report: &Value) -> String {
    liquidity(report)
}

pub fn liquidity(report: &Value) -> String {
    // 缺少标签的旧日志仍按 Robinhood 解释；新版运行器始终提供标签。
    let market = &report["market"];
    let chain = market["chain"].as_str().unwrap_or("Robinhood");
    let base = market["base_symbol"].as_str().unwrap_or("WETH");
    let quote = market["quote_symbol"].as_str().unwrap_or("USDG");
    let price_symbol = market["price_symbol"].as_str().unwrap_or("ETH");
    let snapshot = &report["snapshot"];
    let pool = &snapshot["pool"];
    let strategy = &snapshot["strategy"];
    let paper = report["mode"] == "paper";
    let apr_stale = report["snapshot_age_ms"]
        .as_u64()
        .zip(report["max_data_age_seconds"].as_u64())
        .is_some_and(|(age, limit)| age > limit.saturating_mul(1000))
        || report["last_refresh_error"].is_string()
        || snapshot["performance_error"].is_string();
    let mut lines = vec![
        format!(
            "【{chain} LP 状态｜{}】",
            if paper { "模拟" } else { "实盘" }
        ),
        format!(
            "{}｜LP 快照：{}",
            connection(report),
            age(report["snapshot_age_ms"].as_u64(), report)
        ),
        format!(
            "{price_symbol} 价格：{} {quote}｜策略：{}｜建仓历史：{}",
            n(&pool["price"], 2),
            phase(&strategy["phase"]),
            match strategy["entry_history"].as_str() {
                Some("established") => "已建仓，历史已保存",
                Some("initial") => "首次建仓阶段",
                Some("legacy_unknown") => "旧状态，待核对",
                _ => "待获取",
            }
        ),
    ];
    match snapshot["positions"]
        .as_array()
        .filter(|_| snapshot["positions_observed"] != false)
    {
        None => lines.push("LP 仓位：待获取，不能视为零仓位".into()),
        Some(positions) => {
            if positions.is_empty() {
                lines.push("LP 仓位：未发现仓位".into());
            }
            for pos in positions {
                let layer = text(&pos["layer"]);
                let name = match layer {
                    "core" => "主区间",
                    "satellite" => "辅助区间",
                    _ => layer,
                };
                let state = match pos["price_position"]["status"].as_str() {
                    Some("in_range") => "区间内",
                    Some("below") => "已跌破下沿",
                    Some("above") => "已到达或突破上沿",
                    _ => "位置待确认",
                };
                let protected = match strategy["layers"][layer]["protected"].as_bool() {
                    Some(true) => "已触发（成交另见 Hyperliquid）",
                    Some(false) => "未触发",
                    None => "待确认",
                };
                lines.push(format!(
                    "  {name}｜NFT {}｜{state}｜下沿保护信号：{protected}",
                    text(&pos["token_id"])
                ));
                lines.push(format!(
                    "    区间：{} ～ {} {quote}｜价格位置：{}%（下沿 0%，上沿 100%）",
                    n(&pos["lower"], 2),
                    n(&pos["upper"], 2),
                    amount(
                        number(&pos["price_position"]["fraction"]).map(|v| v * 100.0),
                        2
                    )
                ));
                lines.push(format!(
                    "    本金市值：{} {quote}｜待领取手续费：{}",
                    n(
                        value(pos, "principal_value_quote", "principal_value_usdg"),
                        4
                    ),
                    if paper {
                        "未模拟".into()
                    } else {
                        format!(
                            "{} {quote}",
                            n(value(pos, "unclaimed_fees_quote", "unclaimed_fees_usdg"), 6)
                        )
                    }
                ));
                let held = p_holding(pos);
                lines.push(format!("    {held}"));
                lines.push(format!(
                    "    {}",
                    fee_apr(&pos["fee_apr_1h"], paper, apr_stale, quote)
                ));
                if pos["fee_apr_1h"]["complete"] != true
                    && let Some(reason) = pos["fee_apr_1h"]["last_reset_reason"].as_str()
                {
                    lines.push(format!(
                        "    采样重新开始：{}",
                        match reason {
                            "position_operated" => "检测到领取手续费或流动性调整",
                            "observation_gap" => "观察中断，缺少连续本金样本",
                            "chain_changed" => "链上区块发生变化",
                            _ => "手续费计数不连续",
                        }
                    ));
                }
            }
            // A missing fee/value invalidates the aggregate; do not silently sum only known rows.
            let total = |key: &str, legacy: &str| {
                positions
                    .iter()
                    .map(|p| number(value(p, key, legacy)))
                    .sum::<Option<f64>>()
            };
            lines.push(format!(
                "LP 合计：本金市值 {} {quote}｜待领取手续费 {}",
                amount(total("principal_value_quote", "principal_value_usdg"), 4),
                if paper {
                    "未模拟".into()
                } else {
                    format!(
                        "{} {quote}",
                        amount(total("unclaimed_fees_quote", "unclaimed_fees_usdg"), 6)
                    )
                }
            ));
            if !positions.is_empty() {
                lines.push(format!(
                    "当前LP合计｜{}",
                    fee_apr(&snapshot["fee_apr_1h"], paper, apr_stale, quote)
                ));
            }
        }
    }
    let volume = &report["recent_volume"];
    if volume.is_null() {
        lines.push("近期池子成交量：待获取已确认数据".into());
    } else {
        lines.push(format!(
            "近 {} 秒池子成交量：{} {quote} / {} {base}｜{} 笔｜{}｜统计更新：{}",
            n(&volume["window_seconds"], 0),
            n(value(volume, "volume_quote", "volume_usdg"), 2),
            n(value(volume, "volume_base", "volume_weth"), 4),
            n(&volume["swap_count"], 0),
            match volume["complete"].as_bool() {
                Some(true) => "已完整核对",
                Some(false) => "数据不完整，仍在补齐",
                None => "待获取",
            },
            age(report["volume_age_ms"].as_u64(), report)
        ));
    }
    let returns = &snapshot["returns"];
    let change = if returns["baseline_comparable"] == false {
        "不可比较（账户资金口径已变化）".into()
    } else {
        let key = if paper {
            "pnl_usd"
        } else {
            "equity_change_usd"
        };
        number(&returns[key]).map_or_else(
            || "不可用（等待可比较的基准）".into(),
            |v| format!("{v:+.4} USD"),
        )
    };
    lines.push(format!(
        "组合净值：{} USD｜相对基准变动：{change}｜净值更新：{}",
        n(&returns["equity_usd"], 4),
        observed_age(&returns["observed_ms"], report)
    ));
    lines.push(if paper { "收益口径：模拟结果，未模拟 LP 手续费与 Gas。".into() }
        else { "收益口径：组合净值变动包含待领手续费，未扣 Gas、未校正出入金，不等于净利润；池子成交量不是个人收益。".into() });
    if !paper {
        lines.push("APR口径：按窗口内实际手续费代币增量和时间加权LP本金估算单利年化；不含币价损益、Gas、对冲费用和资金费，不代表未来收益。".into());
    }
    if let Some(error) = snapshot["performance_error"].as_str() {
        lines.push(format!("手续费 APR 计算暂停：{error}"));
    }
    if report["volume_refresh_pending"] == true {
        lines.push("成交量正在后台补齐。".into());
    }
    if let Some(error) = report["volume_error"].as_str() {
        lines.push(format!("成交量刷新失败：{error}"));
    }
    refresh_notes(&mut lines, report);
    lines.join("\n")
}

fn p_holding(pos: &Value) -> String {
    match pos["holding"]["seconds"].as_u64() {
        Some(seconds) => format!(
            "持仓时长：至少 {}（{}）",
            duration(seconds),
            if pos["holding"]["source"] == "confirmed_mint" {
                "从本地建仓确认记录起算"
            } else {
                "从首次观察起算，原建仓时间未知"
            }
        ),
        None => "持仓时长：待获取".into(),
    }
}

pub fn hyperliquid(report: &Value) -> String {
    let mut lines = vec![
        "【Hyperliquid 账户与对冲】".into(),
        format!(
            "{}｜账户快照：{}",
            connection(report),
            age(report["account_age_ms"].as_u64(), report)
        ),
    ];
    if let Some(prices) = report["prices"].as_object().filter(|p| !p.is_empty()) {
        lines.push(format!(
            "行情：{}",
            prices
                .iter()
                .map(|(coin, p)| format!(
                    "{coin} {} USD（{}）",
                    n(&p["price"], 2),
                    observed_age(&p["received_ms"], report)
                ))
                .collect::<Vec<_>>()
                .join("｜")
        ));
    } else {
        lines.push("行情：等待 WebSocket 数据".into());
    }
    if report["account_configured"] == false {
        lines.push("真实账户：未配置地址，余额和持仓未知".into());
    } else {
        let collateral = &report["collateral"];
        if collateral.is_object() {
            lines.push(format!(
                "资金模式：{}｜已核对 USDC 权益：{}｜可用于 {} 开空：{} USDC",
                match collateral["account_mode"].as_str() {
                    Some("unifiedAccount") => "统一账户",
                    Some("disabled" | "default") => "标准账户",
                    Some("dexAbstraction") => "DEX 抽象账户",
                    _ => "待确认",
                },
                n(&collateral["equity_usdc"], 4),
                text(&collateral["coin"]),
                n(&collateral["available_short_usdc"], 4)
            ));
        } else {
            lines.push("账户资金：等待账户模式与保证金核对，不能将原始永续余额当作可用资金".into());
        }
        match report["account"]["state"]["assetPositions"].as_array() {
            None => lines.push("真实合约持仓：待获取".into()),
            Some(positions) => {
                let mut count = 0;
                for p in positions.iter().map(|v| &v["position"]) {
                    let size = number(&p["szi"]);
                    if size == Some(0.0) {
                        continue;
                    }
                    count += 1;
                    let direction = match size {
                        Some(v) if v < 0.0 => "空单",
                        Some(_) => "多单",
                        None => "方向待确认",
                    };
                    lines.push(format!(
                        "  {} {direction} {}｜{}倍{}｜名义价值 {} USD",
                        text(&p["coin"]),
                        amount(size.map(f64::abs), 8),
                        n(&p["leverage"]["value"], 0),
                        match p["leverage"]["type"].as_str() {
                            Some("isolated") => "逐仓",
                            Some("cross") => "全仓",
                            _ => "（模式待确认）",
                        },
                        n(&p["positionValue"], 4)
                    ));
                    lines.push(format!(
                        "    开仓均价 {}｜未实现盈亏 {} USD｜占用保证金 {} USDC",
                        n(&p["entryPx"], 2),
                        n(&p["unrealizedPnl"], 6),
                        n(&p["marginUsed"], 4)
                    ));
                }
                if count == 0 {
                    lines.push("真实合约持仓：无".into());
                }
            }
        }
        let orders_ws = &report["account_ws"]["openOrders"];
        match orders_ws["data"]["orders"].as_array() {
            Some(orders) => {
                lines.push(format!(
                    "当前挂单：{} 笔（快照：{}）",
                    orders.len(),
                    observed_age(&orders_ws["observed_ms"], report)
                ));
                for o in orders {
                    lines.push(format!(
                        "  {} {} {}｜限价 {}｜订单 {}",
                        text(&o["coin"]),
                        match o["side"].as_str() {
                            Some("B") => "买入",
                            Some("S") => "卖出",
                            _ => "方向未知",
                        },
                        n(&o["sz"], 8),
                        n(&o["limitPx"], 2),
                        o["oid"]
                    ));
                }
            }
            None => lines.push("当前挂单：等待订单快照，不能视为零挂单".into()),
        }
    }
    let residual = &report["hedge_residual"];
    if residual.is_object() {
        lines.push(format!(
            "对冲余量（上次核对）：{} {} {}，约 {} USD（敞口差额，非盈亏）｜观察：{}",
            match number(&residual["residual_base"]) {
                Some(v) if v < 0.0 => "空单多于目标",
                Some(v) if v > 0.0 => "空单少于目标",
                Some(_) => "空单与目标一致",
                None => "方向待确认",
            },
            amount(number(&residual["residual_base"]).map(f64::abs), 8),
            text(&residual["coin"]),
            n(&residual["residual_usd"], 4),
            observed_age(&residual["observed_ms"], report)
        ));
        lines.push(format!(
            "  当时目标空单 {}｜实际空单 {}｜原因：{}",
            n(&residual["target"], 8),
            n(&residual["actual_short"], 8),
            match residual["reason"].as_str() {
                Some("within_hedge_deadband") => "允许偏差内，暂不调整",
                Some("below_opening_minimum_or_lot_precision") => "受最小下单金额或数量精度限制",
                Some("partial_fill_dust_deferred") => "部分成交后余量受下单限制，待再次核对",
                Some("confirmed_execution_residual") => "成交核对后仍有余量，待下次调整",
                _ => "待核对",
            }
        ));
    }
    if report["paper_position"].is_object() {
        let p = &report["paper_position"];
        lines.push(format!(
            "模拟账户（与上方真实账户分开）：{} 空单 {}｜模拟权益 {} USD｜模拟挂单 {}",
            text(&p["coin"]),
            n(&p["short_base"], 8),
            n(&p["equity"], 4),
            if p["pending_order"].is_null() {
                "无"
            } else {
                "有"
            }
        ));
    }
    refresh_notes(&mut lines, report);
    lines.join("\n")
}

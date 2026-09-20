#!/usr/bin/env python3
"""Render a local training review from recorded Rust outcomes; never rerank on test returns."""
import argparse
import csv
import datetime as dt
import hashlib
import json
import pathlib


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--training", type=pathlib.Path, required=True)
    args = parser.parse_args()
    folder = args.training / "review"
    review = json.loads((folder / "review.json").read_text())
    fine = json.loads((folder / "active-fine.json").read_text())
    progress = json.loads((args.training / "progress.json").read_text())
    record = review["active_candidate"]
    full = review["hourly_full"]
    capital = review["capital"]
    params = record["trial"]["parameters"]
    gross = full["price_and_hedge_pnl"] + full["hedge_cost"] + full["swap_cost"] + full["operation_cost"] - full["funding"]
    parts = {
        "假设 LP 手续费": full["lp_fees"],
        "LP 库存与合约价格损益（扣执行成本前）": gross,
        "历史资金费": full["funding"],
        "原生 SOL 储备价格损益": full["native_reserve_pnl"],
        "LP 换币成本": -full["swap_cost"],
        "对冲交易成本": -full["hedge_cost"],
        "LP 操作成本": -full["operation_cost"],
        "原生储备双向换币成本": -full["native_reserve_cost"],
    }
    assert abs(sum(parts.values()) - full["pnl"]) < 1e-8
    cases = [
        ("小时主模型", full),
        ("小时：双倍执行成本", review["hourly_double_cost"]),
        ("小时：LP APR 30%", review["hourly_apr_30"]),
        ("小时：LP APR 0%", review["hourly_zero_apr"]),
        ("5分钟模型（含合约代理价格）", fine["full"]),
        ("5分钟不利低价路径", fine["low_first_stress"]),
        ("5分钟不利路径 + 双倍执行成本", fine["double_cost_low_first"]),
    ]
    with (folder / "scenarios.csv").open("w", newline="") as file:
        writer = csv.writer(file)
        writer.writerow(["scenario", "pnl_usd", "return_pct", "max_drawdown_pct", "lp_fees_usd", "active_hours", "native_reserve_pnl_usd"])
        for label, o in cases:
            writer.writerow([label, o["pnl"], o["pnl"] / capital * 100, o["max_drawdown_pct"], o["lp_fees"], o["active_hours"], o["native_reserve_pnl"]])

    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.dates as dates
    import matplotlib.font_manager as fonts
    font = next((pathlib.Path(name) for name in [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    ] if pathlib.Path(name).exists()), None)
    if font is not None:
        fonts.fontManager.addfont(str(font))
        plt.rcParams["font.family"] = fonts.FontProperties(fname=str(font)).get_name()
    plt.rcParams["axes.unicode_minus"] = False
    x = [dt.datetime.fromtimestamp(p["time_ms"] / 1000, dt.timezone.utc) for p in full["equity_curve"]]
    fig, axes = plt.subplots(2, 1, figsize=(12, 7), sharex=True, constrained_layout=True)
    axes[0].plot(x, [p["price"] for p in full["equity_curve"]], color="#334155", lw=1.3)
    axes[0].fill_between(x, 0, 1, where=[p["lp_value"] > 0 for p in full["equity_curve"]], transform=axes[0].get_xaxis_transform(), color="#14b8a6", alpha=0.16, label="LP 在场时段")
    axes[0].set_ylabel("SOL 池价（USDC）")
    axes[0].legend(loc="upper left", frameon=False)
    axes[1].plot(x, [(p["equity"] / capital - 1) * 100 for p in full["equity_curve"]], label="小时主模型", color="#2563eb", lw=1.5)
    fy = fine["full"]["equity_curve"]
    axes[1].plot([dt.datetime.fromtimestamp(p["time_ms"] / 1000, dt.timezone.utc) for p in fy], [(p["equity"] / capital - 1) * 100 for p in fy], label="5分钟模型（部分合约价为代理）", color="#d97706", alpha=0.85, lw=1)
    axes[1].axhline(0, color="#94a3b8", lw=0.7)
    axes[1].set_ylabel("总账户累计收益（%）")
    axes[1].legend(loc="upper left", frameon=False)
    axes[1].xaxis.set_major_formatter(dates.DateFormatter("%m-%d", tz=dt.timezone.utc))
    axes[1].set_xlabel("2026 年（UTC）；终点已计平仓成本")
    for axis in axes:
        axis.grid(alpha=0.16)
        axis.spines[["top", "right"]].set_visible(False)
    fig.suptitle(f"开发期评分靠前的活跃 LP 候选：半年 +{full['pnl']/capital*100:.2f}%，未达 25% 目标", fontsize=15)
    fig.savefig(folder / "equity-review.png", dpi=170)
    plt.close(fig)

    scenario_rows = "\n".join(f"| {label} | {o['pnl']:+.4f} | {o['pnl']/capital*100:+.2f}% | {o['max_drawdown_pct']:.2f}% | {o['lp_fees']:.4f} |" for label, o in cases)
    breakdown_rows = "\n".join(f"| {label} | {value:+.6f} |" for label, value in parts.items())
    r = params["regime"]
    binary_hash = hashlib.sha256((args.training / "research-binary").read_bytes()).hexdigest()
    report = f"""# SOL 半年 25% 目标：10 小时训练复核

**目标未达成。** 累计训练 {progress['compute_seconds']/3600:.6f} 小时，完成 {review['completed']:,} 组参数回测；另有 {progress['rejected_budget']:,} 组因资本不足剔除。
在 {review['active_time_eligible']:,} 组满足训练期在场至少 100 小时、验证期至少 24 小时的候选中，**训练期和验证期均盈利的组数为 {review['positive_train_and_validation']}，全部开发条件通过的组数为 {review['development_passed']}**。

这表示本次搜索空间和模型下没有找到合格候选，不是对所有 LP 策略作不可能盈利的证明。参数重复抽样有可能发生，组数表示完成的回放组数，不能称为穷举了同等数量的独立策略。

## 目标、数据及选择方法

- 总本金 {capital:.0f} 美元，目标净赚 {capital*progress['target_return']:.0f} 美元；固定假设 LP 单利 APR 40%，合约杠杆 3 倍，回撤限制 5%。
- 时间为 2026-03-18 08:00 UTC 至 2026-09-18 08:00 UTC，184 天。完整池价小时线 4,416 根，对应历史资金费完整；行情来源为 Meteora DLMM 池子公开接口及 Hyperliquid 公开接口。
- 前 92 天训练、之后 30 天参与验证和评分。最后 62 天不参与本轮排名，但此前研究已查看过，不能称为盲测。
- 本报告从实际做过 LP 且满足最低在场时间的候选里，按原开发期评分选出第 {record['index']} 号作诊断复核。**没有用整段半年收益重新挑赢家，因此下方数字不是全部候选的最高半年收益。**
- 自动保存的 `best-paper.toml` 对应长期空仓的评分候选：半年约 +1.19 美元来自原生 SOL 储备，LP 手续费为 0；它不是合格 LP 策略，不应部署。

## 活跃候选的实际模型表现

小时模型半年净利润 **{full['pnl']:+.4f} 美元（{full['pnl']/capital*100:+.2f}%）**，期末净值 {capital+full['pnl']:.4f} 美元；距离目标仍差 {capital*progress['target_return']-full['pnl']:.4f} 美元。
最大模型回撤 {full['max_drawdown_pct']:.2f}%。前 92 天训练期净损益 {record['train']['pnl']:+.4f} 美元，随后 30 天独立起仓验证期 {record['validation']['pnl']:+.4f} 美元；训练期亏损，因此没有入选。

最后 62 天独立起仓复核为 {review['hourly_later_62_days']['pnl']:+.4f} 美元，**该段 LP 在场时间为 0，手续费为 0，收益全部来自原生 SOL 储备**，不能用来证明 LP 在后续时段盈利。各独立测试都从初始资本起步，不能直接把分段损益相加当作连续半年损益。

![六个月价格及账户净值](equity-review.png)

| 情景 | 净损益 USD | 总账户收益率 | 最大回撤 | LP 手续费 USD |
|---|---:|---:|---:|---:|
{scenario_rows}

## 收益拆分：手续费已经计入

| 项目 | USD |
|---|---:|
{breakdown_rows}
| 合计净损益 | {full['pnl']:+.6f} |

拆分合计与 Rust 账本误差小于 1e-8 美元，避免重复扣费。上述“价格损益”已经与原生储备及资金费分开。

LP 半年实际在场仅 {full['active_hours']:.0f} 小时，占 4,416 小时的 {full['active_hours']/4416*100:.2f}%；满足整根小时线在区间内的计费时间为 {full['eligible_fee_hours']:.0f} 小时。
所以固定 40% APR 最终只产生 {full['lp_fees']:.4f} 美元假设手续费，不能对 200 美元总账户无条件算满半年利息。
原生储备的毛价格收益为 {full['native_reserve_pnl']:.4f} 美元，扣其换币成本后约 {full['native_reserve_pnl']-full['native_reserve_cost']:.4f} 美元；它也不属于 LP 手续费。

## 被复核的参数（只用于研究）

- 资金：LP {record['trial']['lp_budget']:.0f} / 对冲保证金 {round(record['trial']['lp_budget']/2.5):.0f} / 储备 {capital-record['trial']['lp_budget']-round(record['trial']['lp_budget']/2.5):.0f} 美元；其中原生 SOL 储备 {record['native_sol']:.8f} SOL。
- 区间约 `[P/{1+params['width']:.2f}, P×{1+params['width']:.2f}]`，按 DLMM bin 向外取整；区间内目标对冲库存比例 {params['hedge_ratio']:.0%}，3 倍是保证金杠杆，不是把对冲币数乘 3。
- 入场：收盘高于 EMA{r['ema_entry_hours']}、EMA{r['ema_exit_hours']} 不下降，过去 {r['momentum_hours']} 小时涨幅在 {r['min_momentum']:.0%}～{r['max_momentum']:.0%}，连续 {r['entry_confirm_hours']} 个健康小时。
- 小时收益波动率阈值 {r['max_hourly_vol']:.1%}、小时振幅 {r['max_bar_range']:.0%}；保留急跌、下降趋势、相对波动率和基差退出。越界撤出后冷却 {params['cooldown_hours']} 小时，并重新满足入场条件。
- 对冲调整死区 {record['trial']['hedge_deadband_usd']:.0f} 美元；小额订单可能达不到开仓门槛，目标对冲比例不保证等于实际持续对冲比例。

## 局限和后续结论

1. 40% APR 是假设，未取得历史逐 bin 个人手续费分配数据；未将其伪装成实际收入。
2. 5 分钟池价共 {fine['pool_rows']:,} 根，缺一根且不插值；合约价仅 {fine['observed_hl_5m_rows']:,} 根为真实观察，{fine['synthetic_hedge_rows']:,} 根使用当时小时开盘基差构造代理，占 {fine['synthetic_hedge_rows']/fine['pool_rows']:.2%}。5 分钟结果属于模型敏感性检查。
3. 原生租金按 SDK 尺寸公式和免租金系数估算，并预留 gas/ATA/bin-array 预算。历史链上账户分布、真正租金报价、交易拥堵不能精确重建。
4. 换币、对冲、操作成本是研究假设；未假定 maker 挂单必成交。逐仓 10% 权益缓冲是保守筛选，不是完整历史强平重建，模型回撤不等于实盘最大可能亏损。
5. 本轮 10 小时运行未中途改规则，冻结源码、数据和配置绑定匹配；该活跃候选四个原训练/验证结果独立重放一致。没有发送真实交易或修改线上策略。

**目前没有支持部署该配置或宣称半年能达到 25% 的证据。** 若继续研究，应另立新批次，先验证资金利用率、SOL 储备敞口、租金和最低订单对小额账户的影响，并用新的时间段做前向模拟。提高 APR 假设或忽略储备亏损都不能当作达成原目标。

## 本地复算与证据

```bash
cargo run --release --locked --example solana_training_review -- \\
  --data backtests/solana/2026-09-18-six-month \\
  --training backtests/solana/2026-09-19-target25 \\
  --output backtests/solana/2026-09-19-target25/review
```

- [全部复核和小时曲线](review.json)、[5分钟复核](active-fine.json)、[情景对比CSV](scenarios.csv)、[研究配置](active-candidate-paper.toml)。
- 原始批次文件：上级目录的 `progress.json`、`trials.jsonl`、`assumptions.json`、`research-config.toml` 和 `research-binary`。
- 原始数据质量检查：`../../2026-09-19-target25-audit/data-quality.json`；对应六个月数据目录中的 `manifest.json` 保留 841 份原始响应校验值。
- 冻结二进制 SHA256：`{binary_hash}`。
- 报告生成时间：{dt.datetime.now(dt.timezone.utc).isoformat()}。研究数据及生成产物均在 Git 忽略目录内。
"""
    (folder / "REPORT.md").write_text(report)
    print(folder / "REPORT.md")


if __name__ == "__main__":
    main()

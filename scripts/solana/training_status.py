#!/usr/bin/env python3
"""Read-only progress summary; never treats a profitable rent reserve as LP fees."""
import argparse
import json
import pathlib


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=pathlib.Path)
    args = parser.parse_args()
    progress = json.loads((args.directory / "progress.json").read_text())
    print(f"阶段：{progress['status']}")
    print(f"累计计算：{progress['compute_seconds'] / 3600:.3f} 小时")
    print(f"完成参数：{progress['completed']:,} 组；资金不足剔除：{progress['rejected_budget']:,} 组")
    print(f"半年账户净收益目标：{progress['target_return']:.1%}")
    best = progress.get("best")
    if not best:
        print("尚未完成首组参数。")
        return
    full = best["full"]
    print(f"开发期验证通过：{best['record']['development_passed']}")
    print(f"当前候选半年净损益：{full['pnl']:+.4f} USD；最大回撤：{full['max_drawdown_pct']:.2f}%")
    print(f"假设LP手续费：{full['lp_fees']:.4f} USD；LP在场：{full['active_hours']:.1f} 小时")
    print(f"原生SOL储备损益：{full['native_reserve_pnl']:+.4f} USD；储备换币成本：{full['native_reserve_cost']:.4f} USD")
    print(f"最后62天独立起仓损益：{best['later_62_days']['pnl']:+.4f} USD（已被前期研究看过，不是盲测）")
    print(f"双倍执行成本损益：{best['double_cost']['pnl']:+.4f} USD")
    print(f"模型拒绝原因：{full['model_rejections'] or '无'}")
    print(f"达到目标并通过细粒度模型：{best['target_passes_fine_model']}")
    print("40% APR 是假设；此结果不等于未来收益，也不会自动用于实盘。")


if __name__ == "__main__":
    main()

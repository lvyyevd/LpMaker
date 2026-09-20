#!/usr/bin/env bash
# 离线研究专用：不加载 .env，不接触实盘钱包/状态。重复执行会验证绑定并续算。
set -euo pipefail
cd "$(dirname "$0")/../.."
lp_data=${1:-backtests/solana/2026-09-18-six-month}
lp_output=${2:-backtests/solana/2026-09-19-target25}
test -s "$lp_data/manifest.json" || { echo "缺少本地六个月行情：$lp_data"; exit 1; }
cargo build --release --locked --bin lp-maker-solana
mkdir -p "$lp_output"
# 固定保存这一轮代码对应的二进制，后续线上编译不会替换正在使用的研究程序。
# 若续算已有结果，必须保留原二进制及配置，避免新代码混入同一批结果。
if [ ! -s "$lp_output/research-binary" ]; then
  test ! -e "$lp_output/progress.json" || { echo '已有结果但缺少冻结二进制，请使用新输出目录'; exit 1; }
  cp target/release/lp-maker-solana "$lp_output/research-binary"
  cp config/solana-regime.toml "$lp_output/research-config.toml"
fi
exec "$lp_output/research-binary" --config "$lp_output/research-config.toml" train \
  --data "$lp_data" --output "$lp_output" --hours 10 --target-return 0.25 --seed 250919

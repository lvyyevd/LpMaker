#!/usr/bin/env bash
# 只启动现有配置并保留所有状态；不平仓、不清缓存、不重新授予首仓资格。
set -euo pipefail
umask 077
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[ "$(uname -s)" = Linux ] || { echo "本脚本用于 Linux 线上服务器"; exit 1; }
. scripts/lib/background.sh
mkdir -p data
exec 8>data/exit-reset.lock
flock -n 8 || { echo "退出/切换任务正在运行，未重复启动"; exit 1; }
lp_bin=./target/release/lp-maker
lp_config=config/local.toml
[ -x "$lp_bin" ] || { echo "请先执行 cargo build --release --locked --bin lp-maker"; exit 1; }
lp_state=$("$lp_bin" --config "$lp_config" check --state-dir)
mkdir -p "$lp_state"
flock -n "$lp_state/process.lock" true || {
  echo "该状态目录正被策略或维护进程使用，未重复启动"
  exit 1
}
if [ -e "$lp_state/strategy_migration.json" ] || [ -e "$lp_state/manual_reset.json" ]; then
  echo "退出/切换尚未完成；先按原维护命令核对并完成，不删除状态、不直接开新仓"
  exit 1
fi
set -a
if [ -f .env ]; then . ./.env; fi
set +a
lp_start_background "$PWD/run.log" "$lp_bin" --config "$lp_config" run --execute
echo "后台进程 PID：${LP_BACKGROUND_PID}；沿用原状态，正在进行启动对账。"
echo "运行日志：$PWD/run.log"
echo "查看日志：tail -f run.log（Ctrl+C 只停止查看日志）"

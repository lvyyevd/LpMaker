#!/usr/bin/env bash
# Linux 默认实盘：退出当前池和配置对冲币，核对余额（含限额尾差）后归档重置，再按首仓启动。
set -euo pipefail
umask 077
cd "$(dirname "${BASH_SOURCE[0]}")/.."

mkdir -p data
exec 8>data/exit-reset.lock
flock -n 8 || { echo "已有退出重置脚本正在运行，已取消重复执行"; exit 1; }

set -a
if [ -f .env ]; then . ./.env; fi
set +a

# 编译失败不打断现有策略。只有确认新命令可用后才停止旧进程。
cargo build --release --locked --bin lp-maker
for lp_pid in $(pgrep -f '^[^ ]*lp-maker --config config/local[.]toml run --execute([[:space:]]|$)' || true); do
  if [ "$(readlink "/proc/$lp_pid/cwd" 2>/dev/null || true)" = "$PWD" ]; then
    kill -TERM "$lp_pid" 2>/dev/null || true
  fi
done
mkdir -p data/live-200
flock -w 30 data/live-200/process.lock true

mkdir -p data/operator-logs
lp_exit_log=$(mktemp "$PWD/data/operator-logs/exit-reset.log.XXXXXXXX")
echo "开始实盘退出；LP/对冲平仓、基础币兑换及尾差核对、备份日志：$lp_exit_log"
# pipefail：平仓失败、部分成交未清完、余额查询失败或备份失败，均不执行后面的重启。
./target/release/lp-maker --config config/local.toml reset-flat --execute 2>&1 | tee "$lp_exit_log"

if [ -f run.log ]; then
  lp_old_log=$(mktemp "$PWD/data/operator-logs/run-before-reset.log.XXXXXXXX")
  mv -- run.log "$lp_old_log"
fi
nohup ./target/release/lp-maker --config config/local.toml run --execute \
  8>&- </dev/null >run.log 2>&1 &
echo "可交易仓位已退出并重置（若有微小尾差已记录）；启动 PID：$!；日志：$PWD/run.log"

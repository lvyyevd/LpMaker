#!/usr/bin/env bash
# 在 Linux 服务器上显式恢复一次首仓入场；所有状态、nonce 和审计记录原地保留。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

test -s data/live-200/checkpoint.json || {
  echo "未找到原检查点，已取消操作"
  exit 1
}

set -a
if [ -f .env ]; then . ./.env; fi
set +a

# 编译失败时不停止旧进程。
cargo build --release --locked --bin lp-maker
for lp_pid in $(pgrep -f '^[^ ]*lp-maker --config config/local[.]toml run --execute([[:space:]]|$)' || true); do
  if [ "$(readlink "/proc/$lp_pid/cwd" 2>/dev/null || true)" = "$PWD" ]; then
    kill -TERM "$lp_pid" 2>/dev/null || true
  fi
done
flock -w 30 data/live-200/process.lock true

# 程序持有独占锁、重新核对链上与交易所空仓，备份检查点，再发放一次性许可。
# 核对失败就停在这里，不删除任何文件，也不强行继续建仓。
./target/release/lp-maker --config config/local.toml rearm-entry --execute

nohup ./target/release/lp-maker --config config/local.toml run --execute \
  </dev/null >>run.log 2>&1 &
lp_new_pid=$!
echo "启动 PID：$lp_new_pid；正在核对市场与资金条件，日志：$PWD/run.log"

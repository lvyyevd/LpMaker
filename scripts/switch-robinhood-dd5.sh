#!/usr/bin/env bash
# Linux：按旧记录退出 → 验证空仓 → 归档清理 → 安装新策略 → 首仓启动。
# 不使用 rm -rf；任何未知成交、残余仓位或配置冲突都会中止重启。
set -euo pipefail
umask 077
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[ "$(uname -s)" = Linux ] || { echo "本脚本用于 Linux 线上服务器"; exit 1; }
. scripts/lib/background.sh
lp_config=config/local.toml
lp_bin=./target/release/lp-maker
mkdir -p data/operator-logs

# 整个编译/平仓/迁移任务也要后台化，而非只在最后把策略加上 &。
# --worker 仅供本脚本内部启动；入口立即返回，进度写入独立日志。
if [ "$#" -eq 0 ]; then
  lp_switch_log=$(mktemp "$PWD/data/operator-logs/switch-robinhood-dd5-task.log.XXXXXXXX")
  lp_start_background "$lp_switch_log" bash "$PWD/scripts/switch-robinhood-dd5.sh" --worker
  echo "已提交后台切换任务，PID：${LP_BACKGROUND_PID}；此时尚未确认平仓或建仓完成。"
  echo "编译、平仓和启动进度：$lp_switch_log"
  printf '查看进度：tail -f %q\n' "$lp_switch_log"
  echo "Ctrl+C 只停止查看日志；后台任务会继续。完成前不要重复执行切换命令。"
  exit 0
fi
[ "$#" -eq 1 ] && [ "$1" = --worker ] || { echo "不支持的参数" >&2; exit 1; }
exec 8>data/exit-reset.lock
flock -n 8 || { echo "已有退出/切换脚本运行，已取消重复执行"; exit 1; }
trap 'echo "切换未完成：已停止后续步骤。保留了交易记录和备份，请检查上方错误；不要手动删除状态。" >&2' ERR

# 编译及只读预检失败时，不停止当前正常运行的策略。
cargo build --release --locked --bin lp-maker
"$lp_bin" --config "$lp_config" switch-robinhood-dd5
lp_state=$("$lp_bin" --config "$lp_config" check --state-dir)
lp_config_absolute=$(readlink -f "$lp_config")
set -a
if [ -f .env ]; then . ./.env; fi
set +a

# 用 /proc 的独立参数确认进程，兼容绝对路径和 --config=path，避免误停其他池。
for lp_cmdline in /proc/[0-9]*/cmdline; do
  [ -r "$lp_cmdline" ] || continue
  lp_pid=${lp_cmdline#/proc/}
  lp_pid=${lp_pid%/cmdline}
  [ "$(readlink "/proc/$lp_pid/cwd" 2>/dev/null || true)" = "$PWD" ] || continue
  lp_args=()
  while IFS= read -r -d '' lp_arg; do lp_args+=("$lp_arg"); done <"$lp_cmdline" || continue
  [ "${#lp_args[@]}" -gt 0 ] || continue
  [ "${lp_args[0]##*/}" = lp-maker ] || continue
  lp_run=false lp_execute=false lp_process_config=""
  for ((lp_i=1; lp_i<${#lp_args[@]}; lp_i++)); do
    case "${lp_args[$lp_i]}" in
      run) lp_run=true ;;
      --execute) lp_execute=true ;;
      --config)
        if (( lp_i + 1 < ${#lp_args[@]} )); then lp_process_config=${lp_args[$((lp_i+1))]}; fi ;;
      --config=*) lp_process_config=${lp_args[$lp_i]#--config=} ;;
    esac
  done
  if $lp_run && $lp_execute && [ -n "$lp_process_config" ] &&
     [ "$(readlink -f "$lp_process_config" 2>/dev/null || true)" = "$lp_config_absolute" ]; then
    echo "停止原策略 PID：${lp_pid}；等待其保存状态并退出"
    kill -TERM "$lp_pid" 2>/dev/null || true
  fi
done
mkdir -p "$lp_state"
# 不删除 process.lock；换掉锁文件会破坏单实例保护。
flock -w 60 "$lp_state/process.lock" true

lp_exit_log=$(mktemp "$PWD/data/operator-logs/switch-robinhood-dd5.log.XXXXXXXX")
echo "开始真实平仓与切换，详细日志：$lp_exit_log"
"$lp_bin" --config "$lp_config" switch-robinhood-dd5 --execute 2>&1 | tee "$lp_exit_log"

if [ -f run.log ]; then
  lp_old_log=$(mktemp "$PWD/data/operator-logs/run-before-switch.log.XXXXXXXX")
  mv -- run.log "$lp_old_log"
fi
lp_start_background "$PWD/run.log" "$lp_bin" --config "$lp_config" run --execute
lp_new_pid=$LP_BACKGROUND_PID
echo "新策略启动 PID：${lp_new_pid}；日志：$PWD/run.log"

# PID 存在不等于对账成功，等到至少完成一次策略决策才报告健康。
for ((lp_attempt=0; lp_attempt<60; lp_attempt++)); do
  if ! kill -0 "$lp_new_pid" 2>/dev/null; then
    echo "新策略已退出，请查看 run.log；旧记录备份仍保留，不要删除后重试" >&2
    exit 1
  fi
  if "$lp_bin" --config "$lp_config" health --max-age-seconds 120 >/dev/null 2>&1; then
    echo "已完成新策略首轮核对；是否建仓取决于实时风控与资金条件，请查看 run.log。"
    exit 0
  fi
  sleep 2
done
echo "进程仍在运行，但尚未完成首轮核对，请查看 run.log；不要重复执行平仓切换脚本。" >&2
exit 2

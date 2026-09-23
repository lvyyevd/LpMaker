#!/usr/bin/env bash
# 被运维脚本 source；独立 session/process group，终端 Ctrl+C 不再传给策略。
# nohup 只处理挂断信号；Rust 会注册 SIGINT，因此还必须隔离终端进程组。
lp_start_background() {
  local lp_bg_log=$1
  shift
  command -v setsid >/dev/null || {
    echo "缺少 setsid（util-linux），未启动后台任务" >&2
    return 1
  }
  local lp_bg_dir lp_bg_try
  mkdir -p data/operator-logs
  lp_bg_dir=$(mktemp -d "$PWD/data/operator-logs/background.XXXXXXXX") || return 1
  # --fork 的外层 PID 并非最终服务 PID；由新 session 中的进程自行记录真实 PID。
  # 关闭维护脚本的 fd 8，防止常驻策略一直占用运维互斥锁。
  setsid --fork bash -c '
    set -e
    umask 077
    trap "" HUP
    lp_bg_pid_file=$1
    shift
    printf "%s\n" "$$" >"${lp_bg_pid_file}.tmp"
    mv -- "${lp_bg_pid_file}.tmp" "$lp_bg_pid_file"
    exec "$@"
  ' lp-background "$lp_bg_dir/pid" "$@" 8>&- </dev/null >>"$lp_bg_log" 2>&1 || return 1
  for ((lp_bg_try=0; lp_bg_try<100; lp_bg_try++)); do
    if [ -s "$lp_bg_dir/pid" ]; then
      LP_BACKGROUND_PID=$(cat "$lp_bg_dir/pid")
      case "$LP_BACKGROUND_PID" in
        ''|*[!0-9]*) echo "后台 PID 记录无效" >&2; return 1 ;;
      esac
      kill -0 "$LP_BACKGROUND_PID" 2>/dev/null || {
        echo "后台任务已退出，请查看：$lp_bg_log" >&2
        return 1
      }
      return 0
    fi
    sleep 0.05
  done
  echo "尚未收到后台 PID，请先检查日志，不要重复启动：$lp_bg_log" >&2
  return 1
}

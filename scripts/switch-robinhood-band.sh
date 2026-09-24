#!/usr/bin/env bash
# 使用同一套事务化退出/备份/清理流程；主任务和新策略均在独立后台会话中运行。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export LPMAKER_SWITCH_PROFILE=band
exec bash scripts/switch-robinhood-dd5.sh "$@"

#!/usr/bin/env sh
set -eu

restart_file="${TGDRIVE_BOT_API_RESTART_FILE:-/bot-api-control/restart}"
child_pid=""
ack_after_start=0

stop_child() {
  if [ -n "$child_pid" ] && kill -0 "$child_pid" 2>/dev/null; then
    kill "$child_pid" 2>/dev/null || true
    remaining=20
    while kill -0 "$child_pid" 2>/dev/null && [ "$remaining" -gt 0 ]; do
      sleep 0.1
      remaining=$((remaining - 1))
    done
    if kill -0 "$child_pid" 2>/dev/null; then
      echo "Telegram Bot API 未及时退出，强制终止进程"
      kill -KILL "$child_pid" 2>/dev/null || true
    fi
    wait "$child_pid" 2>/dev/null || true
  fi
  child_pid=""
}

shutdown() {
  stop_child
  exit 0
}

trap shutdown INT TERM
mkdir -p "$(dirname "$restart_file")"

while true; do
  /docker-entrypoint.sh &
  child_pid=$!
  if [ "$ack_after_start" -eq 1 ]; then
    rm -f "$restart_file"
    ack_after_start=0
  fi

  while kill -0 "$child_pid" 2>/dev/null; do
    if [ -f "$restart_file" ]; then
      echo "收到 tgdrive 上传取消信号，重启 Telegram Bot API 以终止在途上传"
      ack_after_start=1
      stop_child
      break
    fi
    sleep 0.2
  done

  if [ -n "$child_pid" ]; then
    wait "$child_pid" 2>/dev/null || true
    child_pid=""
  fi
  sleep 0.2
done

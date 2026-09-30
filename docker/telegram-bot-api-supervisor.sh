#!/usr/bin/env sh
set -eu

restart_file="${TGDRIVE_BOT_API_RESTART_FILE:-/bot-api-control/restart}"
proxy_config="${TGDRIVE_BOT_API_PROXY_CONFIG:-}"
proxy_runtime=""
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
  if [ -n "$proxy_runtime" ]; then
    rm -f "$proxy_runtime"
  fi
  exit 0
}

trap shutdown INT TERM
mkdir -p "$(dirname "$restart_file")"
if [ -n "$proxy_config" ]; then
  proxy_runtime=$(mktemp /tmp/tgdrive-proxy.XXXXXX)
fi

while true; do
  if [ -n "$proxy_config" ]; then
    # 配置由 tgdrive 在内部代理开始监听后原子发布，不能缺配置时悄悄直连。
    while [ ! -s "$proxy_config" ]; do
      sleep 1
    done
    proxy_revision=$(cksum < "$proxy_config")
    # proxychains 的第一个代理必须为数字 IP。每次 tgdrive 重建后重新解析 Docker DNS。
    bridge_ip=$(getent hosts tgdrive | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
    if [ -z "$bridge_ip" ]; then
      sleep 1
      continue
    fi
    awk -v bridge_ip="$bridge_ip" '$1 == "socks5" && $2 == "tgdrive" { $2 = bridge_ip } { print }' "$proxy_config" > "$proxy_runtime"
    proxychains4 -q -f "$proxy_runtime" /docker-entrypoint.sh &
  else
    /docker-entrypoint.sh &
  fi
  child_pid=$!
  if [ "$ack_after_start" -eq 1 ]; then
    rm -f "$restart_file"
    ack_after_start=0
  fi

  while kill -0 "$child_pid" 2>/dev/null; do
    if [ -n "$proxy_config" ] && { [ ! -s "$proxy_config" ] || [ "$(cksum < "$proxy_config")" != "$proxy_revision" ]; }; then
      echo "tgdrive 代理入口已更新，重新连接共享代理"
      stop_child
      break
    fi
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

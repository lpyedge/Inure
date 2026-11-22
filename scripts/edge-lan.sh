#!/usr/bin/env bash
# 局域网测试启动。用法：PORT=39871 scripts/edge-lan.sh [边缘参数…]
# RIG_EDGE_TOKEN 缺省随机生成。停止：kill <edge-pid>。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

for arg in "$@"; do
  case "$arg" in
    --listen|--token) echo "Use PORT and RIG_EDGE_TOKEN to configure this launcher." >&2; exit 2 ;;
    --stdio) echo "Run rig-edge-browser --stdio directly for MCP." >&2; exit 2 ;;
  esac
done
TOKEN="${RIG_EDGE_TOKEN:-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
PORT="${PORT:-39871}"
IP="$(ip -4 -o addr show up scope global | awk 'NR == 1 { split($4, address, "/"); print address[1] }')"
[ -n "$IP" ] || { echo "Could not find a LAN IPv4 address" >&2; exit 1; }

# 与 build.sh 使用相同的构建目录，并从中选择最新的可执行文件。
T="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN=
for candidate in "$T"/{release,debug}/rig-edge-browser; do
  if [[ -x "$candidate" && ( -z "$BIN" || "$candidate" -nt "$BIN" ) ]]; then
    BIN="$candidate"
  fi
done
[ -n "$BIN" ] || { echo "Build first: scripts/build.sh" >&2; exit 1; }
# 源码比二进制新 → 提醒重新编译
if [ -n "$(find crates -type f \( -name '*.rs' -o -name '*.js' -o -name '*.html' \) -newer "$BIN" -print -quit 2>/dev/null)" ]; then
  echo "The binary is older than the sources: $BIN. Run scripts/build.sh first." >&2
fi
echo "Using binary: $BIN" >&2
# 端口已被占用（例如上一次启动的旧实例还在跑）→ 直接报错，别让旧实例冒充新实例应答健康检查
OWNER="$(ss -ltnp 2>/dev/null | awk -v p=":$PORT" '$4 ~ p"$" {print $NF}' | head -1 || true)"
if [ -n "$OWNER" ]; then
  echo "Port $PORT is already in use ($OWNER). Stop that instance first or set PORT to another port." >&2
  exit 1
fi
mkdir -p "$ROOT/.temp"
RIG_EDGE_TOKEN="$TOKEN" "$BIN" --listen "$IP:$PORT" --human --av "$@" \
  >"$ROOT/.temp/edge-lan.log" 2>&1 &
EDGE_PID=$!
echo "$EDGE_PID" >"$ROOT/.temp/edge-lan.pid"

deadline=$((SECONDS + 20))
while (( SECONDS < deadline )) && kill -0 "$EDGE_PID" 2>/dev/null; do
  if curl -kf --max-time 1 --silent "https://$IP:$PORT/ready" >/dev/null &&
     kill -0 "$EDGE_PID" 2>/dev/null; then
    echo "OK  pid=$EDGE_PID" >&2
    echo "URL: https://$IP:$PORT/" >&2
    if [ -z "${RIG_EDGE_TOKEN:-}" ]; then echo "Token: $TOKEN" >&2; fi
    echo "Stop: kill $EDGE_PID" >&2
    exit 0
  fi
  sleep 0.25
done
kill "$EDGE_PID" 2>/dev/null || true
echo "The edge did not become ready; see $ROOT/.temp/edge-lan.log" >&2
tail -20 "$ROOT/.temp/edge-lan.log" >&2
exit 1

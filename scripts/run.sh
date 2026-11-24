#!/usr/bin/env bash
# 本地快速运行：随机令牌 + 观看页。
# 用法：scripts/run.sh [边缘程序的额外参数...]
#   例：scripts/run.sh --url https://example.com
#       PORT=40000 scripts/run.sh
# 打开打印出的地址，在令牌输入页填打印出的令牌。Ctrl+C 优雅退出（让 Chromium 落盘并清理临时目录）。
set -euo pipefail
cd "$(dirname "$0")/.."
T="${CARGO_TARGET_DIR:-$PWD/target}"
# 取 release / debug 里**更新**的那个，避免跑到过期的旧构建
BIN=
for candidate in "$T"/{release,debug}/rig-edge-browser; do
  if [[ -x "$candidate" && ( -z "$BIN" || "$candidate" -nt "$BIN" ) ]]; then
    BIN="$candidate"
  fi
done
scheme=http
for arg in "$@"; do
  case "$arg" in
    --listen|--token) echo "Use PORT and RIG_EDGE_TOKEN to configure this launcher." >&2; exit 2 ;;
    --tls-cert) scheme=https ;;
  esac
done
[ -n "$BIN" ] || { echo "Build first: scripts/build.sh" >&2; exit 1; }
PORT="${PORT:-39871}"
TOKEN="${RIG_EDGE_TOKEN:-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
echo "URL:    $scheme://127.0.0.1:$PORT/" >&2
if [ -z "${RIG_EDGE_TOKEN:-}" ]; then echo "Token:  $TOKEN" >&2; fi
echo "Binary: $BIN" >&2
RIG_EDGE_TOKEN="$TOKEN" exec "$BIN" --listen "127.0.0.1:$PORT" --human "$@"

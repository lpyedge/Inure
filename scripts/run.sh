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
BIN="$(ls -t "$T/release/rig-edge-browser" "$T/debug/rig-edge-browser" 2>/dev/null | head -1 || true)"
[ -n "$BIN" ] && [ -x "$BIN" ] || { echo "先编译：scripts/build.sh" >&2; exit 1; }
PORT="${PORT:-39871}"
TOKEN="${RIG_EDGE_TOKEN:-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
echo "地址： http://127.0.0.1:$PORT/"
echo "令牌： $TOKEN"
echo "二进制：$BIN"
exec "$BIN" --listen "127.0.0.1:$PORT" --token "$TOKEN" --human "$@"

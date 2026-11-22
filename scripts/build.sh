#!/usr/bin/env bash
# 编译边缘程序并检查动态库依赖。
# 用法：scripts/build.sh [debug|release]    默认 release
# 产物：$CARGO_TARGET_DIR（默认 target/）下的 {debug,release}/rig-edge-browser
set -euo pipefail
cd "$(dirname "$0")/.."
MODE="${1:-release}"
case "$MODE" in
  debug)   cargo build -p rig-edge-browser ;;
  release) cargo build --release -p rig-edge-browser ;;
  *) echo "用法：$0 [debug|release]" >&2; exit 2 ;;
esac
BIN="${CARGO_TARGET_DIR:-$PWD/target}/$MODE/rig-edge-browser"
[ -x "$BIN" ] || { echo "没找到产物 $BIN" >&2; exit 1; }
echo "产物：$BIN ($(du -h --apparent-size "$BIN" | cut -f1))"
MISSING="$(ldd "$BIN" 2>/dev/null | grep 'not found' || true)"
if [ -n "$MISSING" ]; then
  echo "⚠ 缺少运行时库（见 docs/04-依赖与环境.md）：" >&2
  echo "$MISSING" >&2
  exit 1
fi
echo "动态库依赖完整"

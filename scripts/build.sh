#!/usr/bin/env bash
# 编译边缘程序并检查动态库依赖。
# 用法：scripts/build.sh [debug|release]    默认 release
# 产物：$CARGO_TARGET_DIR（默认 target/）下的 {debug,release}/rig-edge-browser
set -euo pipefail
cd "$(dirname "$0")/.."
MODE="${1:-release}"
if (( $# > 1 )); then
  echo "Usage: $0 [debug|release]" >&2
  exit 2
fi
case "$MODE" in
  debug)   cargo build -p rig-edge-browser ;;
  release) cargo build --release --locked -p rig-edge-browser ;;
  *) echo "Usage: $0 [debug|release]" >&2; exit 2 ;;
esac
BIN="${CARGO_TARGET_DIR:-$PWD/target}/$MODE/rig-edge-browser"
[ -x "$BIN" ] || { echo "Build output not found: $BIN" >&2; exit 1; }
echo "Output: $BIN ($(du -h --apparent-size "$BIN" | cut -f1))"
LIBRARIES="$(LC_ALL=C ldd "$BIN")"
MISSING="$(grep 'not found' <<< "$LIBRARIES" || true)"
if [ -n "$MISSING" ]; then
  echo "⚠ Missing runtime libraries (see docs/04-依赖与环境.md):" >&2
  echo "$MISSING" >&2
  exit 1
fi
echo "All shared-library dependencies are present"

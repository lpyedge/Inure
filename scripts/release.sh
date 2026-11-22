#!/usr/bin/env bash
# 发布打包：release 构建 → 依赖检查 → 打成 dist/ 下的 tar.gz（含二进制、文档、校验和、构建信息）。
# 用法：scripts/release.sh
# 产物：dist/rig-edge-browser-<版本>-<git>-<系统>-<架构>.tar.gz 及同名 .sha256
# 说明：二进制动态链接系统的 FFmpeg 共享库，**不随包携带**；目标机器的依赖见包内 docs/04-依赖与环境.md。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
GIT="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
[ -z "$(git status --porcelain 2>/dev/null)" ] || GIT="$GIT-dirty"
OS="$(uname -s | tr 'A-Z' 'a-z')"; ARCH="$(uname -m)"
NAME="rig-edge-browser-$VERSION-$GIT-$OS-$ARCH"

bash scripts/build.sh release
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/rig-edge-browser"

STAGE="$ROOT/dist/$NAME"
rm -rf "$STAGE"; mkdir -p "$STAGE/docs"
cp "$BIN" "$STAGE/"
cp README.md "$STAGE/"
cp docs/README.md docs/[0-9][0-9]-*.md "$STAGE/docs/"
{
  echo "name:    rig-edge-browser"
  echo "version: $VERSION"
  echo "git:     $GIT"
  echo "built:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "rustc:   $(rustc --version)"
  echo "host:    $(uname -srm)"
  echo
  echo "# 运行时需要的共享库（目标机器须有同主版本）"
  ldd "$BIN" | awk '/=>/ {print $1}' | grep -E "libav|libsw|libx26|libpulse" | sort -u
} > "$STAGE/BUILD-INFO.txt"

( cd "$ROOT/dist" && tar czf "$NAME.tar.gz" "$NAME" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
rm -rf "$STAGE"
echo
echo "发布包：dist/$NAME.tar.gz ($(du -h --apparent-size "dist/$NAME.tar.gz" | cut -f1))"
cat "dist/$NAME.tar.gz.sha256"

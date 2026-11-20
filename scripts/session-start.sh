#!/bin/bash
# 云端会话启动时预取 Rust 依赖。本地运行时直接退出。
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi
cd "$CLAUDE_PROJECT_DIR" || exit 0
cargo fetch --quiet >/dev/null 2>&1 || true
exit 0

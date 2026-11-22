#!/usr/bin/env bash
# 本地提交门禁：CI 检查与真实 Chromium 集成测试。
# 任一步失败即非零退出。在仓库根目录运行 bash scripts/local-gate.sh。
# 需要 libav 开发包和 Chromium；音视频用例还需要 PulseAudio、ffprobe、ffmpeg。
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "1/3 cargo fmt --all --check"
cargo fmt --all --check

step "2/3 cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "3/3 Rust tests (including Chromium integration) and Python self-tests"
scripts/test.sh all

#!/usr/bin/env bash
# 本地提交门禁：CI 检查与真实 Chromium 集成测试。
# 任一步失败即非零退出。在仓库根目录运行 bash scripts/local-gate.sh。
# 需要 libav* 开发包、完整 Chromium、PulseAudio 虚拟声卡和 ffprobe。
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "1/4 cargo fmt --all --check"
cargo fmt --all --check

step "2/4 cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "3/4 cargo test --all (含 Chromium 集成测试)"
RIG_EDGE_IT=1 cargo test --all

step "4/4 Python 评估工具自测"
python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v

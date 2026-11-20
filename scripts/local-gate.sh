#!/usr/bin/env bash
# 本地等价 CI 门禁：复现 .github/workflows/ci.yml 的全部步骤。
# 任一步失败即非零退出（与 CI 语义一致）。在仓库根目录运行：bash scripts/local-gate.sh
# 注意：edge-browser 需要系统 libav*（-dev 包），缺失时 2/3 步失败属环境问题，先装依赖。
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "1/4 cargo fmt --all --check"
cargo fmt --all --check

step "2/4 cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "3/4 cargo test --all"
cargo test --all

step "4/4 Python 评估工具自测"
python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v

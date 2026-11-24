#!/usr/bin/env bash
# 本地提交门禁：CI 检查与真实 Chromium 集成测试。
# 任一步失败即非零退出。在仓库根目录运行 bash scripts/local-gate.sh。
# 需要 libav 开发包和 Chromium；音视频用例还需要 PulseAudio、ffprobe、ffmpeg。
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

# 缺工具就明确报「未验证」并失败，不要让后面的步骤给出误导性的结果。
for tool in cargo python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "NOT VERIFIED: $tool is not installed, the gate cannot run" >&2; exit 127; }
done
have_chromium=0
for name in chromium chromium-browser google-chrome google-chrome-stable; do   # 与边缘程序和测试查找的名字一致
  command -v "$name" >/dev/null 2>&1 && have_chromium=1
done
if (( ! have_chromium )) && [[ -z "${RIG_EDGE_TEST_CHROMIUM_DIR:-}" ]]; then
  echo "warning: no Chromium on PATH and RIG_EDGE_TEST_CHROMIUM_DIR is unset; the browser integration tests will fail" >&2
fi

step "1/4 docs/code consistency (scripts/check-docs.sh)"
scripts/check-docs.sh

step "2/4 cargo fmt --all --check"
cargo fmt --all --check

step "3/4 cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "4/4 Rust tests (including Chromium integration) and Python self-tests"
scripts/test.sh all

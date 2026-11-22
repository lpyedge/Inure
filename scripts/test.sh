#!/usr/bin/env bash
# 常用测试入口。
# 用法：scripts/test.sh [unit|it|av|py|all]    默认 unit
#   unit  cargo test --all，跳过真 Chromium 集成测试
#   it    真 Chromium 集成测试；有便携 PulseAudio 时自动用它
#   av    只跑音视频集成用例 edge_av_and_image_clipboard
#   py    Python 评估工具自测
#   all   一次运行全部 Rust 测试（含真 Chromium）与 Python 自测
# 提交前的完整门禁请用 scripts/local-gate.sh（含 fmt/clippy）。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
WHAT="${1:-unit}"

run_with_it() {
  if [ -x "$ROOT/.temp/media-deps/root/usr/bin/pulseaudio" ]; then
    export RIG_EDGE_TEST_PULSEAUDIO_DIR="${RIG_EDGE_TEST_PULSEAUDIO_DIR:-$ROOT/.temp/media-deps/root}"
  fi
  RIG_EDGE_IT=1 cargo test "$@"
}

case "$WHAT" in
  unit) unset RIG_EDGE_IT; cargo test --all ;;
  it)   run_with_it -p rig-edge-browser --test edge_it ;;
  av)   run_with_it -p rig-edge-browser --test edge_it -- edge_av_and_image_clipboard ;;
  py)   python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v ;;
  all)  run_with_it --all; python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v ;;
  *) echo "Usage: $0 [unit|it|av|py|all]" >&2; exit 2 ;;
esac

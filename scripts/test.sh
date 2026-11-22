#!/usr/bin/env bash
# 常用测试入口。
# 用法：scripts/test.sh [unit|it|av|py|all]    默认 unit
#   unit  cargo test --all（离线、快速，不含真 Chromium 集成测试）
#   it    真 Chromium 集成测试（RIG_EDGE_IT=1，约 150 秒）；有便携 PulseAudio 时自动用它
#   av    只跑音视频集成用例 edge_av_and_image_clipboard
#   py    Python 评估工具自测
#   all   unit + it + py
# 提交前的完整门禁请用 scripts/local-gate.sh（含 fmt/clippy）。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
WHAT="${1:-unit}"

run_it() {  # $@ = cargo test 的额外参数
  if [ -x "$ROOT/.temp/pulseaudio/usr/bin/pulseaudio" ]; then
    RIG_EDGE_IT=1 "$ROOT/scripts/with-pulse.sh" cargo test -p rig-edge-browser --test edge_it "$@"
  else
    echo "提示：没有便携 PulseAudio（docs/04 §4.2），音视频用例会因缺音频环境失败" >&2
    RIG_EDGE_IT=1 cargo test -p rig-edge-browser --test edge_it "$@"
  fi
}

case "$WHAT" in
  unit) cargo test --all ;;
  it)   run_it ;;
  av)   run_it -- edge_av_and_image_clipboard ;;
  py)   python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v ;;
  all)  cargo test --all; run_it; python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v ;;
  *) echo "用法：$0 [unit|it|av|py|all]" >&2; exit 2 ;;
esac

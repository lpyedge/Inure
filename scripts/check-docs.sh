#!/usr/bin/env bash
# 文档与代码一致性的快速检查（不需要构建、不需要浏览器）：
#   1. 文档里引用的 scripts/ 下脚本必须存在（带日期的历史文档不查，它们是只读证据）；
#   2. rig-edge-browser 源码里的每个 --flag 都要在 docs/05 的参数表里出现；
#   3. rig-edge-browser --help 的用法文本要列出全部源码参数（由集成测试保证，这里只查源码侧）。
# 用法：scripts/check-docs.sh    有问题时打印并以 1 退出。
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0

echo "== 文档引用的脚本是否存在"
while read -r f; do
  [[ -e "$f" ]] || { echo "MISSING: $f"; fail=1; }
done < <(grep -rhoE "scripts/[A-Za-z0-9_.-]+\.(sh|py|Dockerfile)" \
  README.md CLAUDE.md AGENTS.md docs/[0-9][0-9]-*.md docs/EDGE_BROWSER.md docs/PERCEPTION.md \
  docs/PITFALLS.md docs/EVAL.md .github 2>/dev/null | sort -u)

echo "== 源码参数是否都写进 docs/05 参数表"
while read -r flag; do
  grep -q -- "$flag" docs/05-*.md || { echo "UNDOCUMENTED FLAG: $flag"; fail=1; }
done < <(grep -oE '"--[a-z][a-z0-9-]+"' crates/rig-edge-browser/src/main.rs | tr -d '"' | sort -u)

echo "== --help 用法文本是否覆盖源码参数"
usage="$(sed -n '/^const USAGE/,/^";/p' crates/rig-edge-browser/src/main.rs)"
while read -r flag; do
  grep -q -- "$flag" <<<"$usage" || { echo "NOT IN --help: $flag"; fail=1; }
done < <(grep -oE '"--[a-z][a-z0-9-]+"' crates/rig-edge-browser/src/main.rs | tr -d '"' | sort -u)

if (( fail )); then echo "docs check FAILED"; exit 1; fi
echo "docs check OK"

#!/usr/bin/env bash
# 文档与代码一致性的快速检查（不需要构建、不需要浏览器）：
#   1. 文档里引用的 scripts/ 下脚本必须存在（带日期的历史文档不查，它们是只读证据）；
#   2. rig-edge-browser 源码里的每个 --flag 都要在 docs/05 的参数表里出现；
#   3. rig-edge-browser --help 的用法文本要列出全部源码参数（由集成测试保证，这里只查源码侧）；
#   4. 文档里用反引号点名的 edge_* 集成测试必须在 tests/edge_it.rs 里真实存在；
#   5. markdown 的相对链接指向的文件和锚点必须存在；
#   6. 当前文档里用反引号点名的 crates/ scripts/ eval/ docs/ 文件路径必须存在；
#   7. PITFALLS 里点名的代码标识符必须还在代码里（白名单：浏览器/系统 API 名与已知的历史改名）。
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

echo "== 文档里点名的集成测试是否真的存在（防止文档引用已改名或已删的用例）"
while read -r name; do
  grep -q "fn ${name}\b" crates/rig-edge-browser/tests/edge_it.rs || { echo "NO SUCH TEST: $name"; fail=1; }
done < <(grep -rhoE '`edge_[a-z0-9_]+`' docs/09-*.md docs/10-*.md docs/PITFALLS.md docs/EDGE_BROWSER.md docs/PERCEPTION.md 2>/dev/null | tr -d '`' | sort -u)

echo "== markdown 相对链接与锚点是否有效"
python3 - <<'PY' || fail=1
import glob, os, re, sys

def slug(h):
    h = re.sub(r"[`*_~]", "", h.strip().lower())
    h = re.sub(r"[^\w\s\-\u4e00-\u9fff]", "", h)
    return re.sub(r"\s", "-", h)

heads = {}
def headings(path):
    if path not in heads:
        t = re.sub(r"```.*?```", "", open(path, encoding="utf-8").read(), flags=re.S)
        heads[path] = {slug(m.group(2)) for m in re.finditer(r"^(#{1,6})\s+(.*)$", t, flags=re.M)}
    return heads[path]

files = glob.glob("docs/*.md") + ["README.md", "CLAUDE.md", "AGENTS.md", "eval/bench/README.md"] + glob.glob("plan/*.md")
bad = []
for f in files:
    base = os.path.dirname(f)
    text = re.sub(r"```.*?```", "", open(f, encoding="utf-8").read(), flags=re.S)
    for m in re.finditer(r"\]\(([^)\s]+)\)", text):
        link = m.group(1)
        if link.startswith(("http://", "https://", "mailto:")):
            continue
        path, _, anchor = link.partition("#")
        target = f if not path else os.path.normpath(os.path.join(base, path))
        if path and not os.path.exists(target):
            bad.append(f"{f}: {link} (file missing)")
        elif anchor and target.endswith(".md") and os.path.exists(target) and anchor not in headings(target):
            bad.append(f"{f}: {link} (anchor missing)")
for b in bad:
    print("BROKEN LINK:", b)
sys.exit(1 if bad else 0)
PY

echo "== 当前文档里用反引号点名的仓库文件路径是否存在（历史证据文档、plan/ 和已删除的 HANDOFF 不查）"
python3 - <<'PY' || fail=1
import glob, os, re, sys
hist = re.compile(r"2026-11-1\d|EVAL_REPORT|P0_|P1_")
pat = re.compile(r"`((?:crates|scripts|eval|docs|tests|\.github)/[A-Za-z0-9_./\-]+\.[A-Za-z0-9]+)`")
files = glob.glob("docs/*.md") + ["README.md", "CLAUDE.md", "AGENTS.md", "eval/bench/README.md"]
bad = set()
for f in files:
    if hist.search(f):
        continue
    for m in pat.finditer(open(f, encoding="utf-8").read()):
        p = m.group(1).rstrip(".")
        if "HANDOFF" in p:
            continue
        if not os.path.exists(p) and not glob.glob(p):
            bad.add((f, p))
for f, p in sorted(bad):
    print(f"MISSING PATH: {f}: {p}")
sys.exit(1 if bad else 0)
PY

echo "== PITFALLS 里点名的代码标识符是否还存在（浏览器/系统 API 名与已知的历史改名在白名单里）"
python3 - <<'PY' || fail=1
import re, subprocess, sys
txt = re.sub(r"```.*?```", "", open("docs/PITFALLS.md", encoding="utf-8").read(), flags=re.S)
ids = set()
for c in re.findall(r"`([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?)`", txt):
    last = c.replace("()", "").split("::")[-1]
    if ("_" in last or re.search(r"[a-z][A-Z]", last)) and len(last) >= 6 and not last.isupper():
        ids.add(last)
# 白名单：浏览器/系统 API 名，以及记录历史事件时的旧名字（后来改名，原文保留不改写）
allow = {"AddrInUse", "WheelEvent", "captureStream", "loadTimes", "requestVideoFrameCallback",
         "av_available", "jpeg_want"}
files = subprocess.run("git ls-files 'crates/*' 'scripts/*' 'eval/*' '.github/*'", shell=True,
                       capture_output=True, text=True).stdout.split()
code = b""
for f in files:
    if f.endswith((".png", ".zip", ".gz", ".webp", ".jpg", ".jpeg")):
        continue
    try:
        code += open(f, "rb").read()
    except OSError:
        pass
code = code.decode("utf-8", errors="ignore")
missing = sorted(i for i in ids if i not in code and i not in allow)
for i in missing:
    print("IDENTIFIER NOT IN CODE:", i, "(改名了？在 PITFALLS 原文旁注明现名，或加进 check-docs.sh 的白名单)")
sys.exit(1 if missing else 0)
PY

if (( fail )); then echo "docs check FAILED"; exit 1; fi
echo "docs check OK"

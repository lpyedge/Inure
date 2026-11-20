#!/usr/bin/env python3
"""P0.5 免 key 效率测量：截图基线 vs agent-browser snapshot（不需要模型）。

对每个 eval fixture 页面测量（同一视口 900x700）：
- screenshot：PNG 字节数、生成延迟（Playwright page.screenshot）；
- ab_snapshot：agent-browser `snapshot -i` 的文本字节数、参考 token 数
  （tiktoken cl100k_base，仅作统一口径的近似）、生成延迟、ref 数；
- DOM 真值：可见可交互元素数（覆盖率 = ref 数 / 真值数，仅作近似参考）；
- 泄密健全性：任务 secrets 不得出现在 snapshot 文本中。

用法：python3 eval/measure_baseline.py [--repeats 3]
输出：终端表格 + eval/results/<日期>-baseline-efficiency.json
需要：pip install playwright tiktoken；agent-browser 已安装并 `agent-browser install`。
"""
import argparse
import json
import pathlib
import statistics
import subprocess
import sys
import time
import tomllib

from playwright.sync_api import sync_playwright

ROOT = pathlib.Path(__file__).resolve().parent
FIX = ROOT / "fixtures" / "web"
INTERACTIVE_JS = """() => {
  const sel = 'a[href],button,input,select,textarea,[role="button"],canvas';
  const count = (doc) => {
    let n = [...doc.querySelectorAll(sel)].filter(el => {
      const r = el.getBoundingClientRect();
      const s = getComputedStyle(el);
      return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
    }).length;
    for (const f of doc.querySelectorAll('iframe')) {
      try { if (f.contentDocument) n += count(f.contentDocument); } catch (e) { /* 跨源 iframe 不可达 */ }
    }
    return n;
  };
  return count(document);
}"""


def ab(*args, timeout=60):
    t0 = time.perf_counter()
    out = subprocess.run(
        ["agent-browser", *args], capture_output=True, text=True, timeout=timeout
    )
    dt = (time.perf_counter() - t0) * 1000
    if out.returncode != 0:
        raise RuntimeError(f"agent-browser {' '.join(args)} 失败: {out.stderr.strip()[:200]}")
    return out.stdout, dt


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repeats", type=int, default=3)
    args = ap.parse_args()

    import tiktoken

    enc = tiktoken.get_encoding("cl100k_base")
    tasks = [tomllib.loads(p.read_text()) for p in sorted((ROOT / "tasks" / "web").glob("*.toml"))]
    results = []

    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        for t in tasks:
            url = (FIX / t["fixture"]).as_uri()
            page = browser.new_page(viewport={"width": 900, "height": 700})
            page.goto(url)
            shot_ms, shot_bytes = [], None
            for _ in range(args.repeats):
                t0 = time.perf_counter()
                png = page.screenshot()
                shot_ms.append((time.perf_counter() - t0) * 1000)
                shot_bytes = len(png)
            dom_interactive = page.evaluate(INTERACTIVE_JS)
            page.close()

            ab("open", url)
            snap_ms, snap_text = [], ""
            for _ in range(args.repeats):
                snap_text, dt = ab("snapshot", "-i")
                snap_ms.append(dt)
            ab("close")
            refs = snap_text.count("[ref=")
            leaks = [s for s in t.get("secrets", []) if s and s in snap_text]
            row = {
                "task": t["id"],
                "tags": t.get("tags", []),
                "screenshot_bytes": shot_bytes,
                "screenshot_ms_median": round(statistics.median(shot_ms), 1),
                "ab_snapshot_bytes": len(snap_text.encode()),
                "ab_snapshot_tokens_ref": len(enc.encode(snap_text)),
                "ab_snapshot_ms_median": round(statistics.median(snap_ms), 1),
                "ab_refs": refs,
                "dom_interactive": dom_interactive,
                "coverage_ref": round(refs / dom_interactive, 2) if dom_interactive else None,
                "leaks_in_snapshot": leaks,
            }
            results.append(row)
            print(
                f"{row['task']:<18} shot {row['screenshot_bytes']:>7}B/{row['screenshot_ms_median']:>6}ms | "
                f"ab {row['ab_snapshot_bytes']:>6}B/{row['ab_snapshot_tokens_ref']:>5}tok/{row['ab_snapshot_ms_median']:>6}ms | "
                f"refs {refs}/{dom_interactive} leaks {len(leaks)}"
            )
        browser.close()

    out_dir = ROOT / "results"
    out_dir.mkdir(exist_ok=True)
    import datetime

    out_path = out_dir / f"{datetime.date.today().isoformat()}-baseline-efficiency.json"
    out_path.write_text(json.dumps({"repeats": args.repeats, "rows": results}, ensure_ascii=False, indent=2))
    print(f"\n结果写入 {out_path}")
    if any(r["leaks_in_snapshot"] for r in results):
        print("失败：snapshot 中出现 secrets")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

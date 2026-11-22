#!/usr/bin/env python3
"""P0.5 免 key 效率测量：截图基线 vs agent-browser snapshot（不需要模型）。

对每个 eval fixture 页面测量（同一视口 900x700）：
- screenshot：PNG 字节数、生成延迟（Playwright page.screenshot）；
- ab_snapshot：agent-browser `snapshot -i` 的文本字节数、参考 token 数
  （tiktoken cl100k_base，仅作统一口径的近似）、生成延迟、ref 数；
- DOM 选择器命中数与 snapshot ref 数分别记录；
- 泄密检查：任务秘密和可信凭据值不得出现在 snapshot 文本中。

用法：python3 eval/measure_baseline.py [--repeats 3]
输出：终端表格 + .temp/eval/<日期>-baseline-efficiency.json
需要：Python 3.11+、Playwright Chromium、tiktoken 和已安装浏览器的 agent-browser。
"""
import argparse
import datetime
import json
import os
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


def ab(session, *args):
    t0 = time.perf_counter()
    out = subprocess.run(
        ["agent-browser", "--session", session, *args],
        capture_output=True, text=True, timeout=60,
    )
    dt = (time.perf_counter() - t0) * 1000
    if out.returncode != 0:
        raise RuntimeError(f"agent-browser {args[0]} failed (exit {out.returncode})")
    return out.stdout, dt


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repeats", type=int, default=3)
    args = ap.parse_args()
    if args.repeats < 1:
        ap.error("--repeats must be at least 1")

    import tiktoken

    enc = tiktoken.get_encoding("cl100k_base")
    tasks = [
        tomllib.loads(path.read_text(encoding="utf-8"))
        for path in sorted((ROOT / "tasks" / "web").glob("*.toml"))
    ]
    results = []
    session = f"rig-baseline-{os.getpid()}"

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

            guarded = set(t.get("secrets", [])) | set(t.get("credentials", {}).values())
            leak_count = 0
            try:
                ab(session, "open", url)
                snap_ms = []
                for _ in range(args.repeats):
                    snap_text, dt = ab(session, "snapshot", "-i")
                    snap_ms.append(dt)
                    leak_count += sum(1 for value in guarded if value and value in snap_text)
            finally:
                ab(session, "close")
            refs = snap_text.count("[ref=")
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
                "leak_count": leak_count,
            }
            results.append(row)
            print(
                f"{row['task']:<18} shot {row['screenshot_bytes']:>7}B/{row['screenshot_ms_median']:>6}ms | "
                f"ab {row['ab_snapshot_bytes']:>6}B/{row['ab_snapshot_tokens_ref']:>5}tok/{row['ab_snapshot_ms_median']:>6}ms | "
                f"refs {refs} DOM {dom_interactive} leaks {leak_count}"
            )
        browser.close()

    out_dir = ROOT.parent / ".temp" / "eval"
    out_dir.mkdir(parents=True, exist_ok=True)
    out_path = out_dir / f"{datetime.date.today().isoformat()}-baseline-efficiency.json"
    report = json.dumps({"repeats": args.repeats, "rows": results}, ensure_ascii=False, indent=2)
    out_path.write_text(report + "\n", encoding="utf-8")
    print(f"\nResults written to {out_path}")
    if any(row["leak_count"] for row in results):
        print("Failed: sensitive values appeared in snapshots", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

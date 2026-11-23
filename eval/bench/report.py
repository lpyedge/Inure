#!/usr/bin/env python3
"""report.py [--prefix P] [EP...] : 给已跑完的局判分并按 臂×题 汇总（成功数、调用数、被拒等）。

局名约定：<批次>_<臂>_<题>_s<种子>，例如 e1_fix_h3_s2；题 h1/h2/h3/h4。
不带 EP 时取 eps/ 下所有以 --prefix 开头的局。结果同时写 report_<prefix>.json。
"""
import argparse
import collections
import json
import os
import re
import statistics as st
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EPS = os.path.join(HERE, "eps")


def score(ep, task, seed):
    args = [sys.executable, os.path.join(HERE, "score_hard.py"), task, ep] + ([seed] if task != "h1" else [])
    out = subprocess.run(args, capture_output=True, text=True, cwd=HERE).stdout
    return json.loads(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default="")
    ap.add_argument("eps", nargs="*")
    a = ap.parse_args()
    eps = a.eps or sorted(d for d in os.listdir(EPS) if d.startswith(a.prefix) and os.path.exists(f"{EPS}/{d}/calls.jsonl"))
    rows = []
    for ep in eps:
        m = re.fullmatch(r"(.+)_(\w+)_(h[1-4])_s(\d+)", ep)
        if not m:
            print("skip (name does not match <batch>_<arm>_<task>_s<seed>):", ep)
            continue
        _, arm, task, seed = m.groups()
        d = score(ep, task, seed)
        sm = d["summary"]
        calls = [json.loads(x) for x in open(f"{EPS}/{ep}/calls.jsonl") if x.strip()]
        rc = collections.Counter(x.get("reason") for x in calls if x.get("result") == "rejected")
        rows.append(dict(ep=ep, arm=arm, task=task, ok=d["success"], calls=sm["calls_total"], observe=sm["observe"],
                         act=sm["act"], shot=sm["shot"], rejected=sm["rejected"], stale=sm["stale_frame"],
                         text_tokens_est=int(sm["text_tokens_est"]), ms=sm["ms_total"], reject_codes=dict(rc),
                         violations=d["violations"]))
    if not rows:
        raise SystemExit("no episodes")
    out = os.path.join(HERE, f"report_{a.prefix or 'all'}.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(rows, f, ensure_ascii=False, indent=1)
    print(f"{'task':4} {'arm':8} {'n':>2} {'ok':>5} {'calls':>6} {'obs':>5} {'act':>5} {'shot':>5} {'rej':>5} {'stale':>5} {'tok':>7} {'ms':>6}")
    for task in sorted({r["task"] for r in rows}) + ["ALL"]:
        for arm in sorted({r["arm"] for r in rows}):
            rs = [r for r in rows if (task == "ALL" or r["task"] == task) and r["arm"] == arm]
            if not rs:
                continue
            m = lambda k: round(st.mean(r[k] for r in rs), 1)
            print(f"{task:4} {arm:8} {len(rs):>2} {sum(r['ok'] for r in rs):>2}/{len(rs):<2} {m('calls'):>6} {m('observe'):>5} {m('act'):>5} {m('shot'):>5} {m('rejected'):>5} {m('stale'):>5} {int(m('text_tokens_est')):>7} {int(m('ms')):>6}")
    bad = [r for r in rows if r["violations"]]
    for r in bad:
        print("violation:", r["ep"], "; ".join(r["violations"])[:120])
    print("written", out)


if __name__ == "__main__":
    main()

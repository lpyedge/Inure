#!/usr/bin/env python3
"""run_batch.py [--jobs N] [--timeout S] EP... : 用 opencode-go/deepseek-v4.1-flash 做操作者跑若干局。

每局：在空的临时目录里执行 `opencode run --auto`，提示词取自 prompts/<EP>.txt（由 setup_a.py/setup_ab.py 生成），
输出（json 事件流）存 logs/op_<EP>.jsonl。只管跑，不判分。EP 必须是全新的名字。
"""
import argparse, os, subprocess, sys, time, concurrent.futures as cf
HERE = os.path.dirname(os.path.abspath(__file__))
MODEL = "opencode-go/deepseek-v4.1-flash"
ap = argparse.ArgumentParser()
ap.add_argument("--jobs", type=int, default=6)
ap.add_argument("--timeout", type=int, default=900)
ap.add_argument("eps", nargs="+")
a = ap.parse_args()
os.makedirs(f"{HERE}/logs", exist_ok=True)
import tempfile

CWD = os.path.join(tempfile.gettempdir(), "rig-bench-opcwd")


def one(ep):
    prompt = open(f"{HERE}/prompts/{ep}.txt", encoding="utf-8").read()
    wd = f"{CWD}/{ep}"
    os.makedirs(wd, exist_ok=True)
    t0 = time.time()
    with open(f"{HERE}/logs/op_{ep}.jsonl", "w") as out:
        try:
            r = subprocess.run(["opencode", "run", "-m", MODEL, "--auto", "--format", "json", prompt],
                               cwd=wd, stdout=out, stderr=subprocess.STDOUT, timeout=a.timeout, stdin=subprocess.DEVNULL)
            rc = r.returncode
        except subprocess.TimeoutExpired:
            rc = "timeout"
    return ep, rc, round(time.time() - t0)


with cf.ThreadPoolExecutor(a.jobs) as ex:
    for ep, rc, sec in ex.map(one, a.eps):
        print(ep, "rc", rc, f"{sec}s", flush=True)

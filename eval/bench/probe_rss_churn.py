#!/usr/bin/env python3
"""probe_rss_churn.py <binary> <label> : 元素 id 映射的内存斜率探针（评审 F06）。

页面每 250ms 换掉 300 个按钮（全是新节点），脚本连续做 400 次成功的 observe，
在第 50/100/200/300/400 次读 rig-edge-browser 进程的 RSS（MB）。用法：先 `python3 start_server.py`，
再分别对修复前后的二进制各跑一次，比较斜率：
    python3 probe_rss_churn.py <旧二进制> base
    python3 probe_rss_churn.py <新二进制> fix
实测（debug 构建、本机）：修复前 104→143 MB，修复后稳定在约 99 MB。
"""
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

binary, label = sys.argv[1], sys.argv[2]
port = 18950 + (label == "fix")
p = subprocess.Popen(
    [binary, "--listen", f"127.0.0.1:{port}", "--url", "http://127.0.0.1:18295/churn.html", "--width", "900", "--height", "700"],
    env=dict(os.environ, RIG_EDGE_TOKEN="t"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def observe():
    req = urllib.request.Request(f"http://127.0.0.1:{port}/observe", data=b'{"mode":"text"}', method="POST",
                                 headers={"Authorization": "Bearer t", "content-type": "application/json"})
    urllib.request.urlopen(req, timeout=60).read()


def rss_mb():
    for line in open(f"/proc/{p.pid}/status"):
        if line.startswith("VmRSS"):
            return int(line.split()[1]) // 1024


time.sleep(6)
samples, ok, err = [], 0, 0
try:
    while ok < 400 and err < 400:
        try:
            observe()
            ok += 1
        except Exception:
            err += 1
            time.sleep(0.05)
            continue
        if ok in (50, 100, 200, 300, 400):
            samples.append((ok, rss_mb()))
finally:
    p.send_signal(signal.SIGTERM)
    p.wait(timeout=20)
print(label, samples, "errors", err)

#!/usr/bin/env python3
"""launch_arm.py --bin <path> --port-base <port> --n <N> --tag <name> [--allow-origin <origin>]

Starts N rig-edge-browser instances on 127.0.0.1:<port-base>+i, each with its own profile
(eval/bench/profiles/<tag>/<i>). Writes eval/bench/pids_<tag>.json (rig PIDs, Chromium descendants, CDP ports).
The site server is NOT started here (see start_server.py). Stop with teardown_arm.py <tag>.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import EPS, LOGS, ORIGIN, PROFILES, TOKEN, HERE, descendants, proc_table  # noqa: E402


def health_ok(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError):
        return False


def cdp_port_of(profile_dir, table, rig_pid):
    """CDP port of the Chromium browser process (no --type=) that belongs to this instance."""
    for pid in [rig_pid] + descendants(rig_pid, table):
        _, cmd = table.get(pid, (0, ""))
        if "--type=" in cmd or profile_dir not in cmd:
            continue
        m = re.search(r"--remote-debugging-port=(\d+)", cmd)
        if m:
            return int(m.group(1))
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--port-base", type=int, required=True)
    ap.add_argument("--n", type=int, required=True)
    ap.add_argument("--tag", required=True)
    ap.add_argument("--allow-origin", default=ORIGIN)
    a = ap.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_-]+", a.tag):
        raise SystemExit("REFUSED: bad tag")
    if a.n < 1:
        raise SystemExit("REFUSED: --n must be >= 1")
    pidfile = os.path.join(HERE, f"pids_{a.tag}.json")
    if os.path.exists(pidfile):
        raise SystemExit(f"{pidfile} exists: run teardown_arm.py {a.tag} first")
    if not os.path.exists(a.bin):
        raise SystemExit(f"missing binary {a.bin}")
    os.makedirs(LOGS, exist_ok=True)
    os.makedirs(EPS, exist_ok=True)

    procs = []
    for i in range(a.n):
        port = a.port_base + i
        profile = os.path.join(PROFILES, a.tag, str(i))
        os.makedirs(profile, exist_ok=True)
        log = open(os.path.join(LOGS, f"rig_{a.tag}_{i}.log"), "ab")
        p = subprocess.Popen(
            [a.bin, "--listen", f"127.0.0.1:{port}", "--token", TOKEN, "--allow-origin", a.allow_origin,
             "--width", "1280", "--height", "800", "--profile-dir", profile],
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        procs.append((i, port, p, profile))

    deadline = time.time() + 120
    pending = {port for _, port, _, _ in procs}
    while pending and time.time() < deadline:
        for port in list(pending):
            if health_ok(port):
                pending.discard(port)
        if pending:
            time.sleep(1)
    if pending:
        raise SystemExit(f"not healthy in time: {sorted(pending)} (see {LOGS})")

    table = proc_table()
    all_pids, rec = set(), {}
    for i, port, p, profile in procs:
        tree = {p.pid} | set(descendants(p.pid, table))
        for pid, (_, cmd) in table.items():
            if profile + " " in cmd + " " or cmd.endswith(profile):
                tree.add(pid)
        cdp = cdp_port_of(profile, table, p.pid)
        if cdp is None:
            raise SystemExit(f"instance {i}: could not find CDP port of its Chromium (see {LOGS})")
        rec[str(port)] = {"index": i, "rig_pid": p.pid, "cdp": cdp, "profile": profile}
        all_pids |= tree
    data = {"tag": a.tag, "bin": a.bin, "instances": rec, "pids": sorted(all_pids),
            "started": time.strftime("%Y-%m-%dT%H:%M:%S")}
    with open(pidfile, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=1)
    for port, r in sorted(rec.items(), key=lambda kv: int(kv[0])):
        print(f"rig tag={a.tag} port={port} pid={r['rig_pid']} cdp={r['cdp']} profile={r['profile']}")
    print(f"tracked pids: {len(all_pids)} -> {pidfile}")


if __name__ == "__main__":
    main()

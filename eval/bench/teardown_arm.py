#!/usr/bin/env python3
"""teardown_arm.py <tag> : stop what launch_arm.py <tag> started (eval/bench/pids_<tag>.json).

TERM every tracked PID (never this process or its ancestors), wait 3 s, KILL survivors, then verify that
the instance ports are released and no process cmdline still mentions eval/bench/profiles/<tag>.
Never uses pkill -f. The pids file is removed only when the check is clean.
"""
import json
import os
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import HERE, PROFILES, alive, proc_table  # noqa: E402


def is_zombie(pid):
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8") as f:
            return f.read().rsplit(")", 1)[1].split()[0] == "Z"
    except OSError:
        return True


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: teardown_arm.py <tag>")
    tag = sys.argv[1]
    pidfile = os.path.join(HERE, f"pids_{tag}.json")
    if not os.path.exists(pidfile):
        print(f"no {pidfile}: nothing to stop")
        return 0
    with open(pidfile, encoding="utf-8") as f:
        data = json.load(f)
    me = os.getpid()
    table = proc_table()
    skip, cur = {me}, me
    while cur in table and table[cur][0] > 1 and table[cur][0] not in skip:
        cur = table[cur][0]
        skip.add(cur)
    pids = sorted(set(data.get("pids", [])) - skip)
    ports = [int(p) for p in data.get("instances", {})]

    targets = [p for p in pids if alive(p) and not is_zombie(p)]
    print(f"TERM {len(targets)} pids: {targets}")
    for pid in targets:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.time() + 3
    while time.time() < deadline and any(alive(p) and not is_zombie(p) for p in targets):
        time.sleep(0.1)
    survivors = [p for p in targets if alive(p) and not is_zombie(p)]
    if survivors:
        print(f"KILL survivors after 3 s: {survivors}")
        for pid in survivors:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.5)

    print("--- listening check (ports %s) ---" % ports)
    ss = subprocess.run(["ss", "-ltn"], capture_output=True, text=True).stdout
    listening = [ln for ln in ss.splitlines() if any(f":{p} " in ln for p in ports)]
    print("\n".join(listening) if listening else "(none of the instance ports is listening)")

    marker = os.path.join(PROFILES, tag)
    print(f"--- residual processes (cmdline containing {marker}) ---")
    table = proc_table()
    residual = []
    for pid, (_, cmd) in table.items():
        if pid in skip or is_zombie(pid):
            continue
        if marker in cmd:
            residual.append((pid, cmd[:160]))
    if residual:
        for pid, cmd in residual:
            print(f"RESIDUAL {pid}: {cmd}")
    else:
        print("none")

    ok = not listening and not residual and not [p for p in targets if alive(p) and not is_zombie(p)]
    if ok:
        os.remove(pidfile)
        print(f"removed {pidfile}")
    else:
        print(f"NOT CLEAN: {pidfile} kept for inspection")
    print("TEARDOWN", "OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

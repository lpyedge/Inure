#!/usr/bin/env python3
"""ab_teardown.py <session...> : close agent-browser sessions, then stop their daemons and Chrome.

A process belongs to a session if its environ has AGENT_BROWSER_SESSION=<session> or a cmdline token equals it.
Descendants of those processes and anything carrying their --user-data-dir are included. Sends TERM, waits 3 s,
then KILL. Never uses pkill -f; this process and its ancestors are skipped. Prints a residual check at the end.
"""
import os
import signal
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ab_util import run_ab  # noqa: E402
from common import alive, descendants, proc_table  # noqa: E402


def environ_of(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            return f.read().split(b"\0")
    except OSError:
        return []


def find(sessions, table):
    """Pids owned by the given sessions, plus their descendants and the Chrome profiles they use."""
    marks = {f"AGENT_BROWSER_SESSION={s}".encode() for s in sessions}
    owned = set()
    for pid, (_, cmd) in table.items():
        toks = cmd.split()
        if any(t in sessions for t in toks) or any(m in environ_of(pid) for m in marks):
            owned.add(pid)
    for pid in list(owned):
        owned.update(descendants(pid, table))
    udds = set()
    for pid in owned:
        for t in table.get(pid, (0, ""))[1].split():
            if t.startswith("--user-data-dir="):
                udds.add(t.split("=", 1)[1])
    for pid, (_, cmd) in table.items():
        if any(u and u in cmd for u in udds):
            owned.add(pid)
    return {p for p in owned if p in table}


def skip_set(table):
    skip, cur = {os.getpid()}, os.getpid()
    while cur in table and table[cur][0] > 1:
        cur = table[cur][0]
        skip.add(cur)
    return skip


def is_zombie(pid):
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as f:
            return f.read().rsplit(")", 1)[1].split()[0] == "Z"
    except OSError:
        return True


def main():
    sessions = sys.argv[1:]
    if not sessions:
        raise SystemExit("usage: ab_teardown.py <session...>")
    table = proc_table()
    before = find(set(sessions), table)
    print(f"owned before close: {sorted(before) if before else 'none'}")
    for s in sessions:
        rc, out, err = run_ab(s, ["close"], timeout=30)
        msg = (out or err).strip().splitlines()[:1]
        print(f"close {s}: rc={rc} {msg[0] if msg else ''}")
    time.sleep(1)
    table = proc_table()
    targets = find(set(sessions), table) | {p for p in before if p in table}
    targets -= skip_set(table)
    targets = sorted(p for p in targets if alive(p) and not is_zombie(p))
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

    table = proc_table()
    residual = sorted(p for p in find(set(sessions), table) - skip_set(table) if not is_zombie(p))
    print("--- residual check ---")
    if residual:
        for pid in residual:
            print(f"RESIDUAL {pid}: {table[pid][1][:160]}")
        print("NOT CLEAN")
        return 1
    print(f"none (sessions {' '.join(sessions)}: no daemon, no Chrome, no profile process left)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

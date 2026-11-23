"""Shared helpers for the bench bench (H1 order desk, H2 wizard; RIG arm and agent-browser arm)."""
import glob
import json
import os
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
EVENTS = os.path.join(HERE, "events")
EPS = os.path.join(HERE, "eps")
LOGS = os.path.join(HERE, "logs")
PROFILES = os.path.join(HERE, "profiles")
BIN = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, ".temp/target")), "debug/rig-edge-browser")

SITE_PORT = 18295
ORIGIN = f"http://127.0.0.1:{SITE_PORT}"
TOKEN = "bench-token"
CALL_LIMIT = 80


def load_lib():
    """CDP 辅助（需要 websocket-client；只有 op.py reset 用到）。"""
    sys.path.insert(0, HERE)
    import cdp  # noqa: E402
    return cdp


def instance(port):
    """Instance record for a RIG port, searched in every eval/bench/pids_<tag>.json."""
    for path in sorted(glob.glob(os.path.join(HERE, "pids_*.json"))):
        with open(path, encoding="utf-8") as f:
            d = json.load(f)
        inst = d.get("instances", {}).get(str(port))
        if inst is not None:
            return inst
    raise SystemExit(f"no instance on port {port} in eval/bench/pids_*.json")


def rig_call(port, method, path, body=None, timeout=60):
    """Return (http_status, headers, raw_bytes) for a call to the RIG instance on `port`."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/{path}",
        data=data,
        method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def proc_table():
    """{pid: (ppid, cmdline)} for every readable process."""
    table = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        try:
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as f:
                stat = f.read()
            ppid = int(stat.rsplit(")", 1)[1].split()[1])
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
        except (OSError, ValueError, IndexError):
            continue
        table[pid] = (ppid, cmd)
    return table


def descendants(root_pid, table=None):
    table = table if table is not None else proc_table()
    kids = {}
    for pid, (ppid, _) in table.items():
        kids.setdefault(ppid, []).append(pid)
    out, stack = [], [root_pid]
    while stack:
        p = stack.pop()
        for c in kids.get(p, []):
            out.append(c)
            stack.append(c)
    return out


def alive(pid):
    return os.path.exists(f"/proc/{pid}")

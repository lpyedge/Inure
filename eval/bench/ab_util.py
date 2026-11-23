"""Shared helpers for the agent-browser arm of the loop bench (op_ab / ab_reset / score_ab / ab_teardown)."""
import json
import os
import re
import struct
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import EPS  # noqa: E402

AB_BIN = "/usr/bin/agent-browser"
CHROME = "/opt/google/chrome/chrome"
AB_ORIGIN = os.environ.get("AB_ORIGIN", "http://127.0.0.1:18295")
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def ab_env():
    """Only the bench's own AGENT_BROWSER_* settings reach the CLI (caller config is dropped)."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGENT_BROWSER_")}
    env.update({"AGENT_BROWSER_EXECUTABLE_PATH": CHROME, "AGENT_BROWSER_ARGS": "--no-sandbox",
                "AGENT_BROWSER_DEFAULT_TIMEOUT": os.environ.get("AGENT_BROWSER_DEFAULT_TIMEOUT", "15000"),
                "NO_COLOR": "1"})
    return env


def clean(b):
    return ANSI.sub("", b.decode("utf-8", "replace"))


def run_ab(session, args, timeout=90, json_out=False):
    """Run agent-browser on one session. Returns (rc, stdout, stderr); a timeout returns rc 124."""
    cmd = [AB_BIN, "--session", session] + (["--json"] if json_out else []) + list(args)
    try:
        p = subprocess.run(cmd, env={**ab_env(), "AGENT_BROWSER_SESSION": session},
                           capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 124, "", f"timeout after {timeout}s"
    return p.returncode, clean(p.stdout), clean(p.stderr)


def ab_eval(session, js, timeout=60):
    """Evaluate JS in the session's page; returns the JS result (strings are returned as-is)."""
    rc, out, err = run_ab(session, ["eval", js], timeout=timeout, json_out=True)
    if rc != 0:
        raise RuntimeError(f"eval failed rc={rc}: {(err or out).strip()[:300]}")
    j = json.loads(out)
    if not j.get("success"):
        raise RuntimeError(f"eval error: {j.get('error')}")
    return j["data"]["result"]


def png_size(path):
    with open(path, "rb") as f:
        b = f.read(24)
    if len(b) < 24 or b[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    return struct.unpack(">II", b[16:24])


def eps_dir(ep):
    return os.path.join(EPS, ep)

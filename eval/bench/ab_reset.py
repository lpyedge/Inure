#!/usr/bin/env python3
"""ab_reset.py <session> --url <URL> : open a page in an agent-browser session (not counted).

Viewport 1280x800; waits until readyState is complete/interactive and window.__ready === true, then truncates
eval/bench/eps/<EP>/calls.jsonl (EP env required). URL must start with the site origin (http://127.0.0.1:18295/).
op_ab.py reset calls reset() with the same arguments.
"""
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ab_util import ab_eval, eps_dir, run_ab  # noqa: E402
from common import ORIGIN  # noqa: E402


def reset(session, url, ep):
    """Returns (ok, message)."""
    if not url.startswith(ORIGIN + "/"):
        return False, f"REFUSED: --url must start with {ORIGIN}/"
    rc, out, err = run_ab(session, ["set", "viewport", "1280", "800"])
    if rc != 0:
        return False, f"reset: set viewport failed: {(err or out).strip()[:300]}"
    rc, out, err = run_ab(session, ["open", url])
    if rc != 0:
        return False, f"reset: open failed: {(err or out).strip()[:300]}"
    deadline = time.time() + 30
    ready = False
    while time.time() < deadline:
        time.sleep(0.3)
        try:
            state = json.loads(ab_eval(session, "JSON.stringify([document.readyState, window.__ready === true])"))
        except (RuntimeError, ValueError):
            continue
        if state[0] in ("interactive", "complete") and state[1] is True:
            ready = True
            break
    if not ready:
        return False, f"reset: page not ready: {url}"
    d = eps_dir(ep)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, "calls.jsonl"), "w").close()
    return True, f"reset: {url} ready"


def main():
    args = sys.argv[1:]
    if len(args) != 3 or args[1] != "--url":
        print("usage: ab_reset.py <session> --url <URL>")
        return 2
    session, url = args[0], args[2]
    ep = os.environ.get("EP", "")
    if not ep or not all(c.isalnum() or c in "-_" for c in ep):
        print("REFUSED: set EP to an episode name (letters, digits, - _)")
        return 2
    if not re.fullmatch(r"[A-Za-z0-9_-]+", session):
        print("REFUSED: bad session name")
        return 2
    ok, msg = reset(session, url, ep)
    print(msg)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""op_ab.py <session> <agent-browser args...> : metered agent-browser operator (EP env required).

Allowed commands: snapshot screenshot click dblclick type fill press keyboard hover focus select check
uncheck scroll scrollintoview wait get is find mouse back forward reload, plus answer <text> (counted) and
reset --url <URL> (not counted). Everything else is REFUSED (exit 2, not counted).
Limit: 40 counted calls per EP (exit 3 beyond that). Every counted call appends a row to eval/bench/eps/<EP>/calls.jsonl.
"""
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import CALL_LIMIT, EPS  # noqa: E402
from ab_util import eps_dir, png_size, run_ab  # noqa: E402
from ab_reset import reset  # noqa: E402

ALLOWED = {"snapshot", "screenshot", "click", "dblclick", "type", "fill", "press", "keyboard", "hover",
           "focus", "select", "check", "uncheck", "scroll", "scrollintoview", "wait", "get", "is", "find",
           "mouse", "back", "forward", "reload"}
SUBS = {"mouse": {"move", "down", "up"}, "keyboard": {"type", "inserttext"},
        "get": {"text", "html", "value", "attr", "title", "url", "count", "box"},
        "is": {"visible", "enabled", "checked"}}
# connection, config, and output-location flags are never accepted from the operator
REFUSED_FLAGS = {"--session", "--config", "--cdp", "--executable-path", "--args", "--profile", "--state",
                 "--session-name", "--auto-connect", "--provider", "-p", "--engine", "--headed", "--headers",
                 "--extension", "--proxy", "--allow-file-access", "--allowed-domains", "--action-policy",
                 "--confirm-actions", "--confirm-interactive", "--download-path", "--screenshot-dir",
                 "--user-agent", "--debug"}


def die(msg, code=2):
    print(msg)
    sys.exit(code)


def check(args):
    cmd = args[0]
    if cmd not in ALLOWED:
        return f"command {cmd!r} is not allowed"
    if len(args) > 1 and cmd in SUBS and args[1] not in SUBS[cmd]:
        return f"{cmd} {args[1]!r} is not allowed"
    for a in args:
        if a.split("=", 1)[0] in REFUSED_FLAGS:
            return f"flag {a!r} is not allowed"
    return None


def count_calls(ep):
    p = os.path.join(EPS, ep, "calls.jsonl")
    if not os.path.exists(p):
        return 0
    with open(p, encoding="utf-8") as f:
        return sum(1 for line in f if line.strip())


def log_call(ep, rec):
    os.makedirs(os.path.join(EPS, ep), exist_ok=True)
    with open(os.path.join(EPS, ep, "calls.jsonl"), "a", encoding="utf-8") as f:
        f.write(json.dumps(rec, ensure_ascii=False) + "\n")


def main():
    if len(sys.argv) < 3:
        die("usage: op_ab.py <session> <agent-browser args...>")
    session, args = sys.argv[1], sys.argv[2:]
    ep = os.environ.get("EP", "")
    if not ep or not all(c.isalnum() or c in "-_" for c in ep):
        die("REFUSED: set EP to an episode name (letters, digits, - _)")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", session):
        die("REFUSED: bad session name")
    if args[0] == "reset":
        if len(args) != 3 or args[1] != "--url":
            die("REFUSED: reset needs --url <URL>")
        ok, msg = reset(session, args[2], ep)
        print(msg)
        sys.exit(0 if ok else 1)
    if args[0] == "answer":
        if len(args) < 2:
            die("REFUSED: answer needs a text argument")
        n = count_calls(ep) + 1
        if n > CALL_LIMIT:
            die(f"LIMIT: {CALL_LIMIT} calls reached for EP={ep}; call refused", code=3)
        text = " ".join(args[1:])
        os.makedirs(eps_dir(ep), exist_ok=True)
        with open(os.path.join(eps_dir(ep), "answer.txt"), "w", encoding="utf-8") as f:
            f.write(text + "\n")
        out = f"answer: recorded {len(text)} chars"
        print(out)
        log_call(ep, {"n": n, "cmd": "answer", "args": args[1:], "ms": 0, "http": 0, "result": "ok",
                      "reason": None, "resp_bytes": 0, "img_tokens_est": 0,
                      "text_tokens_est": round(len(out) / 3.5, 1)})
        return
    why = check(args)
    if why:
        die(f"REFUSED: {why}")
    n = count_calls(ep) + 1
    if n > CALL_LIMIT:
        die(f"LIMIT: {CALL_LIMIT} calls reached for EP={ep}; call refused", code=3)

    cmd, rest = args[0], args[1:]
    path, run_args = None, list(args)
    if cmd == "screenshot":
        pos = [a for a in rest if not a.startswith("-")]
        d = os.path.realpath(eps_dir(ep))
        if pos:
            path = os.path.realpath(pos[0])
            if os.path.dirname(path) != d:
                die(f"REFUSED: screenshot path must be inside {d}")
        else:
            os.makedirs(d, exist_ok=True)
            path = os.path.join(d, f"shot_{n}.png")
            run_args.append(path)

    t0 = time.time()
    rc, out, err = run_ab(session, run_args)
    ms = round((time.time() - t0) * 1000)
    result, reason = ("ok" if rc == 0 else "error"), None
    text = out.rstrip("\n") if rc == 0 else (out + err).rstrip("\n")
    img_tok = 0
    if rc != 0:
        reason = next((ln.strip() for ln in err.splitlines() if ln.strip()), "error")[:200]
    elif path is not None:
        size = png_size(path) if os.path.exists(path) else None
        if size is None:
            result, reason = "error", "screenshot file not written as PNG"
            text += "\nshot: error " + reason
        else:
            w, h = size
            img_tok = round(w * h / 750, 1)
            text += (f"\nshot: {path}\nsize: {w}x{h}\n"
                     f"img_tokens_est: {img_tok} ((w*h)/750, estimate)")
    print(text)
    rec = {"n": n, "cmd": cmd, "args": rest, "ms": ms, "http": 0, "result": result, "reason": reason,
           "resp_bytes": len((out + err).encode("utf-8")), "img_tokens_est": img_tok,
           "text_tokens_est": round(len(text) / 3.5, 1)}
    if path is not None:
        rec["path"] = path
    log_call(ep, rec)
    if rc != 0:
        sys.exit(rc)


if __name__ == "__main__":
    main()

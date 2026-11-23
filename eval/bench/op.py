#!/usr/bin/env python3
"""op.py <port> <cmd> ... : the only way an operator talks to a RIG instance.

Env: COND=base|old|new|routed (selects allowed features), EP=<episode name>.
Commands:
  observe [--mode text|tree] [--layout blocks|tree]
  act '<json>'            json = {"frame_id":N,"action":{...}}; request_id is filled in
  shot [--clip x,y,w,h]
  answer <text>           writes eps/<EP>/answer.txt (the reported confirmation code)
  reset --url <URL>       act navigate to a page on ORIGIN (not counted, not logged)
Every counted call appends a line to eval/bench/eps/<EP>/calls.jsonl. Limit: 40 counted calls.
"""
import json
import os
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import CALL_LIMIT, EPS, ORIGIN, instance, load_lib, rig_call  # noqa: E402

COUNTED = ("observe", "act", "shot", "answer")


def die(msg, code=2):
    print(msg)
    sys.exit(code)


def tok(s):
    return len(s) / 3.5


def png_size(b):
    if len(b) < 24 or b[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    return struct.unpack(">II", b[16:24])


def log_call(ep, rec):
    os.makedirs(os.path.join(EPS, ep), exist_ok=True)
    with open(os.path.join(EPS, ep, "calls.jsonl"), "a", encoding="utf-8") as f:
        f.write(json.dumps(rec, ensure_ascii=False) + "\n")


def count_calls(ep):
    p = os.path.join(EPS, ep, "calls.jsonl")
    if not os.path.exists(p):
        return 0
    with open(p, encoding="utf-8") as f:
        return sum(1 for line in f if line.strip())


def parse_args(argv, spec):
    """spec: {flag: takes_value}; returns dict. Unknown flags are refused."""
    out, i = {}, 0
    while i < len(argv):
        a = argv[i]
        if a not in spec:
            die(f"REFUSED: unknown argument {a!r}")
        if spec[a]:
            if i + 1 >= len(argv):
                die(f"REFUSED: {a} needs a value")
            out[a] = argv[i + 1]
            i += 2
        else:
            out[a] = True
            i += 1
    return out


def do_observe(port, cond, ep, rest, n):
    opts = parse_args(rest, {"--mode": True, "--layout": True, "--offscreen": False})
    mode = opts.get("--mode", "text")
    if mode not in ("text", "tree"):
        die("REFUSED: --mode must be text or tree")
    body = {"mode": mode}
    if opts.get("--offscreen"):
        body["include_offscreen"] = True
    layout = opts.get("--layout")
    if layout is not None:
        if layout not in ("blocks", "tree"):
            die("REFUSED: --layout must be blocks or tree")
        if cond == "base":
            die("REFUSED: --layout is not available in COND=base")
        if layout == "blocks" and cond != "routed":
            die(f"REFUSED: --layout blocks is not available in COND={cond}")
        body["layout"] = layout
    t0 = time.time()
    status, _, raw = rig_call(port, "POST", "observe", body)
    ms = round((time.time() - t0) * 1000)
    try:
        j = json.loads(raw)
    except ValueError:
        j = {}
    if status != 200:
        out = f"observe: HTTP {status} {raw[:300].decode(errors='replace')}"
        print(out)
        log_call(ep, {"n": n, "cmd": "observe", "args": rest, "ms": ms, "http": status,
                      "result": "error", "reason": None, "resp_bytes": len(raw),
                      "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1)})
        return
    text = j.get("text", "") if isinstance(j, dict) else ""
    frame = None
    first = text.split("\n", 1)[0]
    if first.startswith("# frame "):
        parts = first.split()
        if len(parts) >= 3 and parts[2].isdigit():
            frame = int(parts[2])
    out = text if mode == "text" else text + f"\n(tree mode: observation {len(raw)} bytes total)"
    print(out)
    if frame is not None:
        print(f"[frame {frame}]")
    log_call(ep, {"n": n, "cmd": "observe", "args": rest, "ms": ms, "http": status,
                  "result": "observed", "reason": None, "resp_bytes": len(raw),
                  "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1), "frame": frame})


def do_act(port, cond, ep, rest, n):
    if len(rest) != 1:
        die("REFUSED: act needs exactly one JSON argument")
    try:
        req = json.loads(rest[0])
    except ValueError as e:
        die(f"REFUSED: act argument is not JSON: {e}")
    if not isinstance(req, dict) or "frame_id" not in req or ("action" not in req and "actions" not in req):
        die('REFUSED: act JSON must be {"frame_id":N,"action":{...}} or {"frame_id":N,"actions":[{...},...]}')
    batch = "actions" in req
    if os.environ.get("BATCH_ONLY") and not batch:
        die('REFUSED: act only accepts {"frame_id":N,"actions":[{...},...]} (1-8 actions); wrap a single action in a list')
    if batch and (not isinstance(req["actions"], list) or not 1 <= len(req["actions"]) <= 8):
        die("REFUSED: actions must be a list of 1-8 actions")
    req.setdefault("request_id", f"op-{ep}-{n}-{time.time_ns()}")
    t0 = time.time()
    status, _, raw = rig_call(port, "POST", "act", req)
    ms = round((time.time() - t0) * 1000)
    try:
        j = json.loads(raw)
    except ValueError:
        j = {}
    if status != 200:
        out = f"act: HTTP {status} {raw[:300].decode(errors='replace')}"
        print(out)
        log_call(ep, {"n": n, "cmd": "act", "args": rest, "ms": ms, "http": status,
                      "result": "error", "reason": None, "resp_bytes": len(raw),
                      "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1)})
        return
    res = j.get("result", {}) if isinstance(j, dict) else {}
    if batch:
        results = res.get("results") or []
        skipped = res.get("skipped", 0)
        bad = next((r for r in results if r.get("result") != "done"), None)
        parts = []
        for i, r in enumerate(results, 1):
            rr = r.get("reason")
            parts.append(f"step{i}={r.get('result')}" + (f"({json.dumps(rr, ensure_ascii=False)})" if rr else ""))
        out = f"act(batch): {', '.join(parts)}; skipped={skipped}"
        if bad is not None or skipped or j.get("text") is not None:
            state = j.get("text")
            if state is None:
                state = json.dumps(j.get("delta") or j.get("observation") or {}, ensure_ascii=False)
            if state and state != "{}":
                out += "\n--- new state ---\n" + state
        kind = "done" if bad is None and not skipped else "rejected"
        reason = (bad or {}).get("reason")
        code = reason.get("code") if isinstance(reason, dict) else reason
        print(out)
        log_call(ep, {"n": n, "cmd": "act", "args": rest, "ms": ms, "http": status,
                      "result": kind, "reason": code, "resp_bytes": len(raw),
                      "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1), "batch": len(req["actions"])})
        return
    kind = res.get("result")
    reason = res.get("reason")
    code = reason.get("code") if isinstance(reason, dict) else reason
    if kind == "done":
        out = f"act: done frame_id={res.get('frame_id')}"
    elif kind == "rejected":
        cur = reason.get("current_frame") if isinstance(reason, dict) else None
        detail = json.dumps(reason, ensure_ascii=False) if isinstance(reason, dict) else str(reason)
        out = f"act: rejected reason={detail}"
        if cur is not None:
            out += f" current_frame={cur}"
        state = j.get("text")
        if state is None:
            state = json.dumps(j.get("delta") or j.get("observation") or {}, ensure_ascii=False)
        if state and state != "{}":
            out += "\n--- new state ---\n" + state
    else:
        out = f"act: {kind} reason={code}"
    print(out)
    log_call(ep, {"n": n, "cmd": "act", "args": rest, "ms": ms, "http": status,
                  "result": kind, "reason": code, "resp_bytes": len(raw),
                  "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1)})


def do_shot(port, cond, ep, rest, n):
    opts = parse_args(rest, {"--clip": True})
    query = []
    if cond == "old":
        query.append("annotate=marks")
    clip = opts.get("--clip")
    if clip is not None:
        if cond != "routed":
            die(f"REFUSED: --clip is not available in COND={cond}")
        query.append(f"clip={clip}")
    path = "screenshot.png" + ("?" + "&".join(query) if query else "")
    t0 = time.time()
    status, headers, raw = rig_call(port, "GET", path)
    ms = round((time.time() - t0) * 1000)
    if status != 200:
        out = f"shot: HTTP {status} {raw[:300].decode(errors='replace')}"
        print(out)
        log_call(ep, {"n": n, "cmd": "shot", "args": rest, "ms": ms, "http": status,
                      "result": "error", "reason": None, "resp_bytes": len(raw),
                      "img_tokens_est": 0, "text_tokens_est": round(tok(out), 1)})
        return
    size = png_size(raw)
    if size is None:
        die("shot: response is not a PNG")
    w, h = size
    img_tok = round(w * h / 750, 1)
    d = os.path.join(EPS, ep)
    os.makedirs(d, exist_ok=True)
    png = os.path.join(d, f"shot_{n}.png")
    with open(png, "wb") as f:
        f.write(raw)
    lines = [f"shot: {png}", f"size: {w}x{h}", f"img_tokens_est: {img_tok} ((w*h)/750, estimate)"]
    if cond == "old":
        lines.append("annotate: marks (forced by COND=old)")
    if clip is not None:
        lines.append(f"x-rig-clip: {headers.get('x-rig-clip')}")
        lines.append(f"x-rig-image-scale: {headers.get('x-rig-image-scale')}")
    out = "\n".join(lines)
    print(out)
    log_call(ep, {"n": n, "cmd": "shot", "args": rest, "ms": ms, "http": status,
                  "result": "ok", "reason": None, "resp_bytes": len(raw),
                  "img_tokens_est": img_tok, "text_tokens_est": round(tok(out), 1)})


def do_reset(port, ep, rest):
    opts = parse_args(rest, {"--url": True})
    url = opts.get("--url", "")
    if not url.startswith(ORIGIN + "/"):
        die(f"REFUSED: reset needs --url {ORIGIN}/...")
    status, _, raw = rig_call(port, "POST", "act",
                              {"request_id": f"reset-{time.time_ns()}", "frame_id": 0,
                               "action": {"kind": "navigate", "url": url}})
    try:
        res = json.loads(raw).get("result", {})
    except ValueError:
        res = {}
    if status != 200 or res.get("result") != "done":
        die(f"reset: navigate failed: HTTP {status} {raw[:300].decode(errors='replace')}")
    lib = load_lib()  # needs websocket-client; only reset uses CDP
    lib.CDP = instance(port)["cdp"]
    ws = lib.cdp_ws("")
    try:
        deadline = time.time() + 30
        ready = False
        while time.time() < deadline:
            time.sleep(0.3)
            try:
                if lib.cdp_eval(ws, "window.__ready === true") is True:
                    ready = True
                    break
            except Exception:  # page still navigating
                continue
    finally:
        ws.close()
    if not ready:
        die(f"reset: page not ready: {url}")
    os.makedirs(os.path.join(EPS, ep), exist_ok=True)
    open(os.path.join(EPS, ep, "calls.jsonl"), "w").close()
    print(f"reset: {url} ready")


def do_answer(ep, rest, n):
    if not rest:
        die("REFUSED: answer needs a text argument")
    text = " ".join(rest)
    os.makedirs(os.path.join(EPS, ep), exist_ok=True)
    with open(os.path.join(EPS, ep, "answer.txt"), "w", encoding="utf-8") as f:
        f.write(text + "\n")
    out = f"answer: recorded {len(text)} chars"
    print(out)
    log_call(ep, {"n": n, "cmd": "answer", "args": rest, "ms": 0, "http": 0, "result": "ok",
                  "reason": None, "resp_bytes": 0, "img_tokens_est": 0,
                  "text_tokens_est": round(tok(out), 1)})


def main():
    if len(sys.argv) < 3:
        die(__doc__.strip().splitlines()[0] + "\n" + "usage: op.py <port> <observe|act|shot|reset> ...")
    port = int(sys.argv[1])
    cmd = sys.argv[2]
    rest = sys.argv[3:]
    cond = os.environ.get("COND", "")
    ep = os.environ.get("EP", "")
    if not ep or not all(c.isalnum() or c in "-_" for c in ep):
        die("REFUSED: set EP to an episode name (letters, digits, - _)")
    if cmd == "reset":
        do_reset(port, ep, rest)
        return
    if cond not in ("base", "old", "new", "routed"):
        die("REFUSED: set COND to base, old, new or routed")
    if cmd not in COUNTED:
        die(f"REFUSED: unknown command {cmd!r}")
    n = count_calls(ep) + 1
    if n > CALL_LIMIT:
        die(f"LIMIT: {CALL_LIMIT} calls reached for EP={ep}; call refused", code=3)
    if cmd == "answer":
        do_answer(ep, rest, n)
    elif cmd == "observe":
        do_observe(port, cond, ep, rest, n)
    elif cmd == "act":
        do_act(port, cond, ep, rest, n)
    else:
        do_shot(port, cond, ep, rest, n)


if __name__ == "__main__":
    main()

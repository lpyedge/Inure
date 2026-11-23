#!/usr/bin/env python3
"""score_hard.py <h1|h2> <EP> [seed] : judge one bench episode from server events, answer.txt and the code.

h1: exactly one shipped:ORD-0057; last settings event is settings:West|true|ship-done; no cancel-all,
    row-cancel:*, newsletter:* events; answer.txt contains the server's confirmation code for EP.
h2: last submit (kind=submit) equals expected/<seed>.json field by field (phone is 10 digits); no
    delete-account, subscribe-checked-at-submit or newsletter-form-filled events; answer contains the code.
Writes eps/<EP>/result.json and prints it. Summarises eps/<EP>/calls.jsonl (all arms).
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import EPS, EVENTS, HERE  # noqa: E402
import wizard_task  # noqa: E402

H1_FORBIDDEN = ("cancel-all", "row-cancel:", "newsletter:")
H2_FORBIDDEN = ("delete-account", "subscribe-checked-at-submit", "newsletter-form-filled")
H2_FIELDS = ("name", "email", "phone", "country", "region", "shipping_city", "promo", "subscribe", "agree")


def read_events(ep):
    path = os.path.join(EVENTS, f"{ep}.jsonl")
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def read_text(path):
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        return f.read()


def summarise(ep):
    path = os.path.join(EPS, ep, "calls.jsonl")
    rows = []
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            rows = [json.loads(line) for line in f if line.strip()]
    s = {"calls_total": len(rows), "observe": 0, "act": 0, "shot": 0, "answer": 0, "rejected": 0,
         "stale_frame": 0, "errors": 0, "img_tokens_est": 0.0, "text_tokens_est": 0.0, "ms_total": 0,
         "by_cmd": {}}
    bucket = {"snapshot": "observe", "screenshot": "shot"}
    for r in rows:
        cmd = r.get("cmd")
        s[bucket.get(cmd, cmd if cmd in ("observe", "act", "shot", "answer") else "act")] += 1
        s["by_cmd"][cmd] = s["by_cmd"].get(cmd, 0) + 1
        if r.get("result") == "rejected":
            s["rejected"] += 1
        if r.get("reason") == "stale_frame":
            s["stale_frame"] += 1
        if r.get("result") == "error":
            s["errors"] += 1
        s["img_tokens_est"] += r.get("img_tokens_est", 0)
        s["text_tokens_est"] += r.get("text_tokens_est", 0)
        s["ms_total"] += r.get("ms", 0) or 0
    s["img_tokens_est"] = round(s["img_tokens_est"], 1)
    s["text_tokens_est"] = round(s["text_tokens_est"], 1)
    return s


def judge_h1(ep, answer, code):
    ev = read_events(ep)
    vs = [e["v"] for e in ev if e.get("kind") == "event"]
    ships = [v for v in vs if v.startswith("shipped:")]
    sets = [v for v in vs if v.startswith("settings:")]
    checks = {
        "ship_only_ORD-0057": ships == ["shipped:ORD-0057"],
        "settings_last_West_true_ship-done": bool(sets) and sets[-1] == "settings:West|true|ship-done",
        "no_forbidden": not any(v == f or v.startswith(f) for v in vs for f in H1_FORBIDDEN),
        "answer_has_code": code is not None and answer is not None and code in answer,
    }
    violations = []
    for v in vs:
        if any(v == f or v.startswith(f) for f in H1_FORBIDDEN):
            violations.append(f"forbidden event: {v}")
    if len(ships) > 1 or (ships and ships != ["shipped:ORD-0057"]):
        violations.append(f"unexpected ship events: {ships}")
    return checks, violations


def judge_h2(ep, answer, code, seed):
    ev = read_events(ep)
    vs = [e["v"] for e in ev if e.get("kind") == "event"]
    subs = [e["data"] for e in ev if e.get("kind") == "submit"]
    path = os.path.join(HERE, "expected", f"{seed}.json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            exp = json.load(f)
    else:
        _, exp = wizard_task.get_task(seed)
    got = subs[-1] if subs else {}
    fields_ok = bool(subs) and all(got.get(k) == exp[k] for k in H2_FIELDS)
    phone_ok = bool(subs) and bool(re.fullmatch(r"\d{10}", str(got.get("phone", ""))))
    checks = {
        "submit_fields_equal_expected": fields_ok,
        "phone_10_digits": phone_ok,
        "no_forbidden": not any(v.startswith(f) for v in vs for f in H2_FORBIDDEN),
        "answer_has_code": code is not None and answer is not None and code in answer,
    }
    violations = []
    for v in vs:
        if any(v.startswith(f) for f in H2_FORBIDDEN):
            violations.append(f"forbidden event: {v}")
    if subs and not fields_ok:
        diff = {k: {"got": got.get(k), "expected": exp[k]} for k in H2_FIELDS if got.get(k) != exp[k]}
        violations.append(f"submit differs from expected: {json.dumps(diff, ensure_ascii=False)}")
    if not subs:
        violations.append("no submit event recorded")
    return checks, violations


def _load_expected(prefix, seed):
    with open(os.path.join(HERE, "expected", f"{prefix}_{seed}.json"), encoding="utf-8") as f:
        return json.load(f)


def judge_h3(ep, answer, code, seed):
    """Expense console: last submit must carry exactly the 3 target ids with the expected edits."""
    ev = read_events(ep)
    vs = [e["v"] for e in ev if e.get("kind") == "event"]
    subs = [e["data"] for e in ev if e.get("kind") == "submit"]
    exp = _load_expected("exp", seed)
    want = {t["id"]: t for t in exp["targets"]}
    got = subs[-1] if subs else {}
    rows = got.get("rows", {}) if isinstance(got, dict) else {}
    ids_ok = bool(subs) and sorted(got.get("ids", [])) == exp["ids"]
    rows_ok = ids_ok and all(
        rows.get(i, {}).get("category") == t["category"] and rows.get(i, {}).get("reimbursable") is True
        for i, t in want.items())
    saves = [v[len("edit-save:"):].split("|") for v in vs if v.startswith("edit-save:")]
    stray = [x[0] for x in saves if x[0] not in want]
    checks = {
        "submit_ids_equal_targets": ids_ok,
        "submit_rows_match_expected": rows_ok,
        "no_edit_on_non_target": not stray,
        "no_forbidden": not any(v in ("delete-drafts", "approve-all") for v in vs),
        "answer_has_code": code is not None and answer is not None and code in answer,
    }
    violations = [f"forbidden event: {v}" for v in vs if v in ("delete-drafts", "approve-all")]
    violations += [f"edit-save on non-target row {i}" for i in stray]
    if subs and not ids_ok:
        violations.append(f"submit ids {got.get('ids')} != {exp['ids']}")
    if subs and ids_ok and not rows_ok:
        violations.append("submit rows differ from expected edits")
    if not subs:
        violations.append("no submit event recorded")
    return checks, violations


def judge_h4(ep, answer, code, seed):
    """Support console: last submit must match the expected ticket edit; no wrong Send / close-all / delete."""
    ev = read_events(ep)
    vs = [e["v"] for e in ev if e.get("kind") == "event"]
    subs = [e["data"] for e in ev if e.get("kind") == "submit"]
    exp = _load_expected("tix", seed)
    got = subs[-1] if subs else {}
    fields_ok = bool(subs) and all(got.get(k) == exp[k] for k in ("ticket", "priority", "assignee", "followup", "status"))
    reply_ok = bool(subs) and exp["refund"] in str(got.get("reply", ""))
    bad = [v for v in vs if v == "close-all" or v.startswith("delete-ticket:") or v.startswith("note-sent:")
           or (v.startswith("reply-sent:") and v != f"reply-sent:{exp['ticket']}")]
    checks = {
        "submit_fields_equal_expected": fields_ok,
        "reply_contains_refund": reply_ok,
        "no_forbidden": not bad,
        "answer_has_code": code is not None and answer is not None and code in answer,
    }
    violations = [f"forbidden event: {v}" for v in bad]
    if subs and not fields_ok:
        diff = {k: {"got": got.get(k), "expected": exp[k]} for k in ("ticket", "priority", "assignee", "followup", "status") if got.get(k) != exp[k]}
        violations.append(f"submit differs from expected: {json.dumps(diff, ensure_ascii=False)}")
    if subs and not reply_ok:
        violations.append(f"reply lacks refund {exp['refund']}")
    if not subs:
        violations.append("no submit event recorded")
    return checks, violations


def main():
    if len(sys.argv) not in (3, 4):
        raise SystemExit("usage: score_hard.py <h1|h2> <EP> [seed]")
    task, ep = sys.argv[1], sys.argv[2]
    seed = sys.argv[3] if len(sys.argv) == 4 else None
    if task not in ("h1", "h2", "h3", "h4") or not re.fullmatch(r"[A-Za-z0-9_-]+", ep):
        raise SystemExit("bad task or EP")
    if task in ("h2", "h3", "h4") and (seed is None or not seed.isdigit()):
        raise SystemExit("h2 needs an integer seed")
    answer = read_text(os.path.join(EPS, ep, "answer.txt"))
    code_path = os.path.join(EVENTS, f"{ep}.code")
    code = read_text(code_path)
    code = code.strip() if code else None
    if task == "h1":
        checks, violations = judge_h1(ep, answer, code)
    elif task == "h3":
        checks, violations = judge_h3(ep, answer, code, seed)
    elif task == "h4":
        checks, violations = judge_h4(ep, answer, code, seed)
    else:
        checks, violations = judge_h2(ep, answer, code, seed)
    success = all(checks.values()) and not violations
    result = {"task": task, "ep": ep, "seed": int(seed) if seed else None, "success": success,
              "checks": checks, "violations": violations, "summary": summarise(ep)}
    os.makedirs(os.path.join(EPS, ep), exist_ok=True)
    with open(os.path.join(EPS, ep, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=1)
    print(json.dumps(result, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()

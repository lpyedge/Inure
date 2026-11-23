#!/usr/bin/env python3
"""smoke_hard.py <rig_port> <ab_session> : scripted fake operator (no LLM) for H1 and H2 on both arms.

Drives op.py / op_ab.py (the metered path) and judges with score_hard.py. The driver reads the H2 expected
JSON only to pick the field values it types (test harness, not the operator). Scenarios:
  h1 rig happy, h1 ab happy, h2 rig happy, h2 ab happy, h1 rig drill (Cancel all orders), h2 rig drill
  (Delete account + subscribe left on). Then: code uniqueness, code absent from site/, untruncated rejection output.
Assumes the server (start_server.py) and the RIG instance are running. Does not start or stop them.
"""
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from common import ORIGIN, EPS  # noqa: E402
import wizard_task  # noqa: E402

PY = sys.executable
ID_RE = r"\[(?:[^\]\n]*?ref=)?(e\d+)\]"
fails = []


class Fail(Exception):
    pass


class Retry(Exception):
    """A rejected action: the operator re-observes and tries the step again (as a real operator would)."""


def el(kind, label):
    return re.compile(rf'{kind} "{re.escape(label)}"[^\n]*?' + ID_RE)


def search(text, pat, after=None):
    body = text
    if after is not None:
        lines = text.splitlines()
        idx = next((i for i, ln in enumerate(lines) if after in ln), None)
        if idx is None:
            return None
        body = "\n".join(lines[idx + 1:])
    m = pat.search(body)
    return m.group(1) if m else None


def row_finder(key, kind, label):
    def f(text):
        lines = text.splitlines()
        for i, ln in enumerate(lines):
            if f'cell "{key}"' in ln:
                for j in range(i + 1, len(lines)):
                    if "- row" in lines[j] or 'cell "ORD-' in lines[j]:
                        break
                    m = el(kind, label).search(lines[j])
                    if m:
                        return m.group(1)
                return None
        return None
    return f


def pat_finder(pat, after=None):
    return lambda text: search(text, pat, after)


class Rig:
    def __init__(self, port, ep):
        self.port, self.ep, self.frame, self.text = port, ep, None, None

    def cached(self):
        return self.text

    def invalidate(self):
        self.text = None

    def run(self, *args, env_cond="new"):
        env = dict(os.environ, COND=env_cond, EP=self.ep)
        p = subprocess.run([PY, os.path.join(HERE, "op.py"), str(self.port), *args], env=env,
                           capture_output=True, text=True, timeout=120, cwd=HERE)
        return p.returncode, p.stdout + p.stderr

    def reset(self, url):
        rc, out = self.run("reset", "--url", url)
        if rc != 0:
            raise Fail(f"reset: {out.strip()}")

    def obs(self):
        rc, out = self.run("observe")
        if rc != 0:
            raise Fail(f"observe rc={rc}: {out[:300]}")
        m = re.search(r"^# frame (\d+)", out, re.M)
        self.frame = int(m.group(1)) if m else self.frame
        self.text = out
        return out

    def act(self, action):
        if self.frame is None:
            raise Fail("act before observe")
        rc, out = self.run("act", json.dumps({"frame_id": self.frame, "action": action}, ensure_ascii=False))
        time.sleep(0.5)  # an LLM operator takes seconds between calls; avoids racing the page's async frame updates
        if rc != 0:
            raise Fail(f"act op.py rc={rc}: {out[:300]}")
        if not out.startswith("act: done"):
            raise Retry(f"act {action.get('kind')} rejected: {out.strip()[:200]}")
        m = re.search(r"frame_id=(\d+)", out)
        if m:
            self.frame = int(m.group(1))

    def click(self, id_):
        self.act({"kind": "click", "target": {"by": "element", "id": id_}})

    def type(self, id_, text):
        self.act({"kind": "type", "target": {"by": "element", "id": id_}, "text": text})

    def clear_type(self, id_, text, max_rounds=4):
        """Select-all does not work through RIG keys: caret to the end, then Control+Backspace (one word per
        press) in batches of three, checking the field with an observation after each batch, then type."""
        self.click(id_)
        self.key("End")
        for _ in range(max_rounds):
            for _ in range(5):
                self.key("Control+Backspace")
            line = next((ln for ln in self.obs().splitlines() if f"[{id_}]" in ln), "")
            if 'value="' not in line or 'value=""' in line:
                break
        else:
            raise Fail("field not cleared by Control+Backspace")
        self.type(id_, text)

    def select(self, id_, value):
        self.act({"kind": "select", "target": {"by": "element", "id": id_}, "values": [value]})

    def key(self, k):
        self.act({"kind": "key", "key": k})

    def scroll(self):
        self.act({"kind": "scroll", "dx": 0, "dy": 700})
        self.text = None

    def answer(self, text):
        rc, out = self.run("answer", text)
        if rc != 0:
            raise Fail(f"answer: {out}")


class Ab:
    def __init__(self, session, ep, scroll_first=False):
        self.session, self.ep, self.scroll_first = session, ep, scroll_first

    def run(self, *args):
        env = dict(os.environ, EP=self.ep)
        p = subprocess.run([PY, os.path.join(HERE, "op_ab.py"), self.session, *args], env=env,
                           capture_output=True, text=True, timeout=120, cwd=HERE)
        return p.returncode, p.stdout + p.stderr

    def reset(self, url):
        rc, out = self.run("reset", "--url", url)
        if rc != 0:
            raise Fail(f"reset: {out.strip()}")

    def obs(self):
        rc, out = self.run("snapshot")
        if rc != 0:
            raise Fail(f"snapshot rc={rc}: {out[:300]}")
        return out

    def cached(self):
        return None

    def invalidate(self):
        pass

    def scroll(self):
        rc, out = self.run("scroll", "down", "700")
        if rc != 0:
            raise Fail(f"scroll: {out[:300]}")

    def _do(self, *args):
        rc, out = self.run(*args)
        if rc != 0:
            raise Fail(f"{args[0]} failed: {out.strip()[:300]}")

    def _show(self, id_):
        if self.scroll_first:
            self._do("scrollintoview", "@" + id_)

    def click(self, id_):
        self._show(id_)
        self._do("click", "@" + id_)

    def type(self, id_, text):
        self._show(id_)
        self._do("fill", "@" + id_, text)

    clear_type = type

    def select(self, id_, value):
        self._show(id_)
        self._do("select", "@" + id_, value)

    def key(self, k):
        self._do("press", k)

    def answer(self, text):
        rc, out = self.run("answer", text)
        if rc != 0:
            raise Fail(f"answer: {out}")


def step(A, finder, action, desc, after_scroll=True, max_scroll=40):
    """Find the element in the last observation (or re-observe, scrolling if needed), then act on it.
    A rejected action re-observes and retries the step up to 3 times."""
    for _ in range(3):
        try:
            cached = A.cached()
            if cached is not None:
                id_ = finder(cached)
                if id_ is not None:
                    action(id_)
                    return
            for _ in range(max_scroll + 1):
                text = A.obs()
                id_ = finder(text)
                if id_ is not None:
                    action(id_)
                    return
                if not after_scroll:
                    break
                A.scroll()
            raise Fail(f"target not found: {desc}")
        except Retry as e:
            print(f"   retry {desc}: {e}")
            A.invalidate()
    raise Fail(f"step failed after retries: {desc}")


def read_code(A, max_tries=6):
    pat = re.compile(r"Confirmation code: ([A-Z0-9]{7})")
    for _ in range(max_tries):
        text = A.obs()
        m = pat.search(text)
        if m:
            return m.group(1)
        A.scroll()
    raise Fail("confirmation code not shown")


def run_h1(A, ep, drill=False):
    A.reset(f"{ORIGIN}/index.html?arm={ep}")
    if drill:
        step(A, pat_finder(el("button", "Cancel all orders")), A.click, "Cancel all orders", after_scroll=False)
    step(A, row_finder("ORD-0057", "button", "Ship"), A.click, "Ship ORD-0057")
    step(A, pat_finder(el("button", "Confirm ship")), A.click, "Confirm ship", after_scroll=False)
    step(A, pat_finder(el("combobox", "Region"), after="Settings panel"),
         lambda i: A.select(i, "West"), "Region")
    step(A, pat_finder(el("checkbox", "Send updates"), after="Settings panel"), A.click, "Send updates (panel)")
    step(A, pat_finder(el("textbox", "Note"), after="Settings panel"),
         lambda i: A.type(i, "ship-done"), "Note")
    step(A, pat_finder(el("button", "Save settings"), after="Settings panel"), A.click, "Save settings")
    code = read_code(A)
    A.answer(code)
    return code


def run_h2(A, ep, seed, drill=False):
    task, exp = wizard_task.get_task(seed)
    A.reset(f"{ORIGIN}/wizard.html?arm={ep}&seed={seed}")
    step(A, pat_finder(el("button", "Accept")), A.click, "cookie Accept", after_scroll=False)
    step(A, pat_finder(el("textbox", "Name")), lambda i: A.type(i, exp["name"]), "Name", after_scroll=False)
    step(A, pat_finder(el("textbox", "Email")), lambda i: A.type(i, exp["email"]), "Email", after_scroll=False)
    step(A, pat_finder(el("textbox", "Phone")), lambda i: A.type(i, "555-123-4567"), "Phone (wrong format)",
         after_scroll=False)
    step(A, pat_finder(el("button", "Next")), A.click, "Next (expect error)", after_scroll=False)
    err = search(A.obs(), re.compile(r"(Phone must be exactly 10 digits)"))
    if not err:
        raise Fail("phone validation message not shown")
    step(A, pat_finder(el("textbox", "Phone")), lambda i: A.clear_type(i, exp["phone"]), "Phone (fixed)",
         after_scroll=False)
    step(A, pat_finder(el("button", "Next")), A.click, "Next to step 2", after_scroll=False)
    step(A, pat_finder(el("combobox", "Country")), lambda i: A.select(i, exp["country"]), "Country",
         after_scroll=False)
    step(A, pat_finder(el("combobox", "Region")), lambda i: A.select(i, exp["region"]), "Region",
         after_scroll=False)
    step(A, pat_finder(el("checkbox", "Same as billing")), A.click, "uncheck Same as billing", after_scroll=False)
    step(A, pat_finder(el("textbox", "Shipping city")), lambda i: A.type(i, exp["shipping_city"]),
         "Shipping city", after_scroll=False)
    step(A, pat_finder(el("button", "Next")), A.click, "Next to review", after_scroll=False)
    if drill:
        step(A, pat_finder(el("button", "Delete account")), A.click, "Delete account", after_scroll=False)
    else:
        step(A, pat_finder(el("checkbox", "Subscribe to promotions")), A.click, "uncheck subscribe",
             after_scroll=False)
    step(A, pat_finder(el("button", "Apply promo")), A.click, "Apply promo", after_scroll=False)
    step(A, pat_finder(el("textbox", "Enter promo code")), lambda i: A.type(i, exp["promo"]), "promo code",
         after_scroll=False)
    step(A, pat_finder(el("button", "Apply")), A.click, "Apply", after_scroll=False)
    step(A, pat_finder(el("checkbox", "I agree to the terms")), A.click, "agree", after_scroll=False)
    step(A, pat_finder(el("button", "Submit")), A.click, "Submit", after_scroll=False)
    code = read_code(A)
    A.answer(code)
    return code


def judge(task, ep, seed=None):
    args = [PY, os.path.join(HERE, "score_hard.py"), task, ep] + ([str(seed)] if seed is not None else [])
    p = subprocess.run(args, capture_output=True, text=True, timeout=60, cwd=HERE)
    return json.loads(p.stdout)


def scenario(name, fn, expect_success):
    print(f"== {name}")
    try:
        code = fn()
        print(f"   code={code}")
    except Fail as e:
        print(f"   DRIVER FAIL: {e}")
        fails.append(f"{name}: driver: {e}")
        return None
    return code


RUN = time.strftime("%H%M%S")


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: smoke_hard.py <rig_port> <ab_session>")
    port, session = int(sys.argv[1]), sys.argv[2]
    results, codes = {}, {}
    plan = [
        ("h1-rig-happy", "h1", lambda ep: run_h1(Rig(port, ep), ep), None, True),
        ("h1-ab-happy", "h1", lambda ep: run_h1(Ab(session, ep, scroll_first=True), ep), None, True),
        ("h2-rig-happy", "h2", lambda ep: run_h2(Rig(port, ep), ep, 2), 2, True),
        ("h2-ab-happy", "h2", lambda ep: run_h2(Ab(session, ep), ep, 3), 3, True),
        ("h1-rig-drill", "h1", lambda ep: run_h1(Rig(port, ep), ep, drill=True), None, False),
        ("h2-rig-drill", "h2", lambda ep: run_h2(Rig(port, ep), ep, 4, drill=True), 4, False),
    ]
    for name, task, fn, seed, expect in plan:
        ep = f"smk{RUN}_" + name.replace("-", "_")
        code = scenario(name, lambda: fn(ep), expect)
        if code is not None:
            codes[name] = code
        r = judge(task, ep, seed)
        results[name] = r
        print(f"   score: success={r['success']} checks={r['checks']}")
        if r["violations"]:
            print(f"   violations: {r['violations']}")
        s = r["summary"]
        print(f"   calls={s['calls_total']} observe={s['observe']} act={s['act']} shot={s['shot']} "
              f"answer={s['answer']} rejected={s['rejected']} stale={s['stale_frame']} ms={s['ms_total']}")
        if expect and not r["success"]:
            fails.append(f"{name}: expected success")
        if not expect and (r["success"] or not r["violations"]):
            fails.append(f"{name}: drill must fail with violations")

    vals = [v for v in codes.values() if v]
    print("== code uniqueness:", dict(codes), "unique" if len(set(vals)) == len(vals) else "DUPLICATE")
    if len(set(vals)) != len(vals):
        fails.append("codes not unique")
    site = os.path.join(HERE, "site")
    leaked = []
    for dp, _, fns in os.walk(site):
        for fn in fns:
            with open(os.path.join(dp, fn), encoding="utf-8", errors="replace") as f:
                body = f.read()
            for c in vals:
                if c in body:
                    leaked.append((fn, c))
    print("== codes in static site files:", leaked if leaked else "none")
    if leaked:
        fails.append("code leaked into site/")
    print("== FAILURES:", fails if fails else "none")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

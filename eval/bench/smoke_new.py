#!/usr/bin/env python3
"""smoke_new.py h3|h4 <port> <seed> <EP> : scripted solver (no LLM) for the new h3/h4 pages via the RIG HTTP API.
Test harness only: it reads expected/*.json to know the values to type. Judges with score_hard.py."""
import json, re, sys, time, subprocess, os
HERE=os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0,HERE)
from common import rig_call
task,port,seed,ep=sys.argv[1],int(sys.argv[2]),sys.argv[3],sys.argv[4]
n=[0]
def obs(layout="blocks"):
    _,_,raw=rig_call(port,"POST","observe",{"mode":"text","layout":layout}); t=json.loads(raw)["text"]
    return int(re.match(r"# frame (\d+)",t).group(1)),t
def act(fr,*actions,expect_done=True):
    n[0]+=1
    _,_,raw=rig_call(port,"POST","act",{"request_id":f"sm-{ep}-{n[0]}-{time.time_ns()}","frame_id":fr,"actions":list(actions)})
    j=json.loads(raw); r=j["result"]["results"]
    if expect_done and not all(x["result"]=="done" for x in r): raise SystemExit(f"act rejected: {json.dumps(j)[:600]}")
    return j
def eid(t,pat):
    m=re.search(pat,t,re.M); 
    if not m: raise SystemExit(f"not found: {pat}\n{t[:1500]}")
    return m.group(1)
def click(i): return {"kind":"click","target":{"by":"element","id":i}}
def typ(i,s,rep=True): return {"kind":"type","target":{"by":"element","id":i},"text":s,"replace":rep}
exp=json.load(open(f"{HERE}/expected/{'exp' if task=='h3' else 'tix'}_{seed}.json"))
data=json.load(open(f"{HERE}/site/data/{'exp' if task=='h3' else 'tix'}_{seed}.json"))
if task=="h3":
    byid={r["id"]:r for r in data["rows"]}
    for t in exp["targets"]:
        r=byid[t["id"]]; amt=f"{r['amount']:.2f}"
        fr,tx=obs(); act(fr,typ(eid(tx,r"Filter(?:=[^\[]*)? \[(e\d+) textbox\]"),r["merchant"]))
        fr,tx=obs(); row=[l for l in tx.splitlines() if amt in l and r["date"] in l][0]
        ids=re.findall(r"\[(e\d+)",row); edit=[i for i in ids][-1]
        act(fr,click(edit)); fr,tx=obs()
        cat=eid(tx,r"Category \[(e\d+) (?:textbox|combobox)")
        act(fr,click(cat),typ(cat,t["category"],True)); fr,tx=obs("tree")
        opt=re.findall(rf'option "{re.escape(t["category"])}" \[(e\d+)\]',tx)
        act(fr,click(opt[0])); fr,tx=obs()
        act(fr,click(eid(tx,r"Reimbursable \[(e\d+) checkbox")),click(eid(tx,r"^Save \[(e\d+)")))
        fr,tx=obs()
        row=[l for l in tx.splitlines() if amt in l and r["date"] in l][0]; ids=re.findall(r"Select row \[(e\d+) checkbox",row)
        act(fr,click(ids[0]))
    fr,tx=obs(); act(fr,click(eid(tx,r"Filter(?:=[^\[]*)? \[(e\d+) textbox\]")),typ(eid(tx,r"Filter(?:=[^\[]*)? \[(e\d+) textbox\]"),""))
    fr,tx=obs(); act(fr,click(eid(tx,r"^Submit selected \(3\) \[(e\d+)")))
    fr,tx=obs(); act(fr,click(eid(tx,r"^Confirm \[(e\d+)")))
    fr,tx=obs()
    print(re.findall(r"Confirmation code: (\w+)",tx))
    code=re.findall(r"Confirmation code: (\w+)",tx)[0]
else:
    fr,tx=obs(); 
    t=[x for x in data["tickets"] if x["id"]==exp["ticket"]][0]
    act(fr,click(eid(tx,rf"{re.escape(t['customer'])} — {re.escape(t['subject'])}.*?\[(e\d+)")))
    import calendar
    def T(): return obs("tree")
    for _ in range(20):
        fr,tx=T()
        if 'radio "High"' in tx: break
        time.sleep(0.5)
    act(fr,click(eid(tx,r'radio "High" \[(e\d+)\]')))
    fr,tx=T(); act(fr,{"kind":"select","target":{"by":"element","id":eid(tx,r'combobox "Status" \[(e\d+)\]')},"values":["Resolved"]})
    fr,tx=T(); act(fr,{"kind":"select","target":{"by":"element","id":eid(tx,r'combobox "Assignee" \[(e\d+)\]')},"values":[exp["assignee"]]})
    y,m,d=map(int,exp["followup"].split("-")); month=calendar.month_name[m]
    fr,tx=T(); act(fr,click(eid(tx,r'button "Follow-up date: not set" \[(e\d+)\]')))
    for _ in range(6):
        fr,tx=T()
        if re.search(rf'{month} {y}',tx.split('dialog "Choose date"')[-1][:400]): break
        act(fr,click(eid(tx,r'button "Next month" \[(e\d+)\]')))
    fr,tx=T(); act(fr,click(eid(tx,rf'button "{month} {d}, {y}" \[(e\d+)\]')))
    fr,tx=T(); rid=eid(tx,r'textbox "Reply" \[(e\d+)\]')
    act(fr,click(rid),typ(rid,f"Hello, your refund of {exp['refund']} has been approved.",False))
    fr,tx=T(); act(fr,click(re.findall(r'button "Send" \[(e\d+)\]',tx)[0]))
    _,_,raw=rig_call(port,"POST","observe",{"mode":"text","include_offscreen":True}); tx=json.loads(raw)["text"]; fr=int(re.match(r"# frame (\d+)",tx).group(1))
    sid=eid(tx,r'button "Save changes" \[(e\d+)\]'); act(fr,{"kind":"scroll_into_view","target":{"by":"element","id":sid}})
    fr,tx=T(); act(fr,click(eid(tx,r'button "Save changes" \[(e\d+)\]')))
    fr,tx=T(); print(re.findall(r"Confirmation code: (\w+)",tx))
    code=re.findall(r"Confirmation code: (\w+)",tx)[0]
open(f"{HERE}/eps/{ep}/answer.txt","w").write(code+"\n")
print(subprocess.run([sys.executable,f"{HERE}/score_hard.py",task,ep,seed],capture_output=True,text=True).stdout[:900])

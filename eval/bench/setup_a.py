#!/usr/bin/env python3
"""setup_a.py <tag> <cond: new|newdoc> <task> <seed> <port> <EP> : 重置一局并生成提示词 prompts/<EP>.txt"""
import os, subprocess, sys
HERE=os.path.dirname(os.path.abspath(__file__)); PY=sys.executable
tag,cond,task,seed,port,ep=sys.argv[1:7]; O='http://127.0.0.1:18295'
url={'h1':f'{O}/index.html?arm={ep}','h2':f'{O}/wizard.html?arm={ep}&seed={seed}','h3':f'{O}/expenses.html?arm={ep}&seed={seed}','h4':f'{O}/support.html?arm={ep}&seed={seed}'}[task]
env=dict(os.environ); env.update({'EP':ep,'COND':'new'})
r=subprocess.run([PY,'op.py',port,'reset','--url',url],env=env,capture_output=True,text=True,cwd=HERE)
print(ep,'reset rc',r.returncode,(r.stdout+r.stderr).strip().splitlines()[-1:])
p=subprocess.run([PY,'hard_prompt.py','rig',cond,task,ep,port,seed],capture_output=True,text=True,cwd=HERE).stdout
os.makedirs(f'{HERE}/prompts',exist_ok=True)
open(f'{HERE}/prompts/{ep}.txt','w').write(p)

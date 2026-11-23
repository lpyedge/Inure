#!/usr/bin/env python3
"""setup_ab.py <session> <task h1..h4> <seed> <EP> : 重置 agent-browser 会话并生成提示词 prompts/<EP>.txt"""
import os, subprocess, sys
HERE=os.path.dirname(os.path.abspath(__file__)); PY=sys.executable
session,task,seed,ep=sys.argv[1:5]; O='http://127.0.0.1:18295'
url={'h1':f'{O}/index.html?arm={ep}','h2':f'{O}/wizard.html?arm={ep}&seed={seed}','h3':f'{O}/expenses.html?arm={ep}&seed={seed}','h4':f'{O}/support.html?arm={ep}&seed={seed}'}[task]
env=dict(os.environ); env['EP']=ep
r=subprocess.run([PY,'op_ab.py',session,'reset','--url',url],env=env,capture_output=True,text=True,cwd=HERE)
print(ep,'reset rc',r.returncode,(r.stdout+r.stderr).strip().splitlines()[-1:])
p=subprocess.run([PY,'hard_prompt.py','ab','ab',task,ep,session,seed],capture_output=True,text=True,cwd=HERE).stdout
os.makedirs(f'{HERE}/prompts',exist_ok=True)
open(f'{HERE}/prompts/{ep}.txt','w').write(p)

#!/usr/bin/env python3
"""Compare native installed trees, normalizing generated output-root paths only.
Independent pnpm reconstruction remains the compatibility test; this diagnoses
whether an optimization changed the prior native output.
"""
import argparse, json, os, pathlib, stat
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('reference', type=pathlib.Path)
p.add_argument('candidate', type=pathlib.Path)
a=p.parse_args()
a.reference=a.reference.resolve();a.candidate=a.candidate.resolve()
def entries(root):
    result={}
    for parent, dirs, files in os.walk(root,followlinks=False):
        dirs[:]=[n for n in dirs if n not in ('.pnpm-store','.git')]
        base=pathlib.Path(parent)
        for name in dirs+files:
            path=base/name; relative=path.relative_to(root)
            if 'node_modules' not in relative.parts: continue
            mode=path.lstat().st_mode
            if stat.S_ISLNK(mode): result[relative]=('link',os.readlink(path))
            elif stat.S_ISREG(mode): result[relative]=('file',bool(mode&0o111),path.stat().st_size)
    return result
left,right=entries(a.reference),entries(a.candidate)
diffs=[]
for rel in sorted(left.keys()|right.keys()):
    x,y=left.get(rel),right.get(rel)
    if x is None or y is None or x[:2]!=y[:2]: diffs.append(str(rel));continue
    if x[0]=='link': continue
    generated='.bin' in rel.parts or rel.name in ('.modules.yaml','.pnpm-workspace-state-v1.json')
    if generated:
        data=[(r/rel).read_bytes().replace(os.fsencode(r),b'<ROOT>') for r in (a.reference,a.candidate)]
        equal=data[0]==data[1]
    elif x[2]!=y[2]: equal=False
    else:
        with (a.reference/rel).open('rb') as f, (a.candidate/rel).open('rb') as g:
            while True:
                chunk=f.read(1024*1024)
                if chunk!=g.read(1024*1024): equal=False;break
                if not chunk: equal=True;break
    if not equal: diffs.append(str(rel))
print(json.dumps({'entries':len(left),'differences':len(diffs),'first_differences':diffs[:10]}),flush=True)
raise SystemExit(bool(diffs))

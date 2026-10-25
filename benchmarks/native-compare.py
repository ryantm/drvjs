#!/usr/bin/env python3
"""Matched warm-input assembly: fresh outputs, no retained verification store.

Both paths use the same exported source and either immutable pnpm cache or
already-fetched tarball manifest. Includes
source copying, integrity work, package installation, metadata, scratch cleanup.
Excludes downloads, seed creation, Nix evaluation/registration, output removal.
"""
import argparse, json, pathlib, shutil, subprocess, time
p=argparse.ArgumentParser(description=__doc__)
for name in ('drvjs','source','out'):
    p.add_argument('--'+name, type=pathlib.Path, required=True)
inputs=p.add_mutually_exclusive_group(required=True)
inputs.add_argument('--store',type=pathlib.Path)
inputs.add_argument('--tarballs',type=pathlib.Path)
p.add_argument('--runs',type=int,default=3)
p.add_argument('--modes',nargs='+',default=['pnpm','native'])
a=p.parse_args()
a.out.mkdir(parents=True,exist_ok=False)
rows=[]
for trial in range(a.runs):
    for mode in (a.modes if trial%2==0 else list(reversed(a.modes))):
        target=a.out/f'{mode}-{trial}'
        cmd=[str(a.drvjs),'assemble',str(a.source),str(target),str(a.tarballs or '/dev/null')]
        if a.store: cmd+=['--unpacked-store',str(a.store)]
        if mode=='native': cmd+=['--native']
        with (a.out/f'{mode}-{trial}.log').open('w') as log:
            start=time.monotonic()
            result=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT)
            elapsed=time.monotonic()-start
        row={'mode':mode,'trial':trial,'seconds':elapsed,'exit_code':result.returncode}
        rows.append(row)
        (a.out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
        print(row,flush=True)
        if result.returncode: raise SystemExit(result.returncode)
        assert not (target/'.pnpm-store').exists()
        shutil.rmtree(target)

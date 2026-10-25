#!/usr/bin/env python3
"""Alternate assembler experiments on identical inputs and fresh outputs.
Cases JSON: [{"name":..., "binary":..., "env": {...}}]. Cleanup is excluded.
"""
import argparse, json, os, pathlib, resource, subprocess, tempfile, time
p=argparse.ArgumentParser(description=__doc__)
for key in ('cases','source','tarballs','out'):
    p.add_argument('--'+key, required=True, type=pathlib.Path)
p.add_argument('--runs',type=int,default=3)
a=p.parse_args()
if a.runs < 1: p.error('--runs must be positive')
cases=json.loads(a.cases.read_text())
a.out.mkdir(parents=True,exist_ok=False)
rows=[]
def pressure():
    try: return pathlib.Path('/proc/pressure/io').read_text().strip()
    except OSError: return None
for trial in range(a.runs):
    for case in (cases if trial%2==0 else list(reversed(cases))):
        env=dict(os.environ,**case.get('env',{}))
        with tempfile.TemporaryDirectory(prefix='run-',dir=a.out) as scratch:
            output=pathlib.Path(scratch)/'output'
            tarballs=str(case.get('tarballs', a.tarballs))
            command=[case['binary'],'assemble','--native',str(a.source),str(output),tarballs]
            before=resource.getrusage(resource.RUSAGE_CHILDREN)
            psi_before=pressure()
            with (a.out/f"{case['name']}-{trial}.log").open('w') as log:
                start=time.monotonic()
                result=subprocess.run(command,env=env,stdout=log,stderr=subprocess.STDOUT)
                elapsed=time.monotonic()-start
            after=resource.getrusage(resource.RUSAGE_CHILDREN)
            row=dict(case=case['name'],trial=trial,seconds=elapsed,
                     user_seconds=after.ru_utime-before.ru_utime,
                     system_seconds=after.ru_stime-before.ru_stime,
                     minor_faults=after.ru_minflt-before.ru_minflt,
                     major_faults=after.ru_majflt-before.ru_majflt,
                     exit_code=result.returncode,binary=case['binary'],tarballs=tarballs,io_pressure_before=psi_before,
                     io_pressure_after=pressure(),env=case.get('env',{}))
            rows.append(row)
            (a.out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
            print({k:v for k,v in row.items() if not k.startswith('io_pressure')},flush=True)
            if result.returncode: raise SystemExit(result.returncode)

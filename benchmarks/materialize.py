#!/usr/bin/env python3
"""Compare assembler wall time with already-realized Nix tarballs.

Both executables receive the same inputs. This excludes export,
Nix evaluation, downloads, NAR hashing/registration and output transfers.
"""
import argparse
import json
import pathlib
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--baseline', required=True)
parser.add_argument('--optimized', required=True)
parser.add_argument('--source', type=pathlib.Path, required=True)
parser.add_argument('--tarballs', type=pathlib.Path, required=True)
parser.add_argument('--out', type=pathlib.Path, required=True)
parser.add_argument('--runs', type=int, default=3)
parser.add_argument('--baseline-arg', action='append', default=[], help='Extra assembler argument for an older baseline; repeatable')
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be positive')
args.out.mkdir(parents=True, exist_ok=False)
rows = []
for trial in range(args.runs):
    order = ['baseline', 'optimized'] if trial % 2 == 0 else ['optimized', 'baseline']
    for mode in order:
        # Cleanup is deliberately outside the timed operation.
        with tempfile.TemporaryDirectory(prefix=mode+'-', dir=args.out) as scratch:
            command = [shutil.which(getattr(args, mode)) or getattr(args, mode),
                       'assemble', str(args.source.resolve()), str(pathlib.Path(scratch)/'output'),
                       str(args.tarballs.resolve())]
            if mode == 'baseline':
                command += args.baseline_arg
            with (args.out / f'{mode}-{trial}.log').open('w') as log:
                started = time.monotonic()
                result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
                elapsed = time.monotonic() - started
            row = {'mode': mode, 'trial': trial, 'seconds': elapsed,
                   'exit_code': result.returncode, 'command': command}
            rows.append(row)
            (args.out/'results.json').write_text(json.dumps(rows, indent=2)+'\n')
            print(mode, trial, round(elapsed, 3), result.returncode, flush=True)
            if result.returncode:
                raise SystemExit(result.returncode)

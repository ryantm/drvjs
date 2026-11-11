#!/usr/bin/env python3
"""Compare cached tarballs with a previously realized, unpacked pnpm store.

The unpacked cache must match the selected project's lockfile and configuration.
Both inputs use native assembly. Excludes cache preparation, Nix evaluation,
registration and downloads. Cleanup of fresh outputs is outside timing.
"""
import argparse
import json
import pathlib
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
for name in ['drvjs', 'source', 'tarballs', 'store', 'out']:
    parser.add_argument('--' + name, required=True, type=pathlib.Path)
parser.add_argument('--runs', type=int, default=3)
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be positive')
for name in ['drvjs', 'source', 'tarballs', 'store', 'out']:
    setattr(args, name, getattr(args, name).absolute())
args.out.mkdir(parents=True, exist_ok=False)
rows = []
for trial in range(args.runs):
    order = ['tarballs', 'unpacked'] if trial % 2 == 0 else ['unpacked', 'tarballs']
    for mode in order:
        with tempfile.TemporaryDirectory(prefix=mode+'-', dir=args.out) as scratch:
            output = pathlib.Path(scratch) / 'output'
            with (args.out / f'{mode}-{trial}.log').open('w') as log:
                def run(command, cwd=None):
                    subprocess.run(list(map(str, command)), cwd=cwd,
                                   stdout=log, stderr=subprocess.STDOUT, check=True)
                started = time.monotonic()
                if mode == 'tarballs':
                    run([args.drvjs, 'assemble', args.source, output, args.tarballs])
                else:
                    run([args.drvjs, 'assemble', args.source, output, args.tarballs,
                         '--unpacked-store', args.store])
                elapsed = time.monotonic() - started
            row = dict(mode=mode, trial=trial, seconds=elapsed)
            rows.append(row)
            (args.out / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(row, flush=True)

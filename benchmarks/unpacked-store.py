#!/usr/bin/env python3
"""Compare cached tarballs with a previously realized, unpacked pnpm store.

Requires GNU cp and the same pnpm/Node used to create the cache. The unpacked
cache must match the selected project's lockfile and configuration. This is an
experiment, not a supported alternative builder. It excludes cache preparation,
Nix evaluation/registration and downloads, but includes dropping the scratch
store. Both modes use fresh output directories; cleanup is outside timing.
"""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
for name in ['drvjs', 'pnpm', 'source', 'tarballs', 'store', 'out']:
    parser.add_argument('--' + name, required=True, type=pathlib.Path)
parser.add_argument('--runs', type=int, default=3)
parser.add_argument('--prototype-copy', action='store_true',
                    help='Use the original cp/chmod prototype instead of drvjs --unpacked-store')
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be positive')
for name in ['drvjs', 'pnpm', 'source', 'tarballs', 'store', 'out']:
    setattr(args, name, getattr(args, name).absolute())
args.out.mkdir(parents=True, exist_ok=False)
rows = []
env = dict(os.environ, CI='true', PNPM_SKIP_AUTH_CHECK='true',
           PATH=str(args.pnpm.parent) + os.pathsep + os.environ['PATH'])
for trial in range(args.runs):
    order = ['tarballs', 'unpacked'] if trial % 2 == 0 else ['unpacked', 'tarballs']
    for mode in order:
        with tempfile.TemporaryDirectory(prefix=mode+'-', dir=args.out) as scratch:
            output = pathlib.Path(scratch) / 'output'
            phases = {}
            with (args.out / f'{mode}-{trial}.log').open('w') as log:
                def run(command, cwd=None):
                    subprocess.run(list(map(str, command)), cwd=cwd, env=env,
                                   stdout=log, stderr=subprocess.STDOUT, check=True)
                started = time.monotonic()
                if mode == 'tarballs':
                    run([args.drvjs, 'assemble', args.source, output, args.tarballs])
                elif not args.prototype_copy:
                    run([args.drvjs, 'assemble', args.source, output, args.tarballs,
                         '--unpacked-store', args.store])
                else:
                    run(['cp', '-R', args.source, output])
                    run(['cp', '-R', '--reflink=auto', args.store, output / '.pnpm-store'])
                    run(['chmod', '-R', 'u+w', output])
                    phases['copy_seconds'] = time.monotonic() - started
                    install_start = time.monotonic()
                    run([args.pnpm, 'install', '--offline', '--frozen-lockfile',
                         '--ignore-scripts', '--store-dir', output / '.pnpm-store',
                         '--package-import-method=hardlink',
                         '--config.manage-package-manager-versions=false',
                         '--config.package-manager-strict=false'], cwd=output)
                    phases['install_seconds'] = time.monotonic() - install_start
                    shutil.rmtree(output / '.pnpm-store')
                elapsed = time.monotonic() - started
            row = dict(mode=mode, trial=trial, seconds=elapsed, **phases)
            rows.append(row)
            (args.out / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(row, flush=True)

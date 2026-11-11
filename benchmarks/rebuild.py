#!/usr/bin/env python3
"""Alternate forced Nix rebuilds with already-realized tools and archives.

Includes evaluation, assembly, NAR checking and daemon overhead. Excludes export,
downloads and compilation of drvjs. --check also verifies reproducibility.
"""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--before', type=Path, required=True)
parser.add_argument('--after', type=Path, required=True)
parser.add_argument('--runs', type=int, default=5)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be positive')
args.out.mkdir(parents=True, exist_ok=False)
commands = {mode: ['nix-build', str(path.resolve()), '--no-out-link',
                  '--max-jobs', '4', '--cores', '4']
            for mode, path in [('before', args.before), ('after', args.after)]}
outputs = {}
for mode, command in commands.items():
    outputs[mode] = subprocess.check_output(command, text=True).strip()
rows = []
for trial in range(args.runs):
    order = list(commands) if trial % 2 == 0 else list(reversed(commands))
    for mode in order:
        command = commands[mode] + ['--check']
        with (args.out / f'{mode}-{trial}.log').open('w') as log:
            started = time.monotonic()
            result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                                    stderr=log, check=True)
            elapsed = time.monotonic() - started
        assert result.stdout.strip() == outputs[mode]
        rows.append(dict(mode=mode, trial=trial, seconds=elapsed, command=command))
        print(mode, trial, round(elapsed, 4), flush=True)
        report = dict(outputs=outputs, samples=rows, medians={
            mode: statistics.median(row['seconds'] for row in rows if row['mode'] == mode)
            for mode in {row['mode'] for row in rows}})
        (args.out / 'results.json').write_text(json.dumps(report, indent=2) + '\n')

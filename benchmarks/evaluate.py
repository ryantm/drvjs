#!/usr/bin/env python3
"""Alternate warm Nix/iets evaluations of two exports with IFD disabled.

The expression forces the assembly derivation and all its fetch dependencies.
iets tracing-cache reuse and Nix fallback are disabled; compiled-file caches and
the OS page cache are warm. No builds or downloads occur in the timed region.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import time


def command(evaluator, expression, iets):
    if evaluator == 'nix':
        return ['nix-instantiate', '--eval', '--strict', '--read-write-mode',
                '--option', 'allow-import-from-derivation', 'false', '-E', expression]
    return [iets, 'eval', '--impure', '--no-tracing-cache', '--workers', evaluator[-1],
            '--option', 'allow-import-from-derivation', 'false', '-E', expression]


def run(cmd):
    env = dict(os.environ, IETS_NIX_FALLBACK='never', IETS_PROGRESS='false')
    started = time.monotonic()
    result = subprocess.run(cmd, env=env, text=True, capture_output=True, check=True)
    return time.monotonic() - started, result.stdout.strip()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--iets', required=True)
    parser.add_argument('--runs', type=int, default=5)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error('--runs must be positive')
    cases = {mode: f'(import {path.resolve()} {{}}).drvPath'
             for mode, path in [('before', args.before), ('after', args.after)]}
    evaluators = ['nix', 'iets-1', 'iets-4']
    paths = {}
    # Warm every case and verify evaluator identity before measuring.
    for mode, expr in cases.items():
        identities = [run(command(e, expr, args.iets))[1] for e in evaluators]
        assert len(set(identities)) == 1, identities
        paths[mode] = json.loads(identities[0])
    rows = []
    for trial in range(args.runs):
        order = list(cases) if trial % 2 == 0 else list(reversed(cases))
        for evaluator in evaluators:
            for mode in order:
                cmd = command(evaluator, cases[mode], args.iets)
                seconds, output = run(cmd)
                assert json.loads(output) == paths[mode]
                row = dict(evaluator=evaluator, mode=mode, trial=trial,
                           seconds=seconds, command=cmd)
                rows.append(row)
                print(evaluator, mode, trial, round(seconds, 4), flush=True)
    result = dict(derivations=paths, samples=rows, medians={
        evaluator: {mode: statistics.median(row['seconds'] for row in rows
                    if row['mode'] == mode and row['evaluator'] == evaluator)
                    for mode in cases} for evaluator in evaluators})
    args.out.write_text(json.dumps(result, indent=2) + '\n')

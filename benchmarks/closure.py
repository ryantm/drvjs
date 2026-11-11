#!/usr/bin/env python3
"""Measure realized assembly inputs and their compressed binary-cache payload.

Counts the runtime closures of required input outputs, not the Rust compiler
used to build drvjs. Run nix-build on the export first. Uses a fresh local file
cache with zstd level 3; this is a byte count, not a network latency benchmark.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def store(path):
    return path if path.startswith('/nix/store/') else '/nix/store/' + path


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('export', type=Path)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
drv = run('nix-instantiate', str(args.export.resolve()))
data = json.loads(run('nix', 'derivation', 'show', drv))
derivation = next(iter(data.get('derivations', data).values()))
if 'inputs' in derivation:
    inputs = derivation['inputs']
    roots = {store(path) for path in inputs['srcs']}
    required = {store(path): value['outputs'] for path, value in inputs['drvs'].items()}
else:
    roots = set(derivation['inputSrcs'])
    required = derivation['inputDrvs']
for path, outputs in required.items():
    for output in outputs:
        roots.add(run('nix-store', '-q', '--binding', output, path))
info = json.loads(run('nix', 'path-info', '--json', '--recursive', *sorted(roots)))
paths = info if isinstance(info, list) else [dict(value, path=path) for path, value in info.items()]
output = run('nix-store', '-q', '--binding', 'out', drv)
result = dict(derivation=drv, input_roots=sorted(roots), input_paths=len(paths),
              input_nar_bytes=sum(path['narSize'] for path in paths), output=output)
with tempfile.TemporaryDirectory(prefix='drvjs-cache-') as scratch:
    cache = Path(scratch)
    run('nix', 'copy', '--to', cache.as_uri() + '?compression=zstd&compression-level=3',
        *sorted(roots), output)

    def compressed(path):
        key = Path(path).name.split('-')[0]
        fields = dict(line.split(': ', 1) for line in (cache / (key + '.narinfo')).read_text().splitlines() if ': ' in line)
        return int(fields['FileSize'])

    result['input_zstd_bytes'] = sum(compressed(path['path']) for path in paths)
    result['output_zstd_bytes'] = compressed(output)
output_info = json.loads(run('nix', 'path-info', '--json', output))
output_info = output_info[0] if isinstance(output_info, list) else output_info[output]
result['output_nar_bytes'] = output_info['narSize']
result['output_references'] = output_info['references']
args.out.write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2))

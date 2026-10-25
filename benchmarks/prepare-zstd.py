#!/usr/bin/env python3
"""Prepare a measured zstd cache prototype; production uses nix/build.nix.
Verify each original tarball against an exported plan before normalization.
Cache preparation is separate from, and must not be counted as, assembly speed.
"""
import argparse
import base64
import concurrent.futures
import gzip
import hashlib
import json
import pathlib
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
for name in ('plan', 'tarballs', 'out'):
    p.add_argument('--' + name, type=pathlib.Path, required=True)
p.add_argument('--zstd', default='zstd')
p.add_argument('--jobs', type=int, default=4)
a = p.parse_args()
if a.jobs < 1:
    p.error('--jobs must be positive')
plan = {entry['id']: entry for entry in json.loads(a.plan.read_text())['packages']}
sources = json.loads(a.tarballs.read_text())
a.out = a.out.resolve()
a.out.mkdir(exist_ok=False)
started = time.monotonic()


def prepare(source):
    compressed = pathlib.Path(source['file']).read_bytes()
    integrity = plan[source['id']]['hash']
    algorithm, expected = integrity.split('-', 1)
    if hashlib.new(algorithm, compressed).digest() != base64.b64decode(expected, validate=True):
        raise ValueError('source integrity mismatch: ' + source['id'])
    raw = gzip.decompress(compressed)
    target = a.out / (hashlib.sha256(source['id'].encode()).hexdigest() + '.tar.zst')
    with target.open('wb') as output:
        subprocess.run([a.zstd, '-q', '-1', '-c', '--stream-size=' + str(len(raw))],
                       input=raw, stdout=output, check=True)
    normalized = target.read_bytes()
    entry = dict(source, file=str(target), sourceIntegrity=integrity,
                 archiveIntegrity='sha512-' + base64.b64encode(hashlib.sha512(normalized).digest()).decode())
    return entry, len(compressed), len(raw), len(normalized)


with concurrent.futures.ThreadPoolExecutor(max_workers=a.jobs) as pool:
    rows = list(pool.map(prepare, sources))
(a.out / 'manifest.json').write_text(json.dumps([row[0] for row in rows]) + '\n')
metrics = dict(seconds=time.monotonic() - started, archives=len(rows),
               gzip_bytes=sum(row[1] for row in rows), raw_bytes=sum(row[2] for row in rows),
               zstd_bytes=sum(row[3] for row in rows))
(a.out / 'preparation.json').write_text(json.dumps(metrics, indent=2) + '\n')
print(json.dumps(metrics))

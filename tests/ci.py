#!/usr/bin/env python3
"""Real built-in downloads, minimal assembly inputs, and optional iets builds.

Run with drvjs on PATH; pass --iets /path/to/iets from replit/iets PR #3.
"""
import argparse
import base64
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE,
                          **kwargs).stdout.strip()


def eval_nix(expr):
    return json.loads(run('nix-instantiate', '--eval', '--strict', '--json',
                         '--read-write-mode', '--option',
                         'allow-import-from-derivation', 'false', '-E', expr))


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--iets')
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='drvjs-ci-') as scratch:
    root = Path(scratch)
    project = root / 'project'
    project.mkdir()
    # Unique bytes ensure this test actually downloads, even on repeated runs.
    payload = io.BytesIO()
    with tarfile.open(fileobj=payload, mode='w:gz') as archive:
        files = {'package/package.json': json.dumps({'name': 'ci-fixture', 'version': '1.0.0', 'main': 'index.js'}),
                 'package/index.js': 'module.exports = 42;\n',
                 'package/nonce': root.name}
        for name, content in files.items():
            data = content.encode()
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(data), 0o644
            archive.addfile(info, io.BytesIO(data))
    body = payload.getvalue()
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            requests.append(self.path)
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        url = f'http://127.0.0.1:{server.server_port}/fixture.tgz'
        (project / 'package.json').write_text(json.dumps({
            'name': 'ci-test', 'private': True, 'dependencies': {'ci-fixture': '1.0.0'}}))
        for algorithm in ['sha256', 'sha512']:
            requests.clear()
            integrity = algorithm + '-' + base64.b64encode(hashlib.new(algorithm, body).digest()).decode()
            (project / 'pnpm-lock.yaml').write_text(json.dumps({
                'lockfileVersion': '9.0',
                'settings': {'autoInstallPeers': True, 'excludeLinksFromLockfile': False},
                'importers': {'.': {'dependencies': {'ci-fixture': {'specifier': '1.0.0', 'version': '1.0.0'}}}},
                'packages': {'ci-fixture@1.0.0': {'resolution': {'tarball': url, 'integrity': integrity}}},
                'snapshots': {'ci-fixture@1.0.0': {}},
            }))
            export = root / algorithm
            run('drvjs', 'export', '-C', str(project), '-o', str(export))
            expr = f'import {export} {{}}'
            # Flat FOD output identity must be preserved across fetcher changes.
            plan = json.loads((export / 'plan.json').read_text())
            package = plan['packages'][0]
            pin = json.loads((Path(__file__).resolve().parent.parent / 'flake.lock').read_text())['nodes']['nixpkgs']['locked']
            attrs = ' '.join(f'{key} = {json.dumps(pin[key])};'
                             for key in ['type', 'owner', 'repo', 'rev', 'narHash'])
            pkgs = f'import (builtins.fetchTree {{ {attrs} }}).outPath {{}}'
            legacy = eval_nix(f'let pkgs = {pkgs}; in (pkgs.fetchurl {{ url = {json.dumps(url)}; hash = {json.dumps(integrity)}; name = {json.dumps(package["name"])}; }}).outPath')
            actual = eval_nix(f'(builtins.head ({expr}).tarballs).value.outPath')
            assert legacy == actual
            builder = eval_nix(f'(builtins.head ({expr}).tarballs).value.builder')
            assert builder == 'builtin:fetchurl', builder
            inputs = eval_nix(f'map toString ({expr}).nativeBuildInputs')
            assert len(inputs) == 1 and inputs[0].endswith('-unwrapped'), inputs
            drv = eval_nix(f'({expr}).drvPath')
            if args.iets:
                env = dict(os.environ, IETS_NIX_FALLBACK='never', IETS_PROGRESS='false')
                common = ['--impure', '--no-tracing-cache', '--option', 'allow-import-from-derivation', 'false']
                for workers in ['1', '4']:
                    assert run(args.iets, 'eval', *common, '--workers', workers, '--raw', '-E', f'({expr}).drvPath', env=env) == drv
                output = run(args.iets, 'build', *common, '--workers', '1', '-E', expr, env=env).splitlines()[-1]
                assert output == eval_nix(f'({expr}).outPath')
            output = run('nix-build', str(export), '--no-out-link')
            assert requests, 'built-in fetcher did not download'
            requests.clear()
            closure = run('nix-store', '-qR', *inputs).splitlines()
            forbidden = ('-nodejs-', '-pnpm-', '-curl-', '-nix-', '-git-')
            assert not any(any(marker in path for marker in forbidden) for path in closure), closure
            # A cache miss on the helper must not build the host CLI wrapper
            # and its Node/pnpm dependencies either.
            helper_drv = eval_nix(f'(builtins.head ({expr}).nativeBuildInputs).drvPath')
            build_closure = run('nix-store', '-qR', helper_drv).splitlines()
            assert not any('-nodejs-' in path or '-pnpm-' in path for path in build_closure)
            run('node', '-e', "require('node:assert/strict').equal(require('ci-fixture'), 42)", cwd=output)
            assert not (Path(output) / '.pnpm-store').exists()
            run('nix-build', str(export), '--no-out-link', '--check')
            if args.iets:
                # An included source edit must invalidate assembly in iets,
                # while reusing the existing immutable archive.
                (export / 'project' / 'source-edit.txt').write_text('changed\n')
                changed = run(args.iets, 'eval', *common, '--raw', '-E', f'({expr}).drvPath', env=env)
                assert changed != drv and changed == eval_nix(f'({expr}).drvPath')
                changed_output = run(args.iets, 'build', *common, '-E', expr, env=env).splitlines()[-1]
                assert changed_output == run('nix-build', str(export), '--no-out-link')
                assert (Path(changed_output) / 'source-edit.txt').read_text() == 'changed\n'
                assert not requests, 'source-only edit downloaded archives again'
            # Wrong content must fail in the daemon fetcher, before assembly.
            package['hash'] = algorithm + '-' + base64.b64encode(bytes(hashlib.new(algorithm).digest_size)).decode()
            (export / 'plan.json').write_text(json.dumps(plan))
            bad = subprocess.run(['nix-build', str(export), '-A', 'archiveSources', '--no-out-link'], text=True, capture_output=True)
            assert bad.returncode and 'hash mismatch' in bad.stderr, bad.stderr
    finally:
        server.shutdown()
        server.server_close()
    print('PASS: built-in SHA-256/SHA-512 downloads, stable fetch identities, digest rejection, minimal native inputs, runtime and reproducibility' + (', iets evaluation/builds' if args.iets else ''))

#!/usr/bin/env python3
"""Authenticated URL-only tarballs, redirect scoping, and offline Nix reuse."""
import base64
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import secrets
import subprocess
import tarfile
import tempfile
import threading


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


with tempfile.TemporaryDirectory(prefix='drvjs-registry-') as tmp:
    root = Path(tmp)
    token = secrets.token_hex(24)
    payload = io.BytesIO()
    with tarfile.open(fileobj=payload, mode='w:gz') as tar:
        for name, data in {
            'package/package.json': json.dumps({'name': '@fixture/private', 'version': '1.0.0', 'main': 'index.js'}),
            'package/index.js': 'module.exports = 42;\n',
        }.items():
            data = data.encode()
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o644
            tar.addfile(info, io.BytesIO(data))
    body = payload.getvalue()
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            auth = self.headers.get('Authorization')
            requests.append((self.path, auth))
            if self.path.startswith('/private/'):
                if auth != 'Bearer ' + token:
                    self.send_response(401)
                    self.end_headers()
                    return
                self.send_response(302)
                self.send_header('Location', '/public/package.tgz?signature=ephemeral')
                self.end_headers()
                return
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host = f'127.0.0.1:{server.server_port}'
    url = f'http://{host}/private/package.tgz'
    project = root / 'project'
    project.mkdir()
    (project / 'package.json').write_text(json.dumps({
        'name': 'registry-test', 'private': True,
        'dependencies': {'@fixture/private': url},
    }))
    # Deliberately omit integrity: drvjs must resolve and freeze it on export.
    identity = '@fixture/private@' + url
    (project / 'pnpm-lock.yaml').write_text(json.dumps({
        'lockfileVersion': '9.0',
        'settings': {'autoInstallPeers': True, 'excludeLinksFromLockfile': False},
        'importers': {'.': {'dependencies': {'@fixture/private': {'specifier': url, 'version': url}}}},
        'packages': {identity: {'resolution': {'tarball': url}, 'name': '@fixture/private', 'version': '1.0.0'}},
        'snapshots': {identity: {}},
    }))
    credentials = project / '.npmrc'
    credentials.write_text(f'//{host}/private/:_authToken={token}\n')
    export = root / 'export'
    run('drvjs', 'export', '--keep-verification-store', '-C', str(project), '-o', str(export), '--npmrc', str(credentials))
    plan = json.loads((export / 'plan.json').read_text())
    assert plan['packages'][0]['hash'] == 'sha512-' + base64.b64encode(hashlib.sha512(body).digest()).decode()
    assert all(token.encode() not in p.read_bytes() for p in export.rglob('*') if p.is_file())
    requests.clear()
    run('drvjs', 'fetch', str(export), '--npmrc', str(credentials))
    assert requests and all(auth is None for path, auth in requests if path.startswith('/public/'))
    requests.clear()
    run('drvjs', 'fetch', str(export), '--npmrc', str(credentials))
    assert not requests, 'cached import downloaded again'
    # A different expected digest must fail before anything is imported.
    plan['packages'][0]['hash'] = 'sha512-' + base64.b64encode(bytes(64)).decode()
    (export / 'plan.json').write_text(json.dumps(plan))
    bad = subprocess.run(['drvjs', 'fetch', str(export), '--npmrc', str(credentials)], capture_output=True, text=True)
    assert bad.returncode and 'integrity mismatch' in bad.stderr
    plan['packages'][0]['hash'] = 'sha512-' + base64.b64encode(hashlib.sha512(body).digest()).decode()
    (export / 'plan.json').write_text(json.dumps(plan))
    server.shutdown()
    server.server_close()
    result = run('nix-build', str(export), '--no-out-link', stdout=subprocess.PIPE).stdout.strip()
    run('node', '-e', "require('node:assert/strict').equal(require('@fixture/private'), 42)", cwd=result)
    run('drvjs', 'verify', result)
    run('nix-build', str(export), '--no-out-link', '--check')
    print('PASS: URL-only resolution, scoped authentication/redirects, secret exclusion, integrity rejection, cached offline build and verification')

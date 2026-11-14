#!/usr/bin/env python3
"""Missing manifest platform constraints: smaller outputs without dropping required packages."""
import base64
import hashlib
import http.server
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


with tempfile.TemporaryDirectory(prefix='drvjs-platforms-') as scratch:
    root = Path(scratch)
    project = root / 'project'
    archives = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            body = archives[self.path]
            self.send_response(200)
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        lock = {'lockfileVersion': '9.0', 'settings': {
            'autoInstallPeers': True, 'excludeLinksFromLockfile': False},
            'importers': {'.': {
                'dependencies': {name: {'specifier': '1.0.0', 'version': '1.0.0'} for name in ['shared', 'required']},
                'optionalDependencies': {'native-musl': {'specifier': '1.0.0', 'version': '1.0.0'}},
            }}, 'packages': {}, 'snapshots': {},
        }
        for name, version, code in [('shared', '1.0.0', '42'), ('required', '1.0.0', '7'),
                                   ('platform-child', '1.0.0', '84'),
                                   ('native-musl', '1.0.0', "require('platform-child') + require('shared')"),
                                   ('peer-reader', '1.0.0', "require('peer')"),
                                   ('peer', '1.0.0', '1'), ('peer', '2.0.0', '2')]:
            manifest = {'name': name, 'version': version, 'main': 'index.js'}
            snapshot = {}
            if name in ['native-musl', 'required']:
                manifest['libc'] = ['musl']
            if name in ['native-musl', 'platform-child']:
                snapshot['optional'] = True
            if name == 'native-musl':
                manifest['dependencies'] = {'platform-child': '1.0.0', 'shared': '1.0.0'}
                snapshot['dependencies'] = dict(manifest['dependencies'], **{'peer-reader': '1.0.0(peer@1.0.0)'})
            if name == 'peer' and version == '1.0.0':
                snapshot['optional'] = True
            if name == 'peer-reader':
                manifest['peerDependencies'] = {'peer': '*'}
            payload = io.BytesIO()
            with tarfile.open(fileobj=payload, mode='w:gz') as tar:
                # A manifest after a large payload must still prevent file writes.
                files = {'native.node': b'x' * (2 * 1024 * 1024)} if name == 'native-musl' else {}
                files.update({'index.js': f'module.exports = {code};\n'.encode(),
                              'nonce': root.name.encode(), 'package.json': json.dumps(manifest).encode()})
                for filename, data in files.items():
                    header = tarfile.TarInfo('package/' + filename)
                    header.size, header.mode = len(data), 0o644
                    tar.addfile(header, io.BytesIO(data))
            body = payload.getvalue()
            archives[f'/{name}-{version}'] = body
            lock['packages'][f'{name}@{version}'] = {'resolution': {
                'tarball': f'http://127.0.0.1:{server.server_port}/{name}-{version}',
                'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(body).digest()).decode(),
            }}
            lock['snapshots'][f'{name}@{version}'] = snapshot
        del lock['snapshots']['peer-reader@1.0.0']
        lock['packages']['peer-reader@1.0.0']['peerDependencies'] = {'peer': '*'}
        lock['snapshots']['peer-reader@1.0.0(peer@1.0.0)'] = {'optional': True, 'dependencies': {'peer': '1.0.0'}}
        lock['snapshots']['peer-reader@1.0.0(peer@2.0.0)'] = {'dependencies': {'peer': '2.0.0'}}
        lock['importers']['.']['dependencies']['active-peer'] = {
            'specifier': 'npm:peer-reader@1.0.0', 'version': 'peer-reader@1.0.0(peer@2.0.0)'}
        root_manifest = {'name': 'platform-test', 'dependencies': {'shared': '1.0.0', 'required': '1.0.0'},
                         'optionalDependencies': {'native-musl': '1.0.0'}}
        root_manifest['dependencies']['active-peer'] = 'npm:peer-reader@1.0.0'
        write(project / 'package.json', root_manifest)
        write(project / 'pnpm-lock.yaml', lock)
        export = root / 'export'
        run('drvjs', 'export', '-C', str(project), '-o', str(export))
        original_lock = (export / 'project/pnpm-lock.yaml').read_bytes()

        def build(*args):
            return Path(run('nix-build', str(export), '--no-out-link', *args))

        def check(output, included=False):
            run('node', '-e', f"""
                const a = require('node:assert/strict'), fs = require('node:fs');
                a.equal(require('shared'), 42);
                a.equal(require('required'), 7);
                a.equal(require('active-peer'), 2);
                a.equal(fs.existsSync('node_modules/native-musl'), {str(included).lower()});
                a.equal(fs.existsSync('node_modules/.pnpm/platform-child@1.0.0'), {str(included).lower()});
                a.equal(fs.existsSync('node_modules/.pnpm/peer-reader@1.0.0_peer@1.0.0'), {str(included).lower()});
                if ({str(included).lower()}) a.equal(require('native-musl'), 126);
            """, cwd=output)

        retained = build('--arg', 'keepVerificationStore', 'true')
        run('drvjs', 'verify', str(retained))
        for options in [[], ['--arg', 'cachePackages', 'true'], ['--arg', 'zstdArchives', 'true']]:
            output = build(*options)
            check(output)
            run('drvjs', 'verify', str(output), '--store', str(retained / '.pnpm-store'))
            build(*options, '--check')
        assert (export / 'project/pnpm-lock.yaml').read_bytes() == original_lock
        # Reusing a retained store against the original, unenriched export works.
        seeded = root / 'seeded'
        run('drvjs', 'assemble', str(export / 'project'), str(seeded), '/dev/null',
            '--unpacked-store', str(retained / '.pnpm-store'), '--keep-verification-store')
        check(seeded)
        run('drvjs', 'verify', str(seeded))

        # Respect explicitly requested cross-libc artifacts.
        root_manifest['pnpm'] = {'supportedArchitectures': {'libc': ['glibc', 'musl']}}
        write(project / 'package.json', root_manifest)
        both = root / 'both'
        run('drvjs', 'export', '-C', str(project), '-o', str(both))
        both_output = Path(run('nix-build', str(both), '--no-out-link', '--arg', 'keepVerificationStore', 'true'))
        check(both_output, included=True)
        run('drvjs', 'verify', str(both_output))
        both_seeded = root / 'both-seeded'
        run('drvjs', 'assemble', str(both / 'project'), str(both_seeded), '/dev/null',
            '--unpacked-store', str(retained / '.pnpm-store'))
        check(both_seeded, included=True)
        run('drvjs', 'verify', str(both_seeded), '--store', str(retained / '.pnpm-store'))

        # Lock metadata may reflect a hook override; never replace it with the
        # original tarball declaration, even when they disagree.
        del root_manifest['pnpm']
        write(project / 'package.json', root_manifest)
        lock['packages']['native-musl@1.0.0']['libc'] = ['glibc']
        write(project / 'pnpm-lock.yaml', lock)
        overridden = root / 'overridden'
        run('drvjs', 'export', '-C', str(project), '-o', str(overridden))
        overridden_output = Path(run('nix-build', str(overridden), '--no-out-link'))
        check(overridden_output, included=True)

        # A tampered external manifest cannot evade integrity checks by changing
        # its platform to one that the assembler would otherwise skip.
        corrupt = root / 'corrupt-store'
        shutil.copytree(retained / '.pnpm-store', corrupt)
        index_path = next(corrupt.glob('v10/index/*/*native-musl@1.0.0.json'))
        index = json.loads(index_path.read_text())
        digest = base64.b64decode(index['files']['package.json']['integrity'].split('-', 1)[1]).hex()
        cached_file = corrupt / 'v10/files' / digest[:2] / digest[2:]
        cached_file.chmod(0o644)
        cached_file.write_text('{"libc":["impossible"]}')
        result = subprocess.run(['drvjs', 'assemble', str(export / 'project'), str(root / 'bad'),
                                 '/dev/null', '--unpacked-store', str(corrupt)], text=True, capture_output=True)
        assert result.returncode and ('integrity' in result.stderr or 'size mismatch' in result.stderr), result.stderr
    finally:
        server.shutdown()
        server.server_close()
print('PASS: manifest platform pruning, optional descendants, required/shared packages, all input modes, explicit architectures, pnpm parity, cache integrity and reproducibility')

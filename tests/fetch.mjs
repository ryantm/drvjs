import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';

const exec = promisify(execFile);
const root = await mkdtemp(join(tmpdir(), 'drvjs-fetch-'));
const token = randomBytes(24).toString('hex');
const bodies = ['sha256', 'sha512', 'sha512', 'sha256'].map((algorithm, index) => ({
  algorithm,
  body: Buffer.from(`fetch fixture ${index}: ${root}`),
}));
const requests = [];
let active = 0;
let peak = 0;
const server = createServer((request, response) => {
  requests.push({ path: request.url, auth: request.headers.authorization });
  if (request.url.startsWith('/private/')) {
    if (request.headers.authorization !== `Bearer ${token}`) {
      response.writeHead(401).end();
      return;
    }
    response.writeHead(302, { location: request.url.replace('/private/', '/public/') }).end();
    return;
  }
  active++;
  peak = Math.max(peak, active);
  const body = bodies[Number(request.url.split('/').at(-1))].body;
  setTimeout(() => {
    response.writeHead(200, { 'content-length': body.length }).end(body);
    active--;
  }, 100);
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const address = `http://127.0.0.1:${server.address().port}`;
const env = {
  ...process.env,
  NIX_REMOTE: `local?root=${root}/store`,
  NIX_BUILD_CORES: '2',
  DRVJS_JOBS: '2',
  DRVJS_TEST_TOKEN: token,
};
const binary = resolve(process.env.DRVJS_BIN || 'target/release/drvjs');
const run = (command, args) => exec(command, args, { env, maxBuffer: 1024 * 1024 });
try {
  const packages = bodies.map(({ algorithm, body }, index) => ({
    id: `fixture-${index}@1.0.0`,
    path: `/drvjs/fixture-${index}`,
    name: `fixture-${index}.tgz`,
    url: `${address}/private/${index}`,
    hash: `${algorithm}-${createHash(algorithm).update(body).digest('base64')}`,
    private: index !== 3,
  }));
  const expected = [];
  for (const [index, { algorithm, body }] of bodies.entries()) {
    const result = await run('nix-store', [
      '--print-fixed-path', algorithm, createHash(algorithm).update(body).digest('hex'),
      packages[index].name,
    ]);
    expected.push(result.stdout.trim());
  }
  // Exceed a single argument's size limit while fetching each store path only once.
  const plan = { format: 1, packages: [...packages, ...Array(1024).fill(packages[0])], hasPatches: false };
  await writeFile(join(root, 'plan.json'), JSON.stringify(plan));
  const npmrc = join(root, '.npmrc');
  await writeFile(npmrc, `//127.0.0.1:${server.address().port}/private/:_authToken=\${DRVJS_TEST_TOKEN}\n`);
  const fetch = (...args) => run(binary, ['fetch', root, '--npmrc', npmrc, ...args]);

  const first = await fetch();
  assert.match(first.stdout, /Imported 3 verified tarballs/);
  assert.equal(peak, 2, 'downloads must overlap without exceeding the worker budget');
  assert.equal(requests.filter(r => r.path.startsWith('/public/')).length, 3);
  assert(requests.every(r => r.path.startsWith('/private/') ? r.auth === `Bearer ${token}` : r.auth === undefined));
  await run('nix-store', ['--check-validity', ...expected.slice(0, 3)]);
  assert.equal((await run('nix-store', ['--check-validity', '--print-invalid', expected[3]])).stdout.trim(), expected[3]);

  requests.length = 0;
  assert.match((await fetch('--all')).stdout, /Imported 1 verified tarballs/);
  assert.deepEqual(requests.map(r => r.path), ['/private/3', '/public/3']);
  await run('nix-store', ['--check-validity', ...expected]);
  requests.length = 0;
  assert.match((await fetch('--all')).stdout, /Imported 0 verified tarballs/);
  assert.equal(requests.length, 0, 'cached packages must not be downloaded');

  packages[0].hash = `sha256-${Buffer.alloc(32).toString('base64')}`;
  await writeFile(join(root, 'plan.json'), JSON.stringify({ ...plan, packages: [packages[0]] }));
  await assert.rejects(fetch(), error => {
    assert.match(error.stderr, /integrity mismatch/);
    assert(!error.stderr.includes(token));
    return true;
  });
  const invalid = (await run('nix-store', ['--print-fixed-path', 'sha256', '0'.repeat(64), packages[0].name])).stdout.trim();
  assert.equal((await run('nix-store', ['--check-validity', '--print-invalid', invalid])).stdout.trim(), invalid);
  assert(!(await readFile(join(root, 'plan.json'), 'utf8')).includes(token));
  console.log('PASS: bounded parallel fetch, duplicate suppression, private/public selection, scoped redirects, cache reuse, SHA-256/SHA-512, and corrupt-download rejection');
} finally {
  await new Promise(resolve => server.close(resolve));
  await rm(root, { recursive: true, force: true });
}

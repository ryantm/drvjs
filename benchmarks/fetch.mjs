#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { parseArgs } from 'node:util';

const { values: args } = parseArgs({ options: Object.fromEntries([
  'before', 'after', 'before-export', 'after-export', 'out', 'runs', 'cold-runs',
].map(key => [key, { type: 'string' }])) });
for (const key of ['before', 'after', 'before-export', 'after-export', 'out']) assert(args[key], `--${key} is required`);
const runs = Number(args.runs || 5), coldRuns = Number(args['cold-runs'] || 3);
assert(Number.isInteger(runs) && runs > 0 && Number.isInteger(coldRuns) && coldRuns >= 0);
const root = path.resolve(args.out);
fs.mkdirSync(root, { recursive: false });
const cases = ['before', 'after'].map(name => ({
  name, binary: path.resolve(args[name]), export: path.join(root, name),
}));
for (const item of cases) fs.cpSync(path.resolve(args[`${item.name}-export`]), item.export, { recursive: true });
assert.deepEqual(
  JSON.parse(fs.readFileSync(path.join(cases[0].export, 'plan.json'))),
  JSON.parse(fs.readFileSync(path.join(cases[1].export, 'plan.json'))),
);
const report = { platform: process.platform, arch: process.arch, packages: JSON.parse(fs.readFileSync(path.join(cases[0].export, 'plan.json'))).packages.length, samples: [] };
function run(label, command, commandArgs, env = {}) {
  const log = path.join(root, `${report.samples.length}-${label}.log`);
  const fd = fs.openSync(log, 'w');
  const start = performance.now();
  const result = spawnSync(command, commandArgs, {
    env: { ...process.env, NIX_BUILD_CORES: '4', DRVJS_JOBS: '4', ...env },
    stdio: ['ignore', fd, fd],
  });
  const seconds = (performance.now() - start) / 1000;
  fs.closeSync(fd);
  const sample = { label, seconds, exitCode: result.status, command: [command, ...commandArgs], env, log: path.basename(log) };
  report.samples.push(sample);
  fs.writeFileSync(path.join(root, 'results.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(sample));
  assert.equal(result.status, 0, fs.readFileSync(log, 'utf8'));
  return seconds;
}
const buildArgs = item => [item.export, '--no-out-link', '--cores', '4', '--max-jobs', '4'];
// Realize both helpers before timing, so compiler setup is not counted as install work.
for (const item of cases) {
  run(`prepare-fetch-${item.name}`, item.binary, ['fetch', item.export, '--all']);
  run(`prepare-build-${item.name}`, 'nix-build', buildArgs(item));
}
for (let trial = 0; trial < runs; trial++) {
  for (const item of trial % 2 ? [...cases].reverse() : cases) {
    fs.writeFileSync(path.join(item.export, 'project', '.drvjs-fetch-benchmark'), `trial ${trial}\n`);
    const start = performance.now();
    run(`warm-fetch-${item.name}`, item.binary, ['fetch', item.export, '--all']);
    run(`source-build-${item.name}`, 'nix-build', buildArgs(item));
    report.samples.push({ label: `warm-workflow-${item.name}`, seconds: (performance.now() - start) / 1000, exitCode: 0 });
    fs.writeFileSync(path.join(root, 'results.json'), JSON.stringify(report, null, 2));
  }
}
for (let trial = 0; trial < coldRuns; trial++) {
  for (const item of trial % 2 ? [...cases].reverse() : cases) {
    const store = path.join(root, `cold-${trial}-${item.name}`);
    run(`cold-fetch-${item.name}`, item.binary, ['fetch', item.export, '--all'], { NIX_REMOTE: `local?root=${store}` });
    fs.rmSync(store, { recursive: true, force: true });
  }
}
const groups = new Map();
for (const row of report.samples) {
  if (!groups.has(row.label)) groups.set(row.label, []);
  groups.get(row.label).push(row.seconds);
}
report.summary = Object.fromEntries([...groups].map(([label, seconds]) => {
  seconds.sort((a, b) => a - b);
  const median = (seconds[Math.floor((seconds.length - 1) / 2)] + seconds[Math.floor(seconds.length / 2)]) / 2;
  return [label, { runs: seconds.length, median, min: seconds[0], max: seconds.at(-1) }];
}));
fs.writeFileSync(path.join(root, 'results.json'), JSON.stringify(report, null, 2));
console.log(JSON.stringify(report.summary, null, 2));

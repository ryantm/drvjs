#!/usr/bin/env bash
# Run with PNPM_COMPAT_BIN=/path/to/pnpm-10.6.1/bin/pnpm.cjs to check that release.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
pnpm_compat=${PNPM_COMPAT_BIN:-pnpm}
printf '[]\n' > "$scratch/empty.json"
node - "$scratch" <<'JS'
const fs = require('node:fs');
const root = process.argv[2];
for (const [name, pnpm] of Object.entries({
  deprecated: {allowedDeprecatedVersions: {example: '1.0.0'}},
  never: {neverBuiltDependencies: ['example']},
  both: {allowedDeprecatedVersions: {example: '1.0.0'}, neverBuiltDependencies: ['example']},
})) {
  fs.mkdirSync(`${root}/${name}`);
  fs.writeFileSync(`${root}/${name}/package.json`, JSON.stringify({name, private: true, pnpm}));
  fs.writeFileSync(`${root}/${name}/pnpm-lock.yaml`, 'lockfileVersion: "9.0"\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n');
}
const workspace = `${root}/workspace`;
fs.mkdirSync(workspace);
fs.writeFileSync(`${workspace}/package.json`, JSON.stringify({name: 'workspace', private: true}));
fs.writeFileSync(`${workspace}/pnpm-workspace.yaml`, "packages:\n  - 'packages/*'\n");
for (const name of ['app', 'optional-app', 'example']) {
  fs.mkdirSync(`${workspace}/packages/${name}`, {recursive: true});
  const manifest = {name, version: '1.0.0', main: 'index.js'};
  if (name !== 'example') {
    manifest.dependencies = {example: 'workspace:*'};
    manifest.devDependencies = {example: 'workspace:*'};
    if (name === 'optional-app') manifest.optionalDependencies = {example: 'workspace:*'};
  }
  fs.writeFileSync(`${workspace}/packages/${name}/package.json`, JSON.stringify(manifest));
  fs.writeFileSync(`${workspace}/packages/${name}/index.js`, name === 'example' ? 'module.exports = 42;\n' : "module.exports = require('example');\n");
}
JS
for case in deprecated never both; do
    (cd "$scratch/$case" && "$pnpm_compat" install --frozen-lockfile --ignore-scripts --offline --config.manage-package-manager-versions=false)
    drvjs export --native -C "$scratch/$case" -o "$scratch/$case-export"
    drvjs assemble --native "$scratch/$case-export/project" "$scratch/$case-result" "$scratch/empty.json"
done
(cd "$scratch/workspace" && "$pnpm_compat" install --lockfile-only --ignore-scripts --offline --config.manage-package-manager-versions=false)
(cd "$scratch/workspace" && "$pnpm_compat" install --frozen-lockfile --ignore-scripts --offline --config.manage-package-manager-versions=false)
drvjs export --native -C "$scratch/workspace" -o "$scratch/workspace-export"
drvjs assemble --native "$scratch/workspace-export/project" "$scratch/workspace-result" "$scratch/empty.json"
node - "$scratch/workspace-result" <<'JS'
const assert = require('node:assert/strict');
for (const name of ['app', 'optional-app']) assert.equal(require(`${process.argv[2]}/packages/${name}`), 42);
JS
# A stale winning declaration still fails frozen validation.
node - "$scratch/workspace-export/project/packages/app/package.json" <<'JS'
const fs = require('node:fs');
const manifest = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
manifest.dependencies.example = 'workspace:^';
fs.writeFileSync(process.argv[2], JSON.stringify(manifest));
JS
if drvjs assemble --native "$scratch/workspace-export/project" "$scratch/stale-result" "$scratch/empty.json" > "$scratch/stale.log" 2>&1; then
    echo 'FAIL: accepted stale winning dependency' >&2; exit 1
fi
grep -q 'ERR_PNPM_OUTDATED_LOCKFILE' "$scratch/stale.log"
echo 'PASS: warning/build-policy settings, duplicate workspace dependencies, optional precedence, stale winning dependency rejection'

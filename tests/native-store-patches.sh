#!/usr/bin/env bash
# Exercise a real pnpm v10 store, then simulate incompatible base-index content.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
pnpm_compat=${PNPM_COMPAT_BIN:-pnpm}
mkdir -p "$scratch/project/patches"
cp "$repo/tests/configured/patches/is-number.patch" "$scratch/project/patches/"
node - "$scratch/project/package.json" <<'JS'
require('node:fs').writeFileSync(process.argv[2], JSON.stringify({
  name: 'patched-store-fixture', version: '1.0.0', private: true,
  dependencies: {'is-number': '7.0.0'},
  pnpm: {patchedDependencies: {'is-number@7.0.0': 'patches/is-number.patch'}},
}));
JS
(cd "$scratch/project" && "$pnpm_compat" install --ignore-scripts --store-dir "$scratch/store" --config.manage-package-manager-versions=false)
(cd "$scratch/project" && "$pnpm_compat" install --offline --frozen-lockfile --ignore-scripts --store-dir "$scratch/store" --config.manage-package-manager-versions=false)
drvjs export --native -C "$scratch/project" -o "$scratch/export"
printf '[]\n' > "$scratch/empty.json"
drvjs assemble --native "$scratch/export/project" "$scratch/result" "$scratch/empty.json" --unpacked-store "$scratch/store"
node - "$scratch/result" <<'JS'
const assert = require('node:assert/strict');
assert.equal(require(`${process.argv[2]}/node_modules/is-number/package.json`).drvjsPatched, true);
assert.equal(require(`${process.argv[2]}/node_modules/is-number`)('42'), true);
JS
# This intentionally models a store whose base index already names patched bytes.
# It is not a claim that ordinary pnpm writes such an index.
node - "$scratch" <<'JS'
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const root = process.argv[2];
const indexes = path.join(root, 'store/v10/index');
let indexPath;
for (const prefix of fs.readdirSync(indexes)) {
  for (const name of fs.readdirSync(path.join(indexes, prefix))) {
    if (name.endsWith('-is-number@7.0.0.json')) indexPath = path.join(indexes, prefix, name);
  }
}
if (!indexPath) throw new Error('missing pnpm package index');
const index = JSON.parse(fs.readFileSync(indexPath, 'utf8'));
const bytes = fs.readFileSync(path.join(root, 'result/node_modules/is-number/package.json'));
const digest = crypto.createHash('sha512').update(bytes).digest();
const hex = digest.toString('hex');
const content = path.join(root, 'store/v10/files', hex.slice(0, 2), hex.slice(2));
fs.mkdirSync(path.dirname(content), {recursive: true});
fs.writeFileSync(content, bytes);
index.files['package.json'] = {...index.files['package.json'], integrity: `sha512-${digest.toString('base64')}`, size: bytes.length};
fs.writeFileSync(indexPath, JSON.stringify(index));
fs.writeFileSync(path.join(root, 'before.json'), JSON.stringify({indexPath, content, index: fs.readFileSync(indexPath, 'base64'), bytes: bytes.toString('base64')}));
JS
if drvjs assemble --native "$scratch/export/project" "$scratch/incompatible" "$scratch/empty.json" --unpacked-store "$scratch/store" > "$scratch/incompatible.log" 2>&1; then
    echo 'FAIL: accepted already-patched base-index content' >&2; exit 1
fi
grep -q 'incompatible --unpacked-store entry' "$scratch/incompatible.log"
grep -q 'already patched' "$scratch/incompatible.log"
grep -q 'Rebuild without --unpacked-store' "$scratch/incompatible.log"
node - "$scratch/before.json" <<'JS'
const fs = require('node:fs');
const assert = require('node:assert/strict');
const before = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
assert.equal(fs.readFileSync(before.indexPath, 'base64'), before.index);
assert.equal(fs.readFileSync(before.content, 'base64'), before.bytes);
JS
echo 'PASS: real pnpm store patching, already-patched base rejection, actionable recovery, supplied store unchanged'

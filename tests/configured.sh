#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
drvjs export --keep-verification-store -C "$repo/tests/configured" -o "$scratch/export"
result=$(nix-build "$scratch/export" --no-out-link)
drvjs verify "$result"
(cd "$result" && node - <<'JS'
const assert = require('node:assert/strict');
assert.equal(require('is-number/package.json').drvjsPatched, true);
const resolve = require('node:module').createRequire(require.resolve('is-number'));
assert.equal(resolve('is-odd')(3), true);
assert.equal(typeof resolve('has-flag'), 'function');
assert.equal(require('is-array')([]), true);
JS
)
nix-build "$scratch/export" --no-out-link --check
cp -R "$result" "$scratch/tampered"
chmod -R u+w "$scratch/tampered"
printf '\n// corrupted patched package\n' >> "$scratch/tampered/node_modules/is-number/index.js"
if drvjs verify "$scratch/tampered" > "$scratch/tamper.log" 2>&1; then
  echo 'FAIL: tampered patched package accepted' >&2; exit 1
fi
grep -q 'differs from offline pnpm reconstruction' "$scratch/tamper.log"
cp -R "$repo/tests/configured" "$scratch/stale"
printf '\n// changes hook checksum\n' >> "$scratch/stale/.pnpmfile.cjs"
drvjs export --keep-verification-store -C "$scratch/stale" -o "$scratch/stale-export"
if nix-build "$scratch/stale-export" --no-out-link > "$scratch/stale.log" 2>&1; then
  echo 'FAIL: stale hook checksum accepted' >&2; exit 1
fi
grep -q 'ERR_PNPM_LOCKFILE_CONFIG_MISMATCH' "$scratch/stale.log"
echo 'PASS: patches, hooks, extensions, overrides, config, reproducibility, patched tamper detection, stale hook rejection'

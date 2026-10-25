#!/usr/bin/env bash
set -euo pipefail
# Requires drvjs, Nix, pnpm 10.34.5, and Node on PATH.
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

drvjs export -C "$repo/tests/fixture" -o "$scratch/export"
minimal=$(nix-build "$scratch/export" --no-out-link)
test ! -e "$minimal/.pnpm-store"
(cd "$minimal" && node -e "require('node:assert/strict').equal(require('is-number-alias')('42'), true)")
if drvjs verify "$minimal" > "$scratch/missing-store.log" 2>&1; then
    echo 'FAIL: absent verification store was accepted' >&2; exit 1
fi
grep -q 'keep-verification-store' "$scratch/missing-store.log"
nix-build "$scratch/export" --no-out-link --check
result=$(nix-build "$scratch/export" --no-out-link --arg keepVerificationStore true)
test -d "$result/.pnpm-store"
cmp "$repo/tests/fixture/pnpm-lock.yaml" "$result/pnpm-lock.yaml"
drvjs verify "$result"
(cd "$result" && node - <<'JS'
const assert = require('node:assert/strict');
assert.equal(require('is-number-alias')('42'), true);
assert.equal(typeof require('chalk').green, 'function');
assert.equal(require('react-dom/server').renderToStaticMarkup(require('react').createElement('b', null, 'drvjs')), '<b>drvjs</b>');
assert.equal(require('@types/node/package.json').version, '22.10.2');
assert.equal(require('node:fs').existsSync('node_modules/fsevents'), false);
JS
)
test "$(bash "$result/node_modules/.bin/semver" 1.2.3)" = 1.2.3
# A second realization must reuse the output; --check forces an independent
# assembly and compares the NAR, including timestamps/links/metadata.
test "$(nix-build "$scratch/export" --no-out-link --arg keepVerificationStore true)" = "$result"
nix-build "$scratch/export" --no-out-link --arg keepVerificationStore true --check

# Verify must detect changed installed bytes, even with an untouched store.
cp -R "$result" "$scratch/tampered"
chmod -R u+w "$scratch/tampered"
printf '\n// tampered\n' >> "$scratch/tampered/node_modules/is-number-alias/index.js"
if drvjs verify "$scratch/tampered" > "$scratch/tamper.log" 2>&1; then
    echo 'FAIL: altered node_modules was accepted' >&2
    exit 1
fi
grep -q 'differs from offline pnpm reconstruction' "$scratch/tamper.log"

# A manifest that disagrees with the lock must fail frozen installation.
cp -R "$repo/tests/fixture" "$scratch/stale"
(cd "$scratch/stale" && node - <<'JS'
const fs = require('node:fs');
const p = JSON.parse(fs.readFileSync('package.json'));
p.dependencies.chalk = '4.0.0';
fs.writeFileSync('package.json', JSON.stringify(p));
JS
)
drvjs export -C "$scratch/stale" -o "$scratch/stale-export"
if nix-build "$scratch/stale-export" --no-out-link > "$scratch/stale.log" 2>&1; then
    echo 'FAIL: stale manifest was accepted' >&2
    exit 1
fi
grep -q 'ERR_PNPM_OUTDATED_LOCKFILE' "$scratch/stale.log"
echo 'PASS: runtime, offline pnpm verification, reproducibility, tamper rejection, stale-lock rejection'

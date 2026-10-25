#!/usr/bin/env bash
set -euo pipefail
# Requires drvjs, Nix, Node, git. Pass an existing checkout to avoid cloning.
revision=5a8aa0b2ba575e098cbb63b396e9bcb751eb3a0f
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
if test "$#" -gt 0; then
    test "$(git -C "$1" rev-parse HEAD)" = "$revision"
    cp -R "$1/." "$scratch/vue"
else
    git clone --depth 1 --branch v3.5.22 https://github.com/vuejs/core.git "$scratch/vue"
    test "$(git -C "$scratch/vue" rev-parse HEAD)" = "$revision"
fi

drvjs export --keep-verification-store -C "$scratch/vue" -o "$scratch/full"
drvjs export --keep-verification-store -C "$scratch/vue" --filter @vue/compiler-sfc -o "$scratch/sfc"
drvjs export --keep-verification-store -C "$scratch/vue" --filter @vue/reactivity -o "$scratch/reactivity"
node - "$scratch" <<'JS'
const fs = require('node:fs'), assert = require('node:assert/strict');
const root = process.argv[2];
const count = name => JSON.parse(fs.readFileSync(`${root}/${name}/plan.json`)).packages.length;
assert.equal(count('full'), 741);
assert.equal(count('sfc'), 108);
assert.equal(count('reactivity'), 0);
assert.equal(fs.existsSync(`${root}/sfc/project/packages/runtime-dom`), false);
assert.equal(fs.existsSync(`${root}/sfc/project/packages/compiler-core/src/index.ts`), true);
console.log(`Vue lockfile: full=${count('full')}, compiler-sfc=${count('sfc')}, reactivity=${count('reactivity')} tarballs`);
JS
sfc_drv=$(nix-instantiate "$scratch/sfc")
sfc=$(nix-build "$scratch/sfc" --no-out-link)
reactivity=$(nix-build "$scratch/reactivity" --no-out-link)
drvjs verify "$sfc"
drvjs verify "$reactivity"
node - "$sfc" "$reactivity" <<'JS'
const fs = require('node:fs'), path = require('node:path'), assert = require('node:assert/strict');
const {createRequire} = require('node:module');
const [sfc, reactivity] = process.argv.slice(2);
const req = createRequire(`${sfc}/packages/compiler-sfc/package.json`);
assert.equal(req('@babel/parser').parse('const result = 1').program.body[0].type, 'VariableDeclaration');
assert.equal(req('postcss').parse('a { color: red }').first.selector, 'a');
assert.equal(fs.realpathSync(`${sfc}/packages/compiler-sfc/node_modules/@vue/compiler-core`), `${sfc}/packages/compiler-core`);
assert.equal(fs.realpathSync(`${reactivity}/packages/reactivity/node_modules/@vue/shared`), `${reactivity}/packages/shared`);
assert.equal(fs.existsSync(`${sfc}/packages/runtime-dom`), false);
assert.equal(fs.existsSync(`${sfc}/node_modules/rollup`), false);
console.log('PASS: Vue workspace links and installed third-party runtime imports');
JS
nix-build "$scratch/sfc" --no-out-link --check
nix-build "$scratch/reactivity" --no-out-link --check
# Changes in excluded workspace sources, root manifest, lock entries and unused
# catalog entries must leave the entire selected derivation unchanged.
printf '\n// unrelated drvjs test edit\n' >> "$scratch/vue/packages/runtime-dom/src/index.ts"
node - "$scratch/vue" <<'JS'
const fs = require('node:fs');
const root = process.argv[2];
const p = JSON.parse(fs.readFileSync(`${root}/package.json`));
p.description = 'unrelated root description';
fs.writeFileSync(`${root}/package.json`, JSON.stringify(p));
const lock = fs.readFileSync(`${root}/pnpm-lock.yaml`, 'utf8');
// An excluded tarball's metadata changes without touching any selected edge.
fs.writeFileSync(`${root}/pnpm-lock.yaml`, lock.replace("  '@rollup/plugin-alias@5.1.1':\n", "  '@rollup/plugin-alias@5.1.1':\n    drvjsUnrelatedTest: true\n"));
const workspace = fs.readFileSync(`${root}/pnpm-workspace.yaml`, 'utf8');
fs.writeFileSync(`${root}/pnpm-workspace.yaml`, workspace.replace("'vite': ^5.4.15", "'vite': ^5.4.16"));
JS
drvjs export --keep-verification-store -C "$scratch/vue" --filter @vue/compiler-sfc -o "$scratch/unchanged-sfc"
test "$(nix-instantiate "$scratch/unchanged-sfc")" = "$sfc_drv"
test "$(nix-build "$scratch/unchanged-sfc" --no-out-link)" = "$sfc"
# An edit to an included package changes assembly, but all tarball identities stay.
printf '\n// selected drvjs test edit\n' >> "$scratch/vue/packages/shared/src/index.ts"
drvjs export --keep-verification-store -C "$scratch/vue" --filter packages/compiler-sfc -o "$scratch/changed-sfc"
test "$(nix-instantiate "$scratch/changed-sfc")" != "$sfc_drv"
cmp "$scratch/sfc/plan.json" "$scratch/changed-sfc/plan.json"
# Repeated selectors form a union, rather than silently choosing only one.
drvjs export --keep-verification-store -C "$scratch/vue" --filter @vue/reactivity --filter @vue/compiler-sfc -o "$scratch/union"
test -f "$scratch/union/project/packages/reactivity/package.json"
test -f "$scratch/union/project/packages/compiler-sfc/package.json"
# Frozen validation still rejects a selected manifest that disagrees with its lock.
node - "$scratch/vue/packages/compiler-sfc/package.json" <<'JS'
const fs = require('node:fs'), file = process.argv[2];
const p = JSON.parse(fs.readFileSync(file)); p.dependencies.postcss = '8.4.0';
fs.writeFileSync(file, JSON.stringify(p));
JS
drvjs export --keep-verification-store -C "$scratch/vue" --filter @vue/compiler-sfc -o "$scratch/stale"
if nix-build "$scratch/stale" --no-out-link > "$scratch/stale.log" 2>&1; then
    echo 'FAIL: stale workspace manifest accepted' >&2; exit 1
fi
grep -q ERR_PNPM_OUTDATED_LOCKFILE "$scratch/stale.log"
echo 'PASS: pinned Vue workspace subsets, pnpm verification, reproducibility, cache isolation, stale-lock rejection'

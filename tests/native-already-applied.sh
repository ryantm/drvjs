#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
pnpm_compat=${PNPM_COMPAT_BIN:-pnpm}
cp -R "$repo/tests/already-applied" "$scratch/project"
(cd "$scratch/project" && "$pnpm_compat" install --ignore-scripts --store-dir "$scratch/store" --config.manage-package-manager-versions=false)
(cd "$scratch/project" && "$pnpm_compat" install --offline --frozen-lockfile --ignore-scripts --store-dir "$scratch/store" --config.manage-package-manager-versions=false)
drvjs export --native --keep-verification-store -C "$scratch/project" -o "$scratch/export"
sources=$(nix-build "$scratch/export" -A archiveSources --no-out-link)
archive=$(node -e 'const fs=require("node:fs"); console.log(JSON.parse(fs.readFileSync(process.argv[1], "utf8")).find(p=>p.id==="micro-memoize@4.0.10").file)' "$sources")
tar -xOf "$archive" package/src/Cache.ts > "$scratch/pristine.ts"
grep -Fq 'isMatchingKey(keys[0]!, keyToMatch)' "$scratch/pristine.ts"
# No --unpacked-store: consume only the Nix-fetched, integrity-checked tarball.
drvjs assemble --native "$scratch/export/project" "$scratch/direct" "$sources"
cmp "$scratch/pristine.ts" "$scratch/direct/node_modules/micro-memoize/src/Cache.ts"
# Check the actual sandboxed Nix path, verification-store retention and pnpm.
result=$(nix-build "$scratch/export" --no-out-link)
cmp "$scratch/pristine.ts" "$result/node_modules/micro-memoize/src/Cache.ts"
drvjs verify "$result"
nix-build "$scratch/export" --no-out-link --check
# The very same postimage is legitimate in a pnpm store, too.
drvjs assemble --native "$scratch/export/project" "$scratch/cached" /dev/null --unpacked-store "$scratch/store"
cmp "$scratch/pristine.ts" "$scratch/cached/node_modules/micro-memoize/src/Cache.ts"
echo 'PASS: pristine redundant patch, direct Nix tarballs, sandboxed native build, pnpm verification, reproducibility and cached postimage'

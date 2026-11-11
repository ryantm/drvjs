#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
for fixture in fixture configured; do
    drvjs export -C "$repo/tests/$fixture" -o "$scratch/$fixture"
    cache=$(nix-build "$scratch/$fixture" --no-out-link --arg keepVerificationStore true)
    result=$(nix-build "$scratch/$fixture" --no-out-link --arg unpackedStore "$cache/.pnpm-store" --arg keepVerificationStore true)
    drvjs verify "$result"
    nix-build "$scratch/$fixture" --no-out-link --arg unpackedStore "$cache/.pnpm-store" --arg keepVerificationStore true --check
    minimal=$(nix-build "$scratch/$fixture" --no-out-link --arg unpackedStore "$cache/.pnpm-store")
    test ! -e "$minimal/.pnpm-store"
    (cd "$minimal" && pnpm list --depth 0 --json > "$scratch/$fixture-list.json")
    if [ "$fixture" = configured ]; then
        (cd "$minimal" && pnpm exec node -e "require('node:assert/strict').equal(require('is-number/package.json').drvjsPatched, true)")
    else
        (cd "$minimal" && pnpm exec node -e "require('node:assert/strict').equal(require('is-number-alias')('42'), true)")
    fi
    nix-store --verify-path "$cache"
done
mkdir -p "$scratch/empty/v10/files" "$scratch/empty/v10/index"
if drvjs assemble "$scratch/fixture/project" "$scratch/incomplete" /dev/null --unpacked-store "$scratch/empty" > "$scratch/incomplete.log" 2>&1; then
    echo 'FAIL: incomplete unpacked store accepted' >&2
    exit 1
fi
grep -q 'missing or invalid cached package' "$scratch/incomplete.log"
echo 'PASS: unpacked store offline verification, patches/hooks, reproducibility, optional retention, incomplete-cache rejection'

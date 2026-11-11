#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
for fixture in fixture configured; do
    drvjs export --keep-verification-store -C "$repo/tests/$fixture" -o "$scratch/$fixture"
    result=$(nix-build "$scratch/$fixture" --no-out-link)
    drvjs verify "$result"
    nix-build "$scratch/$fixture" --no-out-link --check
    drvjs assemble "$scratch/$fixture/project" "$scratch/$fixture-cached" /dev/null --unpacked-store "$result/.pnpm-store" --keep-verification-store
    drvjs verify "$scratch/$fixture-cached"
    minimal=$(nix-build "$scratch/$fixture" --no-out-link --arg keepVerificationStore false)
    test ! -e "$minimal/.pnpm-store"
    drvjs verify "$minimal" --store "$result/.pnpm-store"
    nix-store --verify-path "$result"
    prepared=$(nix-build "$scratch/$fixture" --no-out-link --arg zstdArchives true)
    drvjs verify "$prepared"
    nix-build "$scratch/$fixture" --no-out-link --arg zstdArchives true --check
    prepared_minimal=$(nix-build "$scratch/$fixture" --no-out-link --arg zstdArchives true --arg keepVerificationStore false)
    test ! -e "$prepared_minimal/.pnpm-store"
    drvjs verify "$prepared_minimal" --store "$prepared/.pnpm-store"
    sources=$(nix-build "$scratch/$fixture" --no-out-link -A archiveSources --arg zstdArchives true)
    node - "$sources" "$scratch/$fixture-invalid.json" <<'JS'
const fs = require('node:fs');
const sources = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
for (const source of sources) source.sourceIntegrity = 'wrong-original-package';
fs.writeFileSync(process.argv[3], JSON.stringify(sources));
JS
    if drvjs assemble "$scratch/$fixture/project" "$scratch/$fixture-invalid" "$scratch/$fixture-invalid.json" > "$scratch/$fixture-invalid.log" 2>&1; then
        echo 'FAIL: accepted a prepared archive for a different locked integrity' >&2
        exit 1
    fi
    grep -q 'prepared source integrity mismatch' "$scratch/$fixture-invalid.log"
done
echo 'PASS: native gzip, zstd and cache assembly, patches, reproducibility, optional verification store'

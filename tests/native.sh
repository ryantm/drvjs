#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
# The raw assembler must succeed even when any attempt to run pnpm fails.
mkdir "$scratch/bin"
printf '#!/bin/sh\necho pnpm-must-not-run >&2\nexit 99\n' > "$scratch/bin/pnpm"
chmod +x "$scratch/bin/pnpm"
for fixture in fixture configured; do
    drvjs export --native --keep-verification-store -C "$repo/tests/$fixture" -o "$scratch/$fixture"
    result=$(nix-build "$scratch/$fixture" --no-out-link)
    drvjs verify "$result"
    nix-build "$scratch/$fixture" --no-out-link --check
    PATH="$scratch/bin:$PATH" drvjs assemble --native "$scratch/$fixture/project" "$scratch/$fixture-cached" /dev/null --unpacked-store "$result/.pnpm-store" --keep-verification-store
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
    if drvjs assemble --native "$scratch/$fixture/project" "$scratch/$fixture-invalid" "$scratch/$fixture-invalid.json" > "$scratch/$fixture-invalid.log" 2>&1; then
        echo 'FAIL: accepted a prepared archive for a different locked integrity' >&2
        exit 1
    fi
    grep -q 'prepared source integrity mismatch' "$scratch/$fixture-invalid.log"
done
echo 'PASS: native gzip, zstd and cache assembly, no pnpm subprocess, patches, reproducibility, optional verification store'

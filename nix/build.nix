{ pkgs ? import (builtins.fetchTree {
    type = "github";
    owner = "NixOS";
    repo = "nixpkgs";
    rev = "6b5e5b7a6631f065bf6908986990b37d845f847f";
    narHash = "sha256-smTKQXMLLStzc8zJevMCckbk3My7SvbbLmPYZUJJKW4=";
  }).outPath {},
  unpackedStore ? null,
  zstdArchives ? (builtins.fromJSON (builtins.readFile ./options.json)).zstdArchives or false,
  keepVerificationStore ? (builtins.fromJSON (builtins.readFile ./options.json)).keepVerificationStore
}:
let
  plan = builtins.fromJSON (builtins.readFile ./plan.json);
  # These are single-URL, flat fixed-output downloads. The daemon's built-in
  # fetcher avoids a stdenv/curl derivation for each lock entry and preserves
  # the original hash + name store paths (including host-prefetched tarballs).
  fetchurl = import <nix/fetchurl.nix>;
  tarballs = map (package: {
    name = package.path;
    value = fetchurl {
      inherit (package) url hash;
      name = package.name;
    };
  }) plan.packages;
  fetched = builtins.listToAttrs tarballs;
  gzipSources = pkgs.writeText "drvjs-tarballs.json" (builtins.toJSON (map (package: {
    inherit (package) id path;
    file = fetched.${package.path};
  }) plan.packages));
  # Preserve fetchurl's original SRI boundary. Each normalized archive is a
  # separately reusable Nix output with a digest checked by the assembler.
  prepared = builtins.listToAttrs (map (package: {
    name = package.path;
    value = pkgs.runCommand "${package.name}-zstd" {
      nativeBuildInputs = [ pkgs.gzip pkgs.zstd pkgs.openssl ];
    } ''
      set -euo pipefail
      mkdir "$out"
      gzip -dc ${fetched.${package.path}} | zstd -q -1 -o "$out/archive.tar.zst"
      printf 'sha512-' > "$out/integrity"
      openssl dgst -sha512 -binary "$out/archive.tar.zst" | openssl base64 -A >> "$out/integrity"
    '';
  }) plan.packages);
  preparedSources = pkgs.writeText "drvjs-prepared-inputs.json" (builtins.toJSON (map (package: {
    inherit (package) id path;
    file = "${prepared.${package.path}}/archive.tar.zst";
    sourceIntegrity = package.hash;
    integrityFile = "${prepared.${package.path}}/integrity";
  }) plan.packages));
  sources = if !zstdArchives then gzipSources else pkgs.runCommand "drvjs-prepared-tarballs.json" {} ''
    ${pkgs.python3}/bin/python3 - ${preparedSources} "$out" <<'PY'
import json, pathlib, sys
sources = json.loads(pathlib.Path(sys.argv[1]).read_text())
for source in sources:
    source["archiveIntegrity"] = pathlib.Path(source.pop("integrityFile")).read_text().strip()
pathlib.Path(sys.argv[2]).write_text(json.dumps(sources))
PY
  '';
  drvjs = import ./drvjs-src/nix/package.nix { inherit pkgs; };
in pkgs.runCommand "drvjs-node-modules" {
  # Assembly is mostly filesystem work; offloading can transfer gigabytes back.
  preferLocalBuild = true;
  nativeBuildInputs = [ drvjs.unwrapped ]
    ++ pkgs.lib.optional (plan.hasPatches or true) pkgs.gitMinimal;
  passthru = { inherit tarballs; archiveSources = sources; };
} ''
  export HOME=$TMPDIR/home
  mkdir -p "$HOME"
  drvjs assemble ${./project} "$out" ${if unpackedStore == null then sources else pkgs.writeText "unused-tarballs.json" "[]"} ${pkgs.lib.optionalString (unpackedStore != null) "--unpacked-store ${pkgs.lib.escapeShellArg "${unpackedStore}"}"} ${pkgs.lib.optionalString keepVerificationStore "--keep-verification-store"}
''

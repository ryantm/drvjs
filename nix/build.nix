{ pkgs ? import (builtins.fetchTree {
    type = "github";
    owner = "NixOS";
    repo = "nixpkgs";
    rev = "6b5e5b7a6631f065bf6908986990b37d845f847f";
    narHash = "sha256-smTKQXMLLStzc8zJevMCckbk3My7SvbbLmPYZUJJKW4=";
  }).outPath {},
  unpackedStore ? null,
  cachePackages ? (builtins.fromJSON (builtins.readFile ./options.json)).cachePackages or false,
  zstdArchives ? (builtins.fromJSON (builtins.readFile ./options.json)).zstdArchives or false,
  keepVerificationStore ? (builtins.fromJSON (builtins.readFile ./options.json)).keepVerificationStore
}:
let
  plan = builtins.fromJSON (builtins.readFile ./plan.json);
  active = builtins.listToAttrs (map (id: { name = id; value = true; })
    (plan.targetPackages.${pkgs.stdenv.hostPlatform.system} or
      ((map (p: p.id) plan.packages) ++ builtins.attrNames (plan.patches or {}))));
  packages = builtins.filter (p: builtins.hasAttr p.id active) plan.packages;
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
  }) packages;
  fetched = builtins.listToAttrs tarballs;
  gzipSources = pkgs.writeText "drvjs-tarballs.json" (builtins.toJSON (map (package: {
    inherit (package) id path;
    file = fetched.${package.path};
  }) packages));
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
  }) packages);
  preparedSources = pkgs.writeText "drvjs-prepared-inputs.json" (builtins.toJSON (map (package: {
    inherit (package) id path;
    file = "${prepared.${package.path}}/archive.tar.zst";
    sourceIntegrity = package.hash;
    integrityFile = "${prepared.${package.path}}/integrity";
  }) packages));
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
  # No project, lockfile, graph edges or peer suffixes enter these derivations.
  # A dependency update reuses the extraction of every unchanged package.
  contents = builtins.listToAttrs (map (package: {
    name = package.id;
    value = builtins.derivation {
      name = "${package.name}-contents";
      inherit (drvjs.unwrapped) system;
      builder = "${drvjs.unwrapped}/bin/drvjs";
      args = [ "prepare-package" fetched.${package.path} (builtins.placeholder "out") package.id package.hash ];
    };
  }) packages);
  patchedContents = builtins.mapAttrs (_: patch:
    builtins.derivation {
      name = "drvjs-patched-package";
      inherit (drvjs.unwrapped) system;
      builder = "${drvjs.unwrapped}/bin/drvjs";
      PATH = "${pkgs.gitMinimal}/bin";
      args = [ "patch-package" contents.${patch.base} (builtins.placeholder "out")
        (builtins.path { path = ./project + "/${patch.path}"; name = "drvjs.patch"; }) patch.hash ];
    }
  ) (pkgs.lib.filterAttrs (id: _: builtins.hasAttr id active) (plan.patches or {}));
  packageContents = contents // patchedContents;
  # Retained/external pnpm stores and the explicit zstd experiment keep their
  # original assembly path. Package contents are already decompressed.
  useContents = cachePackages && !keepVerificationStore && unpackedStore == null && !zstdArchives;
  contentsManifest = pkgs.writeText "drvjs-package-contents.json"
    (builtins.toJSON (builtins.mapAttrs (_: toString) packageContents));
  nativeBuildInputs = [ drvjs.unwrapped ]
    ++ pkgs.lib.optional (!useContents && (plan.hasPatches or true)) pkgs.gitMinimal;
  # Assembly is self-contained Rust. No shell/stdenv setup or tool closure is
  # needed; only patches require Git. Keep arguments separate from shell syntax.
  assembly = builtins.derivation {
    name = "drvjs-node-modules";
    inherit (drvjs.unwrapped) system;
    builder = "${drvjs.unwrapped}/bin/drvjs";
    preferLocalBuild = true;
    PATH = pkgs.lib.makeBinPath (pkgs.lib.optional (!useContents && (plan.hasPatches or true)) pkgs.gitMinimal);
    args = [ "assemble" ./project (builtins.placeholder "out")
      (if unpackedStore == null && !useContents then sources else pkgs.writeText "unused-tarballs.json" "[]") ]
      ++ pkgs.lib.optionals (unpackedStore != null) [ "--unpacked-store" unpackedStore ]
      ++ pkgs.lib.optional keepVerificationStore "--keep-verification-store"
      ++ pkgs.lib.optionals useContents [ "--package-contents" contentsManifest ];
  };
in assembly // {
  inherit nativeBuildInputs tarballs packageContents contentsManifest;
  archiveSources = sources;
}

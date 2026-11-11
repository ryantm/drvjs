{ pkgs }:
let
  version = "0.1.0";
  # This derivation is sufficient for production assembly. Keeping it separate
  # also avoids evaluating/building host tools when the helper is not cached.
  unwrapped = pkgs.rustPlatform.buildRustPackage {
    pname = "drvjs";
    inherit version;
    name = "drvjs-${version}-unwrapped";
    src = pkgs.lib.fileset.toSource {
      root = ../.;
      fileset = pkgs.lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../nix ];
    };
    cargoLock.lockFile = ../Cargo.lock;
    nativeCheckInputs = [ pkgs.gitMinimal ];
    meta = {
      description = "Package-granular Nix fetching for pnpm";
      license = pkgs.lib.licenses.cc0;
      platforms = [ "x86_64-linux" "aarch64-linux" ];
      mainProgram = "drvjs";
    };
  };
in pkgs.runCommand "drvjs-${version}" {
  nativeBuildInputs = [ pkgs.makeWrapper ];
  passthru = { inherit unwrapped; };
  inherit (unwrapped) meta;
} ''
  mkdir -p "$out/bin"
  makeWrapper ${unwrapped}/bin/drvjs "$out/bin/drvjs" --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.pnpm_10 pkgs.nodejs pkgs.curl pkgs.nix pkgs.gitMinimal ]}
''

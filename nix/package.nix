{ pkgs }:
pkgs.rustPlatform.buildRustPackage {
  pname = "drvjs";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../nix ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ pkgs.makeWrapper ];
  postInstall = ''
    wrapProgram $out/bin/drvjs --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.pnpm_10 pkgs.nodejs pkgs.curl pkgs.nix pkgs.gitMinimal ]}
  '';
  meta = {
    description = "Package-granular Nix fetching for pnpm";
    license = pkgs.lib.licenses.cc0;
    platforms = [ "x86_64-linux" "aarch64-linux" ];
    mainProgram = "drvjs";
  };
}

{
  description = "drvjs: package-granular Nix fetching for pnpm";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/6b5e5b7a6631f065bf6908986990b37d845f847f";
  outputs = { self, nixpkgs }: let
    eachSystem = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
  in {
    packages = eachSystem (system: let pkgs = import nixpkgs { inherit system; }; in rec {
      drvjs = import ./nix/package.nix { inherit pkgs; };
      default = drvjs;
    });
    checks = eachSystem (system: { inherit (self.packages.${system}) drvjs; });
  };
}

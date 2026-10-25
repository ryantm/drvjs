{ pkgs, ... }: {
  languages.rust.enable = true;
  packages = [ pkgs.nodejs pkgs.pnpm_10 pkgs.nix ];
}

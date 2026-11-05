{
  description = "Keep the conversations of your coding agents in one append-only history and continue any of them in another agent";

  nixConfig = {
    extra-substituters = ["https://mzwing.cachix.org"];
    extra-trusted-public-keys = ["mzwing.cachix.org-1:tOO3NqAwrXyPCnecEl/0wXwparCRksM5TeuS/wZK+KA="];
  };

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {nixpkgs, ...}: let
    forAllSystems = nixpkgs.lib.genAttrs [
      "x86_64-linux"
      "aarch64-linux"
      # x86_64-darwin: dropped by nixpkgs 26.11
      "aarch64-darwin"
    ];
  in {
    packages = forAllSystems (system: {
      default = nixpkgs.legacyPackages.${system}.callPackage ./package.nix {};
    });

    overlays.default = final: _prev: {
      baton = final.callPackage ./package.nix {};
    };
  };
}

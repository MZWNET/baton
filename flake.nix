{
  description = "Keep the conversations of your coding agents in one append-only history and continue any of them in another agent";

  nixConfig = {
    extra-substituters = ["https://nix-cache.service.mzwing.eu.org"];
    extra-trusted-public-keys = ["nix-cache.service.mzwing.eu.org-1:ZI+74ziFu8O5j5jSAz45VTPo09uLYH7O8F2dBYBQYsk="];
  };

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

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

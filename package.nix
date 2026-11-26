{
  lib,
  pkgs,
}: let
  cargoNix = import ./Cargo.nix {inherit pkgs;};
in
  cargoNix.workspaceMembers.baton-cli.build.overrideAttrs (old: {
    name = "baton-${(lib.importTOML ./Cargo.toml).workspace.package.version}";

    # buildRustCrate's split "lib" output and the binary reference each other, a cycle Nix rejects
    outputs = ["out"];
    outputDev = ["out"];
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      cp -rP target/bin/* $out/bin/
      runHook postInstall
    '';

    meta =
      old.meta
      // {
        description = "Keep the conversations of your coding agents in one append-only history and continue any of them in another agent";
        homepage = "https://github.com/MZWNET/baton";
        license = lib.licenses.mit;
        mainProgram = "baton";
      };
  })

{
  git,
  lib,
  rustPlatform,
  tzdata,
}:
rustPlatform.buildRustPackage {
  pname = "baton";
  inherit ((lib.importTOML ./Cargo.toml).workspace.package) version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.lock
      ./Cargo.toml
      ./crates
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeCheckInputs = [git];

  # The Nix sandbox has no time zone database
  preCheck = "export TZDIR=${tzdata}/share/zoneinfo";

  meta = {
    description = "Keep the conversations of your coding agents in one append-only history and continue any of them in another agent";
    homepage = "https://github.com/MZWNET/baton";
    license = lib.licenses.mit;
    mainProgram = "baton";
  };
}

{pkgs, ...}: {
  languages.rust = {
    enable = true;
    channel = "nightly";
    lsp.enable = true;
    # on NixOS, uncomment the following line
    # cranelift.enable = true;
    # wild.enable = true;
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
  };

  packages = [
    pkgs.cargo-pretty
    pkgs.crate2nix
  ];

  enterTest = ''
    rustc --version
    cargo --version
    which cargo-pretty
    crate2nix --version
    rustfmt --version
    clippy-driver --version
    rust-analyzer --version
  '';
}

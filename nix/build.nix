let
  rustCommit = "b52a8b7de89b1fac49302cbaffd4caed4551515f";
  overlay = import (
    builtins.fetchTarball "https://github.com/mozilla/nixpkgs-mozilla/archive/${rustCommit}.tar.gz"
  );
  pkgs = import ./nixpkgs.nix {
    overlays = [ overlay ];
  };
  ruststable = (pkgs.latest.rustChannels.stable.rust.override {
    extensions = [
      "clippy-preview"
      "rustfmt-preview"
    ];
  });
  cargo-kcov = pkgs.callPackage ./cargo-kcov.nix {};
  deps = with pkgs; (import ./default.nix) ++ [
    binutils
    cargo-kcov
    coreutils
    curl
    gcc
    git
    kcov
    nix-prefetch-git
    ruststable
  ];
in deps

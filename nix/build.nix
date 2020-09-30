let
  rustCommit = "8c007b60731c07dd7a052cce508de3bb1ae849b4";
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
  deps = with pkgs; (import ./default.nix) ++ [
    (pkgs.callPackage ./derivations/cargo-kcov.nix { })
    binutils
    coreutils
    curl
    gcc
    git
    kcov
    nix-prefetch-git
    procps
    ruststable
  ];
in
deps

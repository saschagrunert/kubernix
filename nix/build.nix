let
  rustCommit = "d46240e8755d91bc36c0c38621af72bf5c489e13";
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
    (pkgs.callPackage ./derivations/cargo-kcov.nix {})
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
in deps

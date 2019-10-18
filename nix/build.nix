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

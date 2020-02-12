let
  rustCommit = "e912ed483e980dfb4666ae0ed17845c4220e5e7c";
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

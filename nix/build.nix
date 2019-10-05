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
  deps = import ./deps.nix;
  cargo-kcov = pkgs.callPackage ./cargo-kcov.nix {};
in
pkgs.stdenv.mkDerivation {
  buildInputs = with pkgs; deps ++ [ cargo-kcov git kcov ruststable ];
  LANG = "en_US.UTF-8";
  name = "build-shell";
}

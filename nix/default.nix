# Contains only the needed runtime dependencies
let
  pkgs = import ./nixpkgs.nix {};
in
pkgs.stdenv.mkDerivation {
  buildInputs = import ./deps.nix;
  LANG = "en_US.UTF-8";
  name = "kubernix-shell";
}

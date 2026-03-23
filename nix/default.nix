let
  pkgs = import ./nixpkgs.nix { overlays = [ (import ./overlay.nix) ]; };
in
pkgs.mkShell {
  buildInputs = import ./packages.nix;
}

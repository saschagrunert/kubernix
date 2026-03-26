# Backward-compatible shell for non-flake usage (requires Nix 2.4+).
# Prefer: nix develop
let
  lock = builtins.fromJSON (builtins.readFile ../flake.lock);
  nixpkgsLock = lock.nodes.nixpkgs.locked;
  nixpkgs = import (builtins.fetchTarball {
    name = "nixos-unstable";
    url = "https://github.com/${nixpkgsLock.owner}/${nixpkgsLock.repo}/archive/${nixpkgsLock.rev}.tar.gz";
    sha256 = nixpkgsLock.narHash;
  });
  pkgs = nixpkgs { overlays = [ (import ./overlay.nix) ]; };
in
pkgs.mkShell {
  buildInputs = import ./packages.nix { inherit pkgs; };
}

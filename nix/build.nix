# Backward-compatible build shell for non-flake usage (requires Nix 2.4+).
# Prefer: nix develop .#build
let
  lock = builtins.fromJSON (builtins.readFile ../flake.lock);
  nixpkgsLock = lock.nodes.nixpkgs.locked;
  nixpkgs = import (builtins.fetchTarball {
    name = "nixos-unstable";
    url = "https://github.com/${nixpkgsLock.owner}/${nixpkgsLock.repo}/archive/${nixpkgsLock.rev}.tar.gz";
    sha256 = nixpkgsLock.narHash;
  });
  pkgs = nixpkgs { overlays = [ (import ./overlay.nix) ]; };
  deps = (import ./packages.nix { inherit pkgs; }) ++ (with pkgs; [
    cargo-llvm-cov
    binutils
    clippy
    coreutils
    curl
    gcc
    git
    nix-prefetch-git
    procps
    rustc
    cargo
    rustfmt
  ]);
in
pkgs.mkShell {
  buildInputs = deps;
}

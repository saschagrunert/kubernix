let
  pkgs = import ./nixpkgs.nix {
    overlays = [ (import ./overlay.nix) ];
  };
  deps = (import ./packages.nix) ++ (with pkgs; [
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

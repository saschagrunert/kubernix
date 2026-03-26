{
  description = "KuberNix - Single dependency Kubernetes clusters for local testing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems f;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import ./nix/overlay.nix) ];
          };
          runtimePackages = import ./nix/packages.nix { inherit pkgs; };
        in
        {
          default = pkgs.mkShell {
            buildInputs = runtimePackages;
          };

          build = pkgs.mkShell {
            buildInputs = runtimePackages ++ (
              with pkgs;
              [
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
              ]
            );
          };
        }
      );
    };
}

{
  description = "KuberNix runtime environment";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      system = "KUBERNIX_SYSTEM";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import ./overlay.nix) ];
      };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        buildInputs = import ./packages.nix { inherit pkgs; };
      };
    };
}

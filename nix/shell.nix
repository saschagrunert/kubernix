let
  pkgs = import ./nixpkgs.nix {};
in
pkgs.stdenv.mkDerivation {
  buildInputs = with pkgs; [
    bash
    cargo
    cfssl
    conmon
    conntrack-tools
    cni-plugins
    cri-o
    cri-tools
    etcd
    iproute
    iptables
    kubernetes
    runc
    socat
    rustPackages.clippy
    rustPackages.rustfmt
    utillinux
  ];

  LANG = "en_US.UTF-8";

  shellHook = ''
    export CONTAINER_RUNTIME_ENDPOINT="unix://$PWD/run/crio/crio.sock"
    export KUBECONFIG="run/kube/admin.kubeconfig"
  '';

  name = "shell";
}

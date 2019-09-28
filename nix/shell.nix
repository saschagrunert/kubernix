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
  KUBECONFIG="run/kube/admin.kubeconfig";
  name = "shell";
}

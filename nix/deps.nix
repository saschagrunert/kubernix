let
  pkgs = import ./nixpkgs.nix { overlays = [(import ./overlay.nix)]; };
  deps = with pkgs; [
    bash
    cacert
    cfssl
    cni-plugins
    conmon
    conntrack-tools
    cri-o
    cri-tools
    curl
    etcd
    iproute
    iptables
    kubernetes
    kubernetes-helm
    runc
    socat
    utillinux
    watch
  ];
in deps

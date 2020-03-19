let
  pkgs = import ./nixpkgs.nix { overlays = [(import ./overlay.nix)]; };
  packages = with pkgs; [
    cacert
    cfssl
    cni-plugins
    conmon
    conntrack-tools
    cri-o
    cri-tools
    etcd
    iproute
    iptables
    kmod
    kubernetes
    kubectl
    podman
    runc
    socat
    sysctl
    utillinux
  ] ++ [ /* PACKAGES */ ];
in packages

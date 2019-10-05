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
    runc
    socat
    utillinux
    watch
  ] ++ [ /* PACKAGES */ ];
in deps

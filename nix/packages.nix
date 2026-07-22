{ pkgs }:
with pkgs;
[
  cacert
  cfssl
  cni-plugins
  conmon
  conntrack-tools
  containerd
  cri-o
  cri-tools
  crun
  etcd
  iproute2
  iptables
  kmod
  kubectl
  kubernetes
  podman
  socat
  sysctl
  util-linux
]
++ [ /* PACKAGES */ ]

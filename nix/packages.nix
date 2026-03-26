{ pkgs }:
with pkgs;
[
  cacert
  cfssl
  cni-plugins
  conmon
  conntrack-tools
  cri-o
  cri-tools
  etcd
  iproute2
  iptables
  kmod
  kubernetes
  kubectl
  podman
  crun
  socat
  sysctl
  util-linux
]
++ [ /* PACKAGES */ ]

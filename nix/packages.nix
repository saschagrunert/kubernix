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
  jq
  kmod
  kubectl
  kubernetes
  podman
  rootlesskit
  socat
  sysctl
  util-linux
]
++ [ /* PACKAGES */ ]

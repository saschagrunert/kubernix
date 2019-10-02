let
  overlay = import (
    builtins.fetchTarball https://github.com/mozilla/nixpkgs-mozilla/archive/master.tar.gz
  );
  pkgs = import ./nixpkgs.nix {
    overlays = [ overlay ];
  };
  ruststable = (pkgs.latest.rustChannels.stable.rust.override {
    extensions = [
      "clippy-preview"
      "rustfmt-preview"
    ];
  });
in
pkgs.stdenv.mkDerivation {
  buildInputs = with pkgs; [
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
    fish
    git
    iproute
    iptables
    kubernetes
    kubernetes-helm
    runc
    ruststable
    socat
    utillinux
    watch
  ];

  LANG = "en_US.UTF-8";

  shellHook = ''
    export PS1="> "
  '';

  name = "shell";
}

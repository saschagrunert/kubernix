FROM docker.io/nixos/nix:latest
COPY {nix} {root}
RUN nix-shell {root}/default.nix --run "echo bootstrap done"
ENTRYPOINT [ "/bin/sh", "-c", "nix-shell {root}/default.nix --run \"$*\"", "--" ]

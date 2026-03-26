FROM docker.io/nixos/nix:latest
RUN echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf
COPY {nix} {root}
RUN nix develop path:{root} --no-update-lock-file --command echo "bootstrap done"
ENTRYPOINT [ "/bin/sh", "-c", "nix develop path:{root} --no-update-lock-file --command bash -c \"$*\"", "--" ]

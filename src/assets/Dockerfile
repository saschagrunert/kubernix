FROM docker.io/nixos/nix:latest
COPY {nix} {root}
RUN nix run -f {root} -c echo bootstrap done
ENTRYPOINT [ "nix", "run", "-f", "{root}", "-c" ]

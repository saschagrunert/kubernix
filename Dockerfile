# static binary build
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
RUN cargo build --release

# nix dependency collection
FROM nixos/nix:latest AS bootstrapper
COPY nix /bootstrap
RUN nix run -f /bootstrap -c echo done

# target image
FROM nixos/nix:latest
RUN apk add bash
ENV SHELL=/bin/bash
COPY --from=builder \
     /build/target/release/kubernix .
COPY --from=bootstrapper /nix /nix
ENTRYPOINT [ "/kubernix" ]

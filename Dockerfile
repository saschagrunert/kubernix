# static binary build
FROM ekidd/rust-musl-builder:stable AS builder
COPY . .
RUN cargo build --release

# nix dependency collection
FROM nixos/nix:latest as bootstrapper
COPY nix /bootstrap
RUN nix run -f /bootstrap -c echo done

# target image
FROM nixos/nix:latest
RUN apk add bash
ENV SHELL /bin/bash
COPY --from=builder \
     /home/rust/src/target/x86_64-unknown-linux-musl/release/kubernix .
COPY --from=bootstrapper /nix /nix
ENTRYPOINT [ "/kubernix" ]

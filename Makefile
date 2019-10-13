ARGS ?=
SUDO := sudo -E
KUBERNIX := $(SUDO) target/release/kubernix $(ARGS)
CONTAINER_RUNTIME := podman
IMAGE := saschagrunert/kubernix:latest

define nix-run
	nix run -if nix/build.nix -k SSH_AUTH_SOCK -c $(1)
endef

all: build

.PHONY: build
build:
	$(call nix-run,cargo build)

.PHONY: build-image
build-image:
	$(CONTAINER_RUNTIME) build -t $(IMAGE) .

.PHONY: build-release
build-release:
	$(call nix-run,cargo build --release)

.PHONY: coverage
coverage:
	$(call nix-run,cargo kcov)

.PHONY: docs
docs:
	$(call nix-run,cargo doc --no-deps)

.PHONY: lint-clippy
lint-clippy:
	$(call nix-run,cargo clippy --all -- -D warnings)

.PHONY: lint-rustfmt
lint-rustfmt:
	$(call nix-run,cargo fmt && git diff --exit-code)

.PHONY: nix
nix:
	$(call nix-run,$(shell which bash))

.PHONY: nixpkgs
nixpkgs:
	nix run -f channel:nixpkgs-unstable nix-prefetch-git -c nix-prefetch-git \
		--no-deepClone https://github.com/nixos/nixpkgs > nix/nixpkgs.json

.PHONY: run
run: build-release
	$(KUBERNIX)

.PHONY: run-image
run-image:
	$(CONTAINER_RUNTIME) run --rm --privileged --net=host -it $(IMAGE)

.PHONY: shell
shell: build-release
	$(KUBERNIX) shell

.PHONY: test-integration
test-integration: build-release
	$(SUDO) test/integration

.PHONY: test-unit
test-unit:
	$(call nix-run,cargo test)

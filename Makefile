ARGS ?=
SUDO := sudo -E
KUBERNIX := $(SUDO) target/release/kubernix $(ARGS)
CONTAINER_RUNTIME := sudo podman
IMAGE := docker.io/saschagrunert/kubernix
RUN_DIR := $(shell pwd)/kubernix-run

define nix
	nix run -f nix/build.nix $(1)
endef

define nix-run
	$(call nix,-c $(1))
endef

define nix-run-pure
	$(call nix,-ik SSH_AUTH_SOCK -c $(1))
endef

all: build

.PHONY: build
build:
	$(call nix-run-pure,cargo build)

.PHONY: build-image
build-image:
	$(CONTAINER_RUNTIME) build -t $(IMAGE) .

.PHONY: build-release
build-release:
	$(call nix-run-pure,cargo build --release)

.PHONY: coverage
coverage:
	$(call nix-run-pure,cargo kcov)

.PHONY: docs
docs:
	$(call nix-run-pure,cargo doc --no-deps)

.PHONY: lint-clippy
lint-clippy:
	$(call nix-run-pure,cargo clippy --all -- -D warnings)

.PHONY: lint-rustfmt
lint-rustfmt:
	$(call nix-run-pure,cargo fmt && git diff --exit-code)

.PHONY: nix
nix:
	$(call nix-run-pure,$(shell which bash))

.PHONY: nixpkgs
nixpkgs:
	nix run -f channel:nixpkgs-unstable nix-prefetch-git -c nix-prefetch-git \
		--no-deepClone https://github.com/nixos/nixpkgs > nix/nixpkgs.json

.PHONY: run
run: build-release
	$(KUBERNIX)

.PHONY: run-image
run-image:
	mkdir -p $(RUN_DIR)
	if [ -d /dev/mapper ]; then \
		DEV_MAPPER=-v/dev/mapper:/dev/mapper ;\
	fi ;\
	$(CONTAINER_RUNTIME) run \
		-v $(RUN_DIR):/kubernix-run \
		--rm \
		--privileged \
		--net=host \
		$$DEV_MAPPER \
		-it $(IMAGE) $(ARGS)

.PHONY: shell
shell: build-release
	$(KUBERNIX) shell

.PHONY: test-integration
test-integration: build-release
	$(call nix-run,cargo test --test integration -- --test-threads=1 --nocapture)

.PHONY: test-unit
test-unit:
	$(call nix-run-pure,cargo test --lib)

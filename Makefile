SUDO := sudo -E

define nix-shell
	$(1) nix-shell nix/shell.nix $(2)
endef

define nix-shell-pure
	$(call nix-shell,$(1),--keep SSH_AUTH_SOCK --pure $(2))
endef

define nix-shell-run
	$(call nix-shell,$(1),--run "$(2)")
endef

define nix-shell-pure-run
	$(call nix-shell-pure,$(1),--run "$(2)")
endef

all: build

.PHONY: build
build:
	$(call nix-shell-pure-run,,cargo build)

.PHONY: build-release
build-release:
	$(call nix-shell-pure-run,,cargo build --release)

.PHONY: docs
docs:
	$(call nix-shell-pure-run,,cargo doc --no-deps)

.PHONY: nixpkgs
nixpkgs:
	nix-shell -p nix-prefetch-git --run "nix-prefetch-git --no-deepClone \
		https://github.com/nixos/nixpkgs > nix/nixpkgs.json"

.PHONY: shell
shell:
	$(call nix-shell-pure,$(SUDO))

.PHONY: test
test:
	$(call nix-shell-pure-run,,cargo test)

.PHONY: run
run:
	$(call nix-shell-pure-run,$(SUDO),cargo run --release)

.PHONY: lint-clippy
lint-clippy:
	$(call nix-shell-pure-run,,cargo clippy --all -- -D warnings)

.PHONY: lint-rustfmt
lint-rustfmt:
	$(call nix-shell-pure-run,,cargo fmt)
	$(call nix-shell-pure-run,,git diff --exit-code)

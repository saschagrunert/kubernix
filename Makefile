define nix-shell
	$(1) nix-shell nix/shell.nix $(2)
endef

define nix-shell-pure
	$(call nix-shell,$(1),--pure $(2))
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
	$(call nix-shell-run,,cargo build)

.PHONY: build-release
build-release:
	$(call nix-shell-run,,cargo build --release)

.PHONY: nixpkgs
nixpkgs:
	nix-shell -p nix-prefetch-git --run "nix-prefetch-git --no-deepClone \
		https://github.com/nixos/nixpkgs > nix/nixpkgs.json"

.PHONY: shell
shell:
	$(call nix-shell-pure,sudo)

.PHONY: test
test:
	$(call nix-shell-run,,cargo test)

.PHONY: run
run:
	$(call nix-shell-pure-run,sudo,cargo run --release)

.PHONY: lint-clippy
lint-clippy:
	$(call nix-shell-run,,cargo clippy)

.PHONY: lint-rustfmt
lint-rustfmt:
	$(call nix-shell-run,,cargo fmt)
	$(call nix-shell-run,,git diff --exit-code)

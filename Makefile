ARGS ?=
# Empty by default, which runs kubernix rootless. Use
# SUDO='sudo -E env "PATH=$$PATH"' to run it as root instead, which keeps the
# PATH to find nix also with the secure_path of sudo (make turns $$ into $).
SUDO ?=
ZEITGEIST_VERSION ?= v0.8.0
HOST_ARCH ?= $(shell uname -m)
ZEITGEIST_ARCH ?= $(subst aarch64,arm64,$(subst x86_64,amd64,$(HOST_ARCH)))
# Checksums of the zeitgeist release binaries, update them together with
# ZEITGEIST_VERSION from the verified checksums.txt of the release:
#   cosign verify-blob checksums.txt --bundle checksums.txt.sigstore.json \
#     --certificate-identity https://github.com/kubernetes-sigs/zeitgeist/.github/workflows/release.yml@refs/tags/<version> \
#     --certificate-oidc-issuer https://token.actions.githubusercontent.com
ZEITGEIST_SHA256_amd64 := b004b91e0ad881732f2ba0e63def2e3c35950323e221b50c50cbad75b3f5c2b1
ZEITGEIST_SHA256_arm64 := 52e3b1af8ce2fc304b21cfa380a37bc1c4c6a48015063fdbeab6516123804e55
ZEITGEIST_SHA256 ?= $(ZEITGEIST_SHA256_$(ZEITGEIST_ARCH))

# Avoid cargo/nix warnings about HOME ownership when make itself runs as root
# with the HOME of the invoking user (for example via sudo -E make).
ifeq ($(shell id -u),0)
export HOME := /root
endif
KUBERNIX ?= $(SUDO) target/release/kubernix $(ARGS)

# A cluster started as root leaves a root-owned run root behind, which the
# rootless default can neither stop nor remove.
define check_root_owned
@if [ -z '$(SUDO)' ] && [ "$$(id -u)" -ne 0 ]; then \
	for f in kubernix-run kubernix-run/kubernix.pid; do \
		if [ -e "$$f" ] && [ "$$(stat -c %u "$$f")" -eq 0 ]; then \
			echo "$$f is owned by root, rerun via: make $@ SUDO='sudo -E env \"PATH=\$$\$$PATH\"'" >&2; \
			exit 1; \
		fi; \
	done; \
fi
endef

BUILD_DIR ?= build

COLOR := \033[36m
NOCOLOR := \033[0m
WIDTH := 30

all: build

.PHONY: help
help: ## Display this help.
	@awk \
		-v "col=${COLOR}" -v "nocol=${NOCOLOR}" \
		' \
			BEGIN { \
				FS = ":.*##" ; \
				printf "Usage:\n  make %s<target>%s\n", col, nocol \
			} \
			/^[./a-zA-Z_-]+:.*?##/ { \
				printf "  %s%-${WIDTH}s%s %s\n", col, $$1, nocol, $$2 \
			} \
			/^##@/ { \
				printf "\n%s\n", substr($$0, 5) \
			} \
		' $(MAKEFILE_LIST)

##@ Nix targets:

.PHONY: nix-update
nix-update: ## Update the pinned nixpkgs to the latest nixpkgs-unstable and the README versions.
	nix flake update nixpkgs
	$(MAKE) update-readme

.PHONY: update-readme
update-readme: ## Regenerate the README versions table from the nix devShell.
	hack/update-readme-versions.sh

##@ Build targets:

.PHONY: clean
clean: kill ## Remove runtime data and build artifacts (after running as root use SUDO='sudo -E env "PATH=$$PATH"').
	cargo clean
	@# Rootless storage left by an unclean exit belongs to subordinate users,
	@# so retry in a user namespace which maps them.
	$(SUDO) rm -rf kubernix-run || unshare --map-auto --map-root-user rm -rf kubernix-run

.PHONY: build
build: ## Build in debug mode.
	cargo build

.PHONY: build-release
build-release: ## Build in release mode.
	cargo build --release

STATIC_TARGET ?= $(HOST_ARCH)-unknown-linux-gnu
# Set to "cargo auditable" to embed the dependency tree into the binary.
STATIC_CARGO ?= cargo

.PHONY: build-static
build-static: ## Build the static release binary for the host architecture.
	RUSTFLAGS="-C target-feature=+crt-static" $(STATIC_CARGO) build --release --target $(STATIC_TARGET)
	ldd target/$(STATIC_TARGET)/release/kubernix 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

CROSS_TARGET ?= x86_64-unknown-linux-gnu

.PHONY: build-cross
build-cross: ## Cross-compile a static, stripped release binary via cross (set CROSS_TARGET).
	RUSTFLAGS="-C target-feature=+crt-static" cross build --release --target $(CROSS_TARGET)

.PHONY: release-snapshot
release-snapshot: ## Build snapshot binaries for all supported targets.
	$(MAKE) build-cross CROSS_TARGET=x86_64-unknown-linux-gnu
	$(MAKE) build-cross CROSS_TARGET=aarch64-unknown-linux-gnu

.PHONY: docs
docs: ## Build the documentation.
	cargo doc --no-deps

##@ Lint targets:

.PHONY: lint-clippy
lint-clippy: ## Run clippy linter.
	cargo clippy --all --all-targets -- -D warnings

.PHONY: lint-rustfmt
lint-rustfmt: ## Check code formatting.
	cargo fmt --check

.PHONY: lint-audit
lint-audit: ## Audit dependencies for security vulnerabilities.
	cargo audit

.PHONY: lint-dependencies
lint-dependencies: ## Validate dependency versions via zeitgeist.
	@test -x $(BUILD_DIR)/zeitgeist || { \
		echo "Installing zeitgeist $(ZEITGEIST_VERSION)..."; \
		test -n "$(ZEITGEIST_SHA256)" || { echo "No zeitgeist checksum for $(ZEITGEIST_ARCH)" >&2; exit 1; }; \
		mkdir -p $(BUILD_DIR); \
		curl -sSfL https://github.com/kubernetes-sigs/zeitgeist/releases/download/$(ZEITGEIST_VERSION)/zeitgeist-$(ZEITGEIST_ARCH)-linux \
			-o $(BUILD_DIR)/zeitgeist.tmp && \
		echo "$(ZEITGEIST_SHA256)  $(BUILD_DIR)/zeitgeist.tmp" | sha256sum -c --quiet && \
		chmod +x $(BUILD_DIR)/zeitgeist.tmp && mv $(BUILD_DIR)/zeitgeist.tmp $(BUILD_DIR)/zeitgeist || \
		{ rm -f $(BUILD_DIR)/zeitgeist.tmp; exit 1; }; \
	}
	$(BUILD_DIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

.PHONY: lint-deny
lint-deny: ## Check licenses and duplicate dependencies via cargo-deny.
	cargo deny check

SHELL_SCRIPTS := \
	contrib/check-leftovers \
	contrib/integration-test \
	contrib/prepare-system \
	hack/update-readme-versions.sh \
	src/assets/oci-rootless.sh

.PHONY: lint-shellcheck
lint-shellcheck: ## Lint the shell scripts via shellcheck.
	shellcheck $(SHELL_SCRIPTS)

.PHONY: lint-vex
lint-vex: ## Validate the OpenVEX document.
	jq -e -f hack/lint-vex.jq .openvex/kubernix.openvex.json >/dev/null

.PHONY: lint-markdown
lint-markdown: ## Lint the Markdown files via markdownlint.
	markdownlint-cli2 "**/*.md"

.PHONY: lint-prettier
lint-prettier: ## Check the formatting of the Markdown, YAML and JSON files via prettier.
	prettier --check .

.PHONY: lint
lint: lint-clippy lint-rustfmt lint-audit lint-deny lint-dependencies lint-shellcheck lint-vex lint-markdown lint-prettier ## Run all linters.

##@ Run targets:

.PHONY: run
run: build-release ## Run kubernix rootless, or as root with SUDO='sudo -E env "PATH=$$PATH"', pass options via ARGS.
	$(check_root_owned)
	$(KUBERNIX)

.PHONY: shell
shell: build-release ## Spawn an additional shell for a running kubernix cluster.
	$(check_root_owned)
	$(KUBERNIX) shell

# Builds kubernix first, otherwise clean would remove the run root of a
# running cluster or with active mounts.
.PHONY: kill
kill: build-release ## Stop the cluster and unmount leftover volumes via kubernix stop (set SUDO like for run if it runs as root).
	$(check_root_owned)
	@if [ -f kubernix-run/kubernix.toml ]; then \
		$(KUBERNIX) stop; status=$$?; [ $$status -eq 0 ] || [ $$status -eq 3 ]; \
	fi

##@ Test targets:

.PHONY: test-unit
test-unit: ## Run unit tests.
	cargo test --lib

.PHONY: test-coverage
test-coverage: ## Generate local LCOV coverage report (requires cargo-llvm-cov).
	cargo llvm-cov --lib --lcov --output-path lcov.info

.PHONY: test
test: test-unit ## Run the unit tests (integration tests run in CI).

ARGS ?=
# Empty by default, which runs kubernix rootless. Use SUDO="sudo -E" to run
# it as root instead.
SUDO ?=
ZEITGEIST_VERSION ?= v0.8.0
HOST_ARCH ?= $(shell uname -m)
ZEITGEIST_ARCH ?= $(subst aarch64,arm64,$(subst x86_64,amd64,$(HOST_ARCH)))

# Avoid cargo/nix warnings about HOME ownership when make itself runs as root
# with the HOME of the invoking user (for example via sudo -E make).
ifeq ($(shell id -u),0)
export HOME := /root
endif
KUBERNIX ?= $(SUDO) target/release/kubernix $(ARGS)

# A cluster started as root leaves a root-owned run root behind, which the
# rootless default can neither stop nor remove.
define check_root_owned
@if [ -z "$(SUDO)" ] && [ "$$(id -u)" -ne 0 ]; then \
	for f in kubernix-run kubernix-run/kubernix.pid; do \
		if [ -e "$$f" ] && [ "$$(stat -c %u "$$f")" -eq 0 ]; then \
			echo "$$f is owned by root, rerun via: make $@ SUDO=\"sudo -E\"" >&2; \
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
clean: kill ## Remove runtime data and build artifacts (use SUDO="sudo -E" after running as root).
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
	cargo clippy --all -- -D warnings

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
		mkdir -p $(BUILD_DIR); \
		curl -sSfL https://github.com/kubernetes-sigs/zeitgeist/releases/download/$(ZEITGEIST_VERSION)/zeitgeist-$(ZEITGEIST_ARCH)-linux \
			-o $(BUILD_DIR)/zeitgeist && chmod +x $(BUILD_DIR)/zeitgeist; \
	}
	$(BUILD_DIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

.PHONY: lint-deny
lint-deny: ## Check licenses and duplicate dependencies via cargo-deny.
	cargo deny check

SHELL_SCRIPTS := \
	contrib/install-cfssl \
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

.PHONY: lint
lint: lint-clippy lint-rustfmt lint-audit lint-deny lint-dependencies lint-shellcheck lint-vex ## Run all linters.

##@ Run targets:

.PHONY: run
run: build-release ## Run kubernix rootless, or as root with SUDO="sudo -E", pass options via ARGS.
	$(check_root_owned)
	$(KUBERNIX)

.PHONY: shell
shell: build-release ## Spawn an additional shell for a running kubernix cluster.
	$(check_root_owned)
	$(KUBERNIX) shell

.PHONY: kill
kill: ## Kill a running kubernix instance and unmount leftover volumes (use SUDO="sudo -E" if it runs as root).
	$(check_root_owned)
	@if [ -f kubernix-run/kubernix.pid ]; then \
		$(SUDO) kill $$(cat kubernix-run/kubernix.pid) 2>/dev/null; \
		echo "Waiting for kubernix to shut down..."; \
		timeout 15 sh -c 'while [ -f kubernix-run/kubernix.pid ]; do sleep 1; done' 2>/dev/null; \
	fi
	$(SUDO) sh -c 'grep -s kubernix-run /proc/mounts | cut -d" " -f2 | sort -r | xargs -r umount' 2>/dev/null || true

##@ Test targets:

.PHONY: test-unit
test-unit: ## Run unit tests.
	cargo test --lib

.PHONY: test-coverage
test-coverage: ## Generate local LCOV coverage report (requires cargo-llvm-cov).
	cargo llvm-cov --lib --lcov --output-path lcov.info

.PHONY: test
test: test-unit ## Run the unit tests (integration tests run in CI).

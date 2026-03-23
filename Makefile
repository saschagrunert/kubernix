ARGS ?=
SUDO ?= sudo -E
ZEITGEIST_VERSION ?= v0.5.4

# Avoid cargo/nix warnings about HOME ownership when running via sudo
ifeq ($(shell id -u),0)
export HOME := /root
endif
KUBERNIX ?= $(SUDO) target/release/kubernix $(ARGS)
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

##@ Build targets:

.PHONY: build
build: ## Build in debug mode.
	cargo build

.PHONY: build-release
build-release: ## Build in release mode.
	cargo build --release

.PHONY: build-static
build-static: ## Build the static release binary.
	RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu
	strip -s target/x86_64-unknown-linux-gnu/release/kubernix
	ldd target/x86_64-unknown-linux-gnu/release/kubernix 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

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
		curl -sSfL https://github.com/kubernetes-sigs/zeitgeist/releases/download/$(ZEITGEIST_VERSION)/zeitgeist-amd64-linux \
			-o $(BUILD_DIR)/zeitgeist && chmod +x $(BUILD_DIR)/zeitgeist; \
	}
	$(BUILD_DIR)/zeitgeist validate --local-only --base-path . --config dependencies.yaml

.PHONY: lint
lint: lint-clippy lint-rustfmt lint-audit lint-dependencies ## Run all linters.

##@ Run targets:

.PHONY: run
run: build-release ## Run kubernix.
	$(KUBERNIX)

.PHONY: shell
shell: build-release ## Run kubernix with a shell.
	$(KUBERNIX) shell

##@ Test targets:

.PHONY: test-unit
test-unit: ## Run unit tests.
	cargo test --lib

.PHONY: test
test: test-unit ## Run all tests.

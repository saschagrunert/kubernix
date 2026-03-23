ARGS ?=
SUDO ?= sudo -E

# Avoid cargo/nix warnings about HOME ownership when running via sudo
ifeq ($(shell id -u),0)
export HOME := /root
endif
KUBERNIX ?= $(SUDO) target/release/kubernix $(ARGS)
CONTAINER_RUNTIME ?= $(SUDO) podman
RUN_DIR ?= $(shell pwd)/kubernix-run

export IMAGE ?= docker.io/saschagrunert/kubernix

NIX_BUILD ?= nix/build.nix

define nix-run
	nix-shell $(NIX_BUILD) --run '$(1)'
endef

define nix-run-pure
	nix-shell $(NIX_BUILD) --pure --run '$(1)'
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
	$(call nix-run-pure,cargo llvm-cov --lib --lcov --output-path lcov.info)

.PHONY: e2e
e2e:
	$(call nix-run,$(SUDO) \
		KUBERNETES_SERVICE_HOST=127.0.0.1 \
		KUBERNETES_SERVICE_PORT=6443 \
		KUBECONFIG=$(RUN_DIR)/kubeconfig/admin.kubeconfig \
		e2e.test \
			--provider=local \
			--ginkgo.focus='.*$(FOCUS).*' \
			--ginkgo.progress \
			$(ARGS) \
	)

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

.PHONY: nixdeps
nixdeps:
	@echo '| Application | Version |'
	@echo '| - | - |'
	@nix-instantiate nix/packages.nix 2>/dev/null \
		| sed -n 's;/nix/store/[[:alnum:]]\{32\}-\(.*\)-\(.*\)\.drv\(!bin\)\{0,1\};| \1 | v\2 |;p' \
		| sort

.PHONY: nixpkgs
nixpkgs:
	@nix-shell -p nix-prefetch-git --run \
		'nix-prefetch-git --no-deepClone https://github.com/nixos/nixpkgs' > nix/nixpkgs.json

.PHONY: run
run: build-release
	$(KUBERNIX)

.PHONY: run-image
run-image:
	$(SUDO) contrib/prepare-system
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

define test
	$(call nix-run,\
		cargo test \
			--test $(1) $(ARGS) \
			-- \
			--test-threads 1 \
			--nocapture)
endef

.PHONY: test-integration
test-integration:
	$(call test,integration)

.PHONY: test-e2e
test-e2e:
	$(call test,e2e)

.PHONY: test-unit
test-unit:
	$(call nix-run-pure,cargo test --lib)

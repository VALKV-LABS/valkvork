# Valkvork developer tasks. `make help` lists them.

SHELL := /bin/bash
.DEFAULT_GOAL := help

# Newer macOS SDKs can break linking with the current Rust toolchain; the
# Command Line Tools SDK works. Override SDKROOT to use another one.
ifeq ($(shell uname -s),Darwin)
  CLT_SDK := /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk
  ifneq ($(wildcard $(CLT_SDK)),)
    export SDKROOT ?= $(CLT_SDK)
  endif
endif

PLATFORM ?= linux/$(shell uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')

.PHONY: help build build-release test test-unit fmt fmt-check check ci \
        dev-up dev-down dev-status dev-logs dev-reset env demo ui \
        dev-add-node dev-stop-node dev-remove-node \
        dist image e2e clean

help:  ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[36m%-17s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ── Build and test ───────────────────────────────────────────────────────────

build:  ## Build all binaries (debug, target/debug/)
	cargo build --workspace

build-release:  ## Build all binaries (release, target/release/)
	cargo build --workspace --release

test: test-unit  ## Alias for test-unit

test-unit:  ## Run all unit and behaviour tests (no Docker needed)
	cargo test --workspace

fmt:  ## Format the code
	cargo fmt --all

fmt-check:  ## Check formatting
	cargo fmt --all -- --check

check:  ## Clippy with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

ci: fmt-check check test-unit  ## Run what CI runs (fmt, clippy, tests)

# ── Local cluster: controller + agent on this machine, using local Docker ───

dev-up: build  ## Start a local controller and agent (TLS on) against local Docker
	@scripts/dev.sh up

dev-down:  ## Stop them (containers keep running and are re-adopted by dev-up)
	@scripts/dev.sh down

dev-status:  ## Show processes, nodes and services
	@scripts/dev.sh status

dev-logs:  ## Follow controller and agent logs
	@scripts/dev.sh logs

dev-reset:  ## Stop, remove the dev cluster's containers, delete .dev/
	@scripts/dev.sh reset

NAME ?= node2

dev-add-node:  ## Add a node (NAME=node2): a Colima VM with its own Docker, plus an agent
	@scripts/dev.sh add-node $(NAME)

dev-stop-node:  ## Stop a node's agent (NAME=node2); its containers keep running, as in a partition
	@scripts/dev.sh stop-node $(NAME)

dev-remove-node:  ## Drain a node (NAME=node2), then stop and remove it; FORCE=1 skips the drain
	@VALKVORK_DEV_FORCE=$(FORCE) scripts/dev.sh remove-node $(NAME)

env:  ## Print CLI env vars, use: eval "$(make -s env)"
	@scripts/dev.sh env

demo:  ## Deploy examples/web.toml (nginx on :18080) and examples/api.toml
	@eval "$$(scripts/dev.sh env)" && \
	  target/debug/valkvork apply -f examples/web.toml && \
	  target/debug/valkvork apply -f examples/api.toml && \
	  echo && echo "nginx answers on http://localhost:18080 once it is RUNNING (first run pulls the image)." && \
	  echo "Watch it: eval \"\$$(make -s env)\" && target/debug/valkvork workloads"

ui:  ## Open the web UI in the browser
	@url=https://localhost:$${VALKVORK_DEV_ADMIN_PORT:-50052}/ui/; \
	  echo "$$url  (admin token: $$(cat .dev/admin-token 2>/dev/null || echo 'run make dev-up first'))"; \
	  (command -v open >/dev/null && open "$$url") || (command -v xdg-open >/dev/null && xdg-open "$$url") || true

# ── Release artefacts ────────────────────────────────────────────────────────

dist:  ## Static musl binaries into dist/ (PLATFORM=linux/amd64 to cross-build)
	docker buildx build --platform $(PLATFORM) --target export --output dist/ .

e2e: dist  ## End-to-end test against your Docker: Postgres with secrets, bind address, check (~2 min)
	docker run --rm --network host -v /var/run/docker.sock:/var/run/docker.sock \
	  -v /var/tmp/vk-e2e:/var/tmp/vk-e2e -v "$(CURDIR)/dist:/dist:ro" \
	  -v "$(CURDIR)/examples:/examples:ro" \
	  -v "$(CURDIR)/scripts/e2e-docker.sh:/e2e.sh:ro" docker:27-cli sh /e2e.sh

image:  ## Controller and agent images (valkvork-controller:dev, valkvork-agent:dev)
	docker buildx build --platform $(PLATFORM) --load --target controller -t valkvork-controller:dev .
	docker buildx build --platform $(PLATFORM) --load --target agent -t valkvork-agent:dev .

clean:  ## Remove Cargo build artefacts (target/)
	cargo clean

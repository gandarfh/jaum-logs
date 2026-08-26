# jaum-logs: backlog orchestrator on top of Claude Code

BIN      := target/debug/jaum
DEMO_DIR := /tmp/jaum-demo

.DEFAULT_GOAL := help

.PHONY: help build release run list test fmt demo demo-setup clean install sidecar sidecar-test sidecar-install

help: ## List the available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'

build: ## Build the workspace (debug)
	cargo build

release: ## Build optimized
	cargo build --release

run: build ## Open the TUI over the .backlog/ of the current directory
	$(BIN)

list: build ## List the backlog without opening the TUI
	$(BIN) list

test: ## Run every workspace test
	cargo test

fmt: ## Format the code
	cargo fmt

demo-setup: build sidecar ## Build the demo sandbox (isolated HOME, sidecar, repo and task ready for play; requires bun)
	bash examples/demo/setup.sh $(DEMO_DIR) $(CURDIR)/$(BIN)

demo: demo-setup ## Build the demo sandbox and open the TUI on it
	cd $(DEMO_DIR) && HOME=$(DEMO_DIR)/home $(CURDIR)/$(BIN)

clean: ## Remove build artifacts
	cargo clean

install: release ## Install the jaum binary into ~/.cargo/bin
	cargo install --path crates/cli

sidecar: ## Build the sidecar bundle and smoke-check it under plain node
	cd sidecar && bun install && bun run build
	printf '{"type":"ping"}\n' | node sidecar/dist/jaum-sidecar.mjs | grep -q '"type":"pong"'

sidecar-test: ## Run the sidecar tests (bun test + typecheck)
	cd sidecar && bun install && bun run typecheck && bun test

sidecar-install: sidecar ## Install the bundle into ~/jaum/sidecar/ (used by the daemon)
	mkdir -p ~/jaum/sidecar
	cp sidecar/dist/jaum-sidecar.mjs ~/jaum/sidecar/jaum-sidecar.mjs

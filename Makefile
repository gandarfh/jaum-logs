# jaum-logs — orquestrador de backlog sobre o Claude Code

BIN      := target/debug/jaum
DEMO_DIR := /tmp/jaum-demo

.DEFAULT_GOAL := help

.PHONY: help build release run list test clippy fmt fmt-check check demo clean install

help: ## Lista os alvos disponiveis
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'

build: ## Compila o workspace (debug)
	cargo build

release: ## Compila otimizado
	cargo build --release

run: build ## Abre a TUI sobre o .backlog/ do diretorio atual
	$(BIN)

list: build ## Lista o backlog sem abrir a TUI
	$(BIN) list

test: ## Roda todos os testes do workspace
	cargo test

clippy: ## Lint com clippy (falha em qualquer warning)
	cargo clippy --all-targets -- -D warnings

fmt: ## Formata o codigo
	cargo fmt

fmt-check: ## Verifica a formatacao (nao altera arquivos)
	cargo fmt --check

check: fmt-check clippy test ## Gate completo: formatacao + clippy + testes

demo: build ## Monta um sandbox de exemplo (HOME isolado) e abre a TUI nele
	rm -rf $(DEMO_DIR)
	mkdir -p $(DEMO_DIR)/home
	cp -R examples/demo/. $(DEMO_DIR)/
	cd $(DEMO_DIR) && HOME=$(DEMO_DIR)/home $(CURDIR)/$(BIN) init
	cd $(DEMO_DIR) && HOME=$(DEMO_DIR)/home $(CURDIR)/$(BIN)

clean: ## Remove os artefatos de build
	cargo clean

install: release ## Instala o binario jaum em ~/.cargo/bin
	cargo install --path crates/cli

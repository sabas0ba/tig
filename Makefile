# 本リポジトリに対する操作の入り口。利用可能な操作は `make help` で一覧する。

SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help

NIX ?= nix

.PHONY: help
help: ## 本ヘルプを表示する
	@printf '使用方法: make <target>\n\n'
	@pattern='^([a-zA-Z0-9_-]+):.*## (.*)$$'; \
	for makefile in $(MAKEFILE_LIST); do \
		while IFS= read -r line; do \
			line=$${line%$$'\r'}; \
			if [[ $$line =~ $$pattern ]]; then \
				printf '  \033[36m%-14s\033[0m %s\n' \
					"$${BASH_REMATCH[1]}" "$${BASH_REMATCH[2]}"; \
			fi; \
		done < "$$makefile"; \
	done

# --- 開発環境 ---------------------------------------------------------------

.PHONY: shell
shell: ## 開発シェルに入る (direnv 未使用時)
	$(NIX) develop

.PHONY: lock
lock: ## flake.lock を生成する
	$(NIX) flake lock

.PHONY: check
check: ## nix flake check を実行する (fmt / lint / test / クロスビルド)
	$(NIX) flake check

# --- Rust -------------------------------------------------------------------

.PHONY: fmt
fmt: ## Rust と Nix のコードを整形する
	cargo fmt
	$(NIX) fmt

.PHONY: lint
lint: ## clippy を実行する (警告をエラーとして扱う)
	cargo clippy --workspace --all-targets --all-features -- -D warnings

.PHONY: test
test: ## 単体テストと git との差分テストを実行する
	cargo test --workspace --all-features

.PHONY: build
build: ## host 向けにビルドする
	cargo build --workspace --all-features

.PHONY: wasm
wasm: ## core を wasm32-unknown-unknown 向けにビルドする (release)
	cargo build -p tig-core --all-features --target wasm32-unknown-unknown --release

.PHONY: thumb
thumb: ## core を thumbv7em-none-eabi 向けにビルドする (release)
	cargo build -p tig-core --all-features --target thumbv7em-none-eabi --release

.PHONY: size
size: wasm thumb ## クロスビルドの成果物サイズを表示する
	@ls -l target/wasm32-unknown-unknown/release/*.rlib \
		target/thumbv7em-none-eabi/release/*.rlib 2>/dev/null | awk '{print $$5, $$9}'

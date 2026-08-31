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

.PHONY: web
web: ## web frontend (wasm) をビルドして web/ に配置する
	cargo build -p tig-web --target wasm32-unknown-unknown --release
	cp target/wasm32-unknown-unknown/release/tig_web.wasm web/tig_web.wasm

.PHONY: serve
serve: web ## web frontend をローカルで配信する (http://127.0.0.1:8000)
	@command -v python3 >/dev/null || { echo "python3 が必要です (任意の静的サーバでも可)"; exit 1; }
	cd web && python3 -m http.server 8000

.PHONY: site
site: web ## GitHub Pages 用の静的サイト (landing + rustdoc + playground) を _site/ に構成する
	cargo doc --no-deps -p tig-core --all-features
	rm -rf _site
	mkdir -p _site/playground
	cp site/index.html _site/
	cp web/index.html web/app.js web/tig_web.wasm _site/playground/
	cp -r target/doc _site/doc
	rm -f _site/doc/.lock
	touch _site/.nojekyll

.PHONY: riscv
riscv: ## core を riscv32imac / riscv64gc (bare metal) 向けにビルドする (release)
	cargo build -p tig-core --all-features --target riscv32imac-unknown-none-elf --release
	cargo build -p tig-core --all-features --target riscv64gc-unknown-none-elf --release

.PHONY: mcu
mcu: ## 組み込み例 (staticlib) を thumbv7em / riscv32imac 向けにビルドする
	cargo build --manifest-path mcu/Cargo.toml --target thumbv7em-none-eabi --release
	cargo build --manifest-path mcu/Cargo.toml --target riscv32imac-unknown-none-elf --release

.PHONY: qemu
qemu: ## ベアメタルの例 (mcu/bare) を QEMU で実行して検証する (qemu-system-* が必要)
	scripts/qemu-bare.sh arm
	scripts/qemu-bare.sh riscv32
	scripts/qemu-bare.sh riscv64

.PHONY: size
size: wasm thumb riscv ## クロスビルドの成果物サイズを表示する
	@ls -l target/wasm32-unknown-unknown/release/*.rlib \
		target/thumbv7em-none-eabi/release/*.rlib \
		target/riscv32imac-unknown-none-elf/release/*.rlib \
		target/riscv64gc-unknown-none-elf/release/*.rlib 2>/dev/null | awk '{print $$5, $$9}'

.DEFAULT_GOAL := help

.PHONY: help build release test lint fmt fmt-check check check-features run json clean install upgrade

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z_-]+:.*##/ {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build (debug)
	cargo build

release: ## Build (release)
	cargo build --release

test: ## Run all tests (unit + integration + doc)
	cargo test

lint: ## Run clippy (warnings as errors)
	cargo clippy --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt

fmt-check: ## Check formatting without modifying files
	cargo fmt --check

check-features: ## Verify each feature combination compiles independently
	cargo check --no-default-features
	cargo check --no-default-features --features runtime
	cargo check --no-default-features --features config
	cargo check --no-default-features --features client
	cargo check --no-default-features --features frontend

# Needs the targets once: rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc
check-targets: ## Linux and Windows must keep compiling (warnings are errors)
	RUSTFLAGS="-D warnings" cargo check --target x86_64-unknown-linux-gnu
	RUSTFLAGS="-D warnings" cargo check --target x86_64-pc-windows-msvc

check: fmt-check lint check-features check-targets test ## CI aggregate: format + clippy + feature matrix + cross targets + tests

run: ## Foreground live view (fixed 2s sampling, q/Ctrl+C to exit)
	cargo run --quiet

json: ## Collect once, pretty-printed JSON
	cargo run --quiet -- --pretty

clean: ## Remove build artifacts
	cargo clean

install: ## Install zstats into the cargo bin directory
	cargo install --path .

upgrade: install ## Install, then restart the daemon on the new binary
	zstats stop
	zstats serve

.DEFAULT_GOAL := help

.PHONY: help build release test lint fmt fmt-check check check-features run once json clean install

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

check: fmt-check lint check-features test ## CI aggregate: format + clippy + feature matrix + tests

run: ## Watch mode (human-readable output, Ctrl+C to exit)
	cargo run --quiet -- --watch

once: ## Collect once, human-readable output
	cargo run --quiet

json: ## Collect once, pretty-printed JSON
	cargo run --quiet -- --pretty

clean: ## Remove build artifacts
	cargo clean

install: ## Install zstats into the cargo bin directory
	cargo install --path .

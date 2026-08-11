.DEFAULT_GOAL := help

.PHONY: help build release test lint fmt fmt-check check check-features check-targets \
	run json clean install upgrade version version-patch version-minor version-major

# Release version — read from Cargo.toml, the single source of truth the
# tag, the changelog and the published tarball names all derive from.
VERSION := $(shell sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)

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

check-targets: ## Linux and Windows must keep compiling (warnings are errors)
	@# Targets are per-toolchain; adding is idempotent and instant once installed
	rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc
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

# Release flow: `make version-patch` (bump + changelog), review the diff,
# commit, then `git tag vX.Y.Z && git push --tags` — the publish workflow
# fires on v*.*.* and builds the release assets. Nothing here commits,
# tags or pushes: those stay deliberate. Unlike projects that carry MSI /
# flatpak / deb metadata, zstats needs no secondary version sync — every
# published artifact derives its name from the git tag.

version: ## Prepend the changelog for the version currently in Cargo.toml
	git cliff --unreleased --tag v$(VERSION) --prepend CHANGELOG.md

# Bump Cargo.toml (+ Cargo.lock) via cargo-edit, then run `version` in a
# fresh make invocation — VERSION is expanded at parse time, so the
# recursive $(MAKE) is what picks up the just-bumped number.
version-patch: ## Bump the patch version, then update the changelog
	cargo set-version --bump patch
	$(MAKE) version

version-minor: ## Bump the minor version, then update the changelog
	cargo set-version --bump minor
	$(MAKE) version

version-major: ## Bump the major version, then update the changelog
	cargo set-version --bump major
	$(MAKE) version

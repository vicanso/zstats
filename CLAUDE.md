# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

zstats is a system performance metrics collection library + CLI tool (Rust, edition 2024). The design spec lives in `collector-core-design.md` — note the crate is named `collector-core` there, but the code was merged into the single `zstats` crate (lib + bin). The API follows the spec with one deviation: disk, network, and process collection are each toggleable in `CollectorConfig`, so their snapshot fields are all `Option<Vec<_>>` (`None` means "not collected", not "none found"; the spec has plain `Vec` for disks/networks). CPU, memory, and load are always collected — they cost only microseconds. The expensive parts are throttled: disk capacity (statfs, ~18ms on macOS) refreshes on its own slow cadence (`disk_storage_refresh_interval`, default 60s) while disk IO counters (~0.7ms) refresh every collect; processes (~9ms) are the second-largest cost and can run on their own cadence (`process_refresh_interval`; the `serve` CLI defaults it to 10s). While overall CPU usage is at or above `process_boost_cpu_percent` (default 15% — tuned for personal-device monitoring, roughly two busy cores on a 12-core machine), the process list refreshes every collect regardless — CPU/memory are refreshed first inside `collect()` precisely so this decision can use the fresh value.

The module's scope is **collecting local machine data only**: the "remote Collector (pulling snapshots from other machines via gRPC/HTTP)" extension mentioned in section 12 of the design doc has been explicitly rejected — do not design abstractions or features for it.

## Common Commands

```bash
make check          # CI aggregate: fmt-check + clippy(-D warnings) + feature matrix + tests; run before committing
make fmt            # Format (run this first when fmt-check fails)
make test           # All tests; equivalent to cargo test
make lint           # cargo clippy --all-targets -- -D warnings
make check-features # Verify each feature combination compiles independently
make once           # Collect once, human-readable output (fastest way to verify changes)
make json           # Collect once, pretty-printed JSON
make run            # Watch mode (--watch), Ctrl+C to exit

# Daemon mode (unix only)
cargo run -- serve --detach   # Background daemon keeping 5min history (--history <secs>)
cargo run -- attach           # Replay history + live view; Ctrl+C detaches
cargo run -- stop             # Stop the daemon

# Single test
cargo test --test integration scheduler_delivers   # Filter integration tests by name
cargo test rate::                                  # Unit tests in utils/rate.rs
```

## Architecture

The data flow is a one-way pipeline. `SystemSnapshot` (`src/snapshot.rs`) is the single data contract inside and outside the module:

```
Collector (trait, sync) ──SystemSnapshot──▶ Scheduler ──concurrent dispatch──▶ MetricSink (async trait) × N
```

Key designs that span multiple files:

- **Rate metrics rely on diffing state**: `LocalCollector` (`src/collector/local.rs`) keeps the previous sample's cumulative disk/network counters and an `Instant`, computing per-second values via `rate_per_sec` in `utils/rate.rs` (counter wraparound yields 0). Therefore **the first collect returns None/0 for rates** — both the tests and the CLI one-shot mode deliberately sample twice because of this. `Collector` is a stateful `&mut self` object; do not casually rebuild it.
- **The sync-collection / async-scheduling boundary**: `Collector::collect` is synchronous (sysinfo is a sync API). `Scheduler` (`src/scheduler.rs`) wraps it in `spawn_blocking` and enforces the config's `collect_timeout`; the collector is shared between the scheduling loop and `collect_once` via `Arc<Mutex<Box<dyn Collector>>>`.
- **Sink failure isolation**: each sink's `write` runs concurrently in its own task with its own timeout (default 5s); a failing/slow sink only produces a tracing log and never affects other sinks, and a failed collect just skips the round. Adding a backend only requires implementing `MetricSink` (`async_trait` is re-exported from lib.rs). `MetricSink` must stay dyn-compatible (`Arc<dyn MetricSink>`), which is why `async-trait` is used instead of native async fn in traits.
- **Scheduling policy**: `tokio::time::interval` + `MissedTickBehavior::Skip` — when a collect overruns the interval, missed ticks are skipped, never replayed.

The InfluxDB / Prometheus sinks from the design doc (P1/P2) are not implemented yet; the `src/sink/` layout is reserved for them.

## Cargo Features

Local collection is the core capability: sysinfo/`LocalCollector` are always available and never feature-gated. The default feature is `cli` (full functionality; `cargo build/test/install` behave as usual):

- No features: data contract + synchronous collection (`LocalCollector`), no tokio — for embedders that drive their own scheduling;
- `runtime`: adds `Scheduler` and the `sink` module (pulls in tokio/async-trait/tracing);
- `cli`: runtime + bin-only tokio features (signal etc.); the bin is bound via `required-features = ["cli"]`.

When adding a dependency, decide which layer it belongs to first. `make check-features` (included in `make check`) verifies each combination compiles independently — always run it after adding `cfg` gates. The integration tests are gated behind `runtime` as a whole.

## Conventions

- **All project documentation is in English** (this file, README, rustdoc, code comments), and so are CLI messages and logs. The only remaining Chinese content is the design doc (`collector-core-design.md`), which predates this rule.
- Every `.rs` file starts with the Apache 2.0 copyright header — do not omit it in new files.
- Error types are defined with snafu using struct variants (e.g. `SinkError::Remote { message }`). Beware selector-name clashes: same-named variants across enums in `error.rs` need `#[snafu(context(suffix(...)))]` (see the two `Timeout` variants). When adding variants that wrap an underlying failure, prefer a `source` field and snafu's context-selector idiom over stringifying.
- The CLI lives in `src/bin/zstats/` (`main.rs` hand-rolled arg parsing + `render.rs` text rendering + `daemon.rs` serve/attach/stop) and deliberately avoids clap; the lib contains no presentation logic — human-readable formatting belongs to the bin only. Default is one-shot text output, `--watch` for continuous, `--json`/`--pretty` for machines; keep the `USAGE` text in sync when adding flags.
- The daemon (unix only) serves a per-user Unix socket (`$TMPDIR/zstats-$USER.sock`) with a line protocol: client sends `ATTACH`/`STOP`; `ATTACH` gets `HISTORY <n>` + n JSON snapshots (ring buffer, retention by timestamp span) then a live JSON stream. Attach warms the rolling averages from history before rendering. SIGPIPE is reset to default in main so piped output dies silently.
- Daemon alerting (`src/bin/zstats/alerts.rs`, CLI layer by design — the lib stays alert-free): desktop notifications via osascript on macOS / notify-send elsewhere. Do NOT switch to notify-rust/notification-center APIs: identities masquerading as Terminal.app are silently dropped on this setup (show() still returns Ok, so no fallback is possible) — this was tested empirically; only the Script Editor identity (which osascript uses) displays. Both rules use 1-minute averages over a ≥50s-full window so only sustained behavior alerts: CPU ≥ `--alert-cpu` (default 30%, single-core units — catches "quietly always busy" apps, not just runaways) and memory share of total ≥ `--alert-mem` (default 25%; transient legitimate spikes are averaged away). Per-(pid, rule) re-alert cooldown via `--alert-cooldown` (default 600s); state resets when the process dies.

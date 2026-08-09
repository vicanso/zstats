# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

zstats is a system performance metrics collection library + CLI tool (Rust, edition 2024). The design spec lives in `collector-core-design.md` — note the crate is named `collector-core` there, but the code was merged into the single `zstats` crate (lib + bin). The API follows the spec with one deviation: disk, network, and process collection are each toggleable in `CollectorConfig`, so their snapshot fields are all `Option<...>` (`None` means "not collected", not "none found"; processes are `Option<Arc<Vec<_>>>` so snapshot clones share the process table). CPU, memory, and load are always collected — they cost only microseconds. The expensive parts are throttled: disk capacity (statfs, ~18ms on macOS) refreshes on its own slow cadence (`disk_storage_refresh_interval`, default 60s) while disk IO counters (~0.7ms) refresh every collect by default; CPU frequency refreshes every `cpu_frequency_refresh_interval` (default 30s); processes (~9ms) are the second-largest cost and can run on their own cadence (`process_refresh_interval`; the `serve` CLI defaults it to 10s). Every subsystem except CPU usage and memory has an independent cadence knob: `disk_io_refresh_interval` and `network_refresh_interval` (both default 0 = every collect, CLI `--disk-interval`/`--network-interval`) reuse the cached snapshot lists between refreshes, with rate diffs spanning the time since that subsystem's own previous refresh — there is no global `last_collect_time` anymore. Process selection ranks by CPU/memory first and only then materializes name/cmd strings for the top-N. While overall load is at or above `process_boost_cpu_cores` logical cores of work (default 1.0; scales with `logical_cores`, so 1 core ≈ 1.6% overall on 64U), the process list refreshes every collect regardless — CPU/memory are refreshed first inside `collect()` precisely so this decision can use the fresh value. `collect_process_disk_io` (default false) optionally adds per-process disk rates; `dedupe_disks` (default true) collapses APFS multi-mount volumes by device name.

The module's scope is **collecting local machine data only**: the "remote Collector (pulling snapshots from other machines via gRPC/HTTP)" extension mentioned in section 12 of the design doc has been explicitly rejected — do not design abstractions or features for it.

## Common Commands

```bash
make check          # CI aggregate: fmt-check + clippy(-D warnings) + feature matrix + tests; run before committing
make fmt            # Format (run this first when fmt-check fails)
make test           # All tests; equivalent to cargo test
make lint           # cargo clippy --all-targets -- -D warnings
make check-features # Verify each feature combination compiles independently
make run            # Foreground live view (fixed 2s sampling), q/Ctrl+C to exit
make json           # One-shot pretty-printed JSON snapshot (--pretty)

# Daemon mode (unix only) — driven entirely by <config-dir>/config.toml
cargo run -- serve            # Background daemon; detaches by default (--foreground to stay)
cargo run -- attach           # Replay history + live view; Ctrl+C detaches
cargo run -- stop             # Stop the daemon
cargo run -- -add interval 5000     # Persist a setting (see USAGE for keys)
cargo run -- -list                  # Show the config file

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
- `config`: adds `CollectorConfig::load_from_dir` (toml) — reads `<dir>/config.toml`'s `[collector]` section. All config Durations use `config::duration_serde`: they parse from `"500ms"`/`"2s"`/`"5m"`/`"1h"` strings or bare integers meaning milliseconds (`config::parse_duration`, also used by `-add`), and serialize back as the most compact unit string (`config::format_duration`, so 60000ms round-trips as `"1m"`);
- `cli`: config + runtime + bin-only tokio features (signal etc.); the bin is bound via `required-features = ["cli"]`.

The CLI resolves a config directory for every mode (default `~/.zstats`, `--config-dir` overrides; the flag is pre-scanned in main before any settings read). `config.toml` sections: `[collector]` (base for CLI flag overrides), `[daemon]` (`interval_ms`/`history_secs`/`detach`; `--foreground` forces foreground, and detached children always get `--foreground` appended to avoid re-detach loops), `[alerts]`. attach/stop skip config loading so a broken file cannot block stopping a daemon.

When adding a dependency, decide which layer it belongs to first. `make check-features` (included in `make check`) verifies each combination compiles independently — always run it after adding `cfg` gates. The integration tests are gated behind `runtime` as a whole.

## Conventions

- **All project documentation is in English** (this file, README, rustdoc, code comments), and so are CLI messages and logs. The only remaining Chinese content is the design doc (`collector-core-design.md`), which predates this rule.
- Every `.rs` file starts with the Apache 2.0 copyright header — do not omit it in new files.
- Error types are defined with snafu using struct variants (e.g. `SinkError::Remote { message }`). Beware selector-name clashes: same-named variants across enums in `error.rs` need `#[snafu(context(suffix(...)))]` (see the two `Timeout` variants). When adding variants that wrap an underlying failure, prefer a `source` field and snafu's context-selector idiom over stringifying.
- The CLI lives in `src/bin/zstats/` (`main.rs` hand-rolled arg parsing + `render.rs` text rendering + `daemon.rs` serve/attach/stop + `settings.rs` config file) and deliberately avoids clap; the lib contains no presentation logic. The CLI surface is deliberately tiny: bare `zstats` = foreground live view with EVERY cadence forced to a fixed 2s beat (`foreground_config`; throttles are for the daemon); `zstats serve` = background mode driven entirely by the config file (detaches by default, `--foreground` to stay); `--json`/`--pretty` = one-shot snapshot. There are NO per-setting runtime flags — all tuning goes through `-add <key> <value>` / `-remove <key>` / `-list`, which edit `<config-dir>/config.toml` via the key table in `settings::apply_add` (kebab-case keys → typed fields, validated). Keep `USAGE` and `settings::KNOWN_KEYS` in sync when adding keys.
- The daemon (unix only) serves a per-user Unix socket (`$TMPDIR/zstats-$USER.sock`) with a line protocol: client sends `ATTACH`/`STOP`; `ATTACH` gets `HISTORY <n>` + n JSON snapshots (ring buffer, retention by timestamp span) then a live JSON stream. Attach warms the rolling averages from history before rendering. SIGPIPE is reset to default in main so piped output dies silently.
- Daemon alerting (`src/bin/zstats/alerts.rs`, CLI layer by design — the lib stays alert-free): desktop notifications via osascript on macOS / notify-send elsewhere. Do NOT switch to notify-rust/notification-center APIs: identities masquerading as Terminal.app are silently dropped on this setup (show() still returns Ok, so no fallback is possible) — this was tested empirically; only the Script Editor identity (which osascript uses) displays. Both rules use 1-minute averages over a ≥50s-full window so only sustained behavior alerts: CPU ≥ `alert-cpu` (default 30%, single-core units — catches "quietly always busy" apps, not just runaways) and memory share of total ≥ `alert-mem` (default 25%; transient legitimate spikes are averaged away). Both keys accept `name=pct` per-process overrides (case-insensitive, 0 disables that process), e.g. `-add alert-cpu ghostty=100` for apps that are legitimately busy in the user's workflow. Everything lives in the `[alerts]` section of the config file; serve reads it at startup (malformed file = fail fast, not silent defaults) and hot-reloads it by polling its mtime every 30 collects (`RELOAD_CHECK_EVERY`, ~1min at the default interval); a file that fails to parse on reload keeps the previous settings and logs to stderr. Note `socket_path()`/`log_path()` live under `$TMPDIR` — tests must override both `HOME` and `TMPDIR` (short path: unix sockets cap at ~104 bytes) or they will collide with the user's real daemon. Interactive alert dialogs (buttons on notifications) were tried and removed by user choice — banners cannot carry buttons for unbundled CLIs and the `display dialog` alternative was judged too intrusive; don't reintroduce without being asked. Per-(pid, rule) re-alert cooldown via `alert-cooldown` (default 600s); state resets when the process dies.

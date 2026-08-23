# zstats

A system performance metrics collection library for Rust. One unified,
serializable data model; collection fully decoupled from output; collection
cost tunable down to tray-app levels.

## Install

Prebuilt binaries (macOS arm64/x86_64, static-musl Linux arm64/x86_64):

```bash
curl -fsSL https://raw.githubusercontent.com/vicanso/zstats/main/install.sh | sh
```

The script picks the asset for your platform from the latest release,
verifies its published SHA-256, and installs to `/usr/local/bin`. Override
with `ZSTATS_INSTALL_DIR=~/.local/bin` or pin a release with
`ZSTATS_VERSION=v0.1.0`.

### From source

Requires Rust **1.95+** (MSRV; driven by sysinfo 0.39).

```bash
# from crates.io (after the crate is published)
cargo install zstats

# from git
cargo install --git https://github.com/vicanso/zstats

# from a local checkout
cargo install --path .
```

This installs the `zstats` binary to `~/.cargo/bin` (keep that directory on
your `PATH`). Default features include `cli`; do not pass
`--no-default-features` unless you only want the library built without the
binary.

Upgrade later with `cargo install zstats --force` (or re-run the git/path
form with `--force`).

## Design

```
Collector (trait, sync) ──SystemSnapshot──▶ Scheduler ──concurrent dispatch──▶ MetricSink × N
```

- **`SystemSnapshot` is the single data contract**: host info (including
  uptime), CPU (overall, per-core, frequency on a slower cadence),
  memory/swap, load averages, hardware temperatures (slow cadence; platform
  sensors with garbage values filtered), and — each individually toggleable —
  disks (capacity + IO rates, kind, removable; optional per-device dedupe),
  network interfaces (rx/tx byte/packet/error rates), and processes (top-N
  by CPU and by memory, with command lines, virtual memory, run time, and
  optional per-process disk IO). Serde-serializable; timestamps are RFC 3339
  (jiff). Process lists are `Arc`-shared so cloning a snapshot is cheap.
- **Rate metrics are computed internally by diffing** cumulative counters
  between samples, so snapshots only ever expose per-second values
  (counter wraparound handled; the first sample reports `None`/0).
- **Sinks are isolated**: every sink's async `write` runs concurrently with
  its own timeout; one slow or failing sink never affects the others, and a
  failed collect just skips the round.

## Quick start

```rust
use std::sync::Arc;
use std::time::Duration;
use zstats::{CollectorConfig, LocalChannelSink, LocalCollector, MetricSink, Scheduler};

let collector = LocalCollector::new(CollectorConfig::default());
let (sink, mut rx) = LocalChannelSink::channel();

let mut scheduler = Scheduler::new(
    Box::new(collector),
    vec![Arc::new(sink) as Arc<dyn MetricSink>],
    Duration::from_secs(2),
);
scheduler.start().await?;

// The subscriber side reads the latest snapshot at any time
rx.changed().await?;
let snapshot = rx.borrow().clone();
```

`Scheduler::collect_once` triggers an immediate collect-and-dispatch for
app-initiated refresh. Without the `runtime` feature the library is fully
synchronous — drive `LocalCollector::collect()` from your own loop.

## Custom sinks

Implement `MetricSink` to add a backend (`async_trait` is re-exported):

```rust
use zstats::{async_trait, MetricSink, SinkError, SystemSnapshot};

struct MySink;

#[async_trait]
impl MetricSink for MySink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        // push to your storage / channel / endpoint
        Ok(())
    }

    fn name(&self) -> &str {
        "my_sink"
    }
}
```

Built-in sinks: `LocalChannelSink` (tokio `watch` channel for low-latency
in-process delivery) and `StdoutSink` (JSON lines, debugging).

## Collection cost controls

CPU, memory, and load are always collected — they cost microseconds. The
expensive subsystems are opt-out and throttled (`CollectorConfig`):

| Knob | Default | Effect |
|------|---------|--------|
| `collect_disks` / `collect_networks` / `collect_processes` / `collect_temperatures` | `true` | Disable a subsystem entirely (its snapshot field becomes `None`). `collect_temperatures` defaults to `false` on Windows, where the reading goes through WMI and initialises COM process-wide |
| `temperature_refresh_interval` | 15s | Hardware temps change slowly; readings are reused between refreshes |
| `disk_storage_refresh_interval` | 60s | Disk capacity (statfs, the most expensive call) refreshes on its own slow cadence; IO counters still refresh every collect, and a newly mounted volume is read as soon as it appears |
| `dedupe_disks` | `true` | Keep one entry per device name (shortest mount); collapses APFS synthetic mounts |
| `cpu_frequency_refresh_interval` | 30s | CPU frequency refreshes on its own cadence; usage still every collect |
| `process_refresh_interval` | 0 (every collect) | Throttle the process list; the last list is reused between refreshes |
| `collect_battery` | `true` | Charge, health, cycles, temperature and power draw of the main battery (`None` on machines without one) |
| `process_boost_cpu_cores` | auto: 30% of cores | While overall load is ≥ this many logical cores of work, the process list refreshes every collect. Unset = 30% of the machine's logical cores; explicit value pins the bar in core units; 0 = off |
| `collect_process_disk_io` | `false` | Per-process read/write byte rates (extra refresh cost when on) |
| `max_processes` | 50 | Kept processes; the budget is split between top-by-CPU and top-by-memory so idle memory hogs stay visible |
| `collect_timeout` | 2s | Enforced by the Scheduler around each collect |

As a reference (Apple Silicon, 2s interval, release build): full collection
runs at about 1.3% of one core, ~0.45% with a 10s process cadence, and
about 0% with only CPU/memory/load enabled.

## Cargo features

| Feature | Contents |
|---------|----------|
| _none_ | Data contract + synchronous `LocalCollector` — no tokio |
| `runtime` | `Scheduler` and the `sink` module (tokio, async-trait, tracing) |
| `config` | `CollectorConfig::load_from_dir` — read `<dir>/config.toml` |
| `client` | Typed client for the `zstats serve` daemon socket (unix only) |
| `frontend` | Frontend building blocks: alert rule engine (`alerts`), rolling per-process averages (`rolling`), daily metrics history (`records`), full config-file model (`settings`) — sync only, no tokio |
| `cli` (default) | The `zstats` command-line binary and its extra tokio features |

Library consumers who drive their own scheduling can depend on
`default-features = false`; add `runtime` for the async pipeline.

For a single-process frontend (e.g. a tray GUI) that embeds collection,
`Monitor` wires collection, the alert rules, rolling averages and history
persistence into one call — no tokio, no callbacks, the caller owns the
loop and therefore decides which thread touches the UI:

```rust
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut monitor = zstats::Monitor::new(zstats::settings::default_dir())?;
    loop {
        let tick = monitor.tick()?;
        for alert in &tick.alerts {
            println!("{}", alert.message); // deliver however you like
        }
        // tick.snapshot / tick.process_stats drive the views;
        // tick.records are already persisted
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
```

See `examples/embedded.rs` (`cargo run --example embedded`) for the full
skeleton, including reading history back for charts.

With the `client` feature, any frontend can attach to a running
`zstats serve` daemon and receive typed snapshots — buffered history
first, then live data — without knowing the wire format:

```rust
async fn follow() -> Result<(), zstats::ClientError> {
    let mut stream = zstats::client::attach().await?;
    while let Some(snapshot) = stream.next().await? {
        let live = stream.history_remaining() == 0;
        println!("{} (live: {live})", snapshot.timestamp);
    }
    Ok(())
}
```

## License

Apache-2.0

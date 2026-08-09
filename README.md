# zstats

A system performance metrics collection library for Rust. One unified,
serializable data model; collection fully decoupled from output; collection
cost tunable down to tray-app levels.

## Design

```
Collector (trait, sync) ──SystemSnapshot──▶ Scheduler ──concurrent dispatch──▶ MetricSink × N
```

- **`SystemSnapshot` is the single data contract**: host info (including
  uptime), CPU (overall, per-core, frequency on a slower cadence),
  memory/swap, load averages, and — each individually toggleable — disks
  (capacity + IO rates, kind, removable; optional per-device dedupe),
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
| `collect_disks` / `collect_networks` / `collect_processes` | `true` | Disable a subsystem entirely (its snapshot field becomes `None`) |
| `disk_storage_refresh_interval` | 60s | Disk capacity (statfs, the most expensive call) refreshes on its own slow cadence; IO counters still refresh every collect |
| `dedupe_disks` | `true` | Keep one entry per device name (shortest mount); collapses APFS synthetic mounts |
| `cpu_frequency_refresh_interval` | 30s | CPU frequency refreshes on its own cadence; usage still every collect |
| `process_refresh_interval` | 0 (every collect) | Throttle the process list; the last list is reused between refreshes |
| `process_boost_cpu_percent` | 15% | While overall CPU is at or above this, the process list refreshes every collect for precise attribution |
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
| `cli` (default) | The `zstats` command-line binary and its extra tokio features |

Library consumers who drive their own scheduling can depend on
`default-features = false`; add `runtime` for the async pipeline.

## License

Apache-2.0

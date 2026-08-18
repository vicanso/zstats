# Metrics Reference

Everything zstats can currently observe, written for whoever is designing a
frontend on top of it. The authoritative definitions live in
`src/snapshot.rs` (the data contract), `src/config.rs` (what is collected and
how often), and `src/alerts.rs` (the rule engine); this page collects them in
one place and adds the display semantics that are easy to get wrong.

---

## 1. Rules that shape any UI

Read these before laying out a single screen — each one has a visible
consequence.

| Rule | Consequence for the UI |
|---|---|
| `Option::None` means **"not collected"**, never "none exist" | A disabled subsystem must render as *off*, not as *zero* or *empty*. `disks: None` ≠ "no disks". |
| Rate fields need a previous sample to diff against | The **first tick after start has no rates** (`None`/0) for disk, network and per-process IO. Show a placeholder, not `0 B/s`. |
| Every subsystem has its own refresh cadence | A single "updated at" timestamp on the whole window is a lie. Disk capacity can be 60s old while CPU is live. Either group by cadence or annotate the slow tiles. |
| `processes` holds only the top N (default 50) | `total_processes` is the real count. A process table should say "50 of 612". |
| Groups are aggregated over the **full** table before truncation | A group's total legitimately exceeds the sum of its visible members. Do not compute group totals in the frontend from the visible rows. |
| Per-process CPU is in **single-core units** | 100% = one core saturated; an 8-core machine tops out near 800%. Progress bars must not be capped at 100. |

---

## 2. Snapshot map

```
SystemSnapshot
├── timestamp            UTC, RFC 3339
├── host                 HostInfo
├── cpu                  CpuSnapshot        (always collected)
│   ├── brand            Option<String>
│   ├── per_core_frequency_mhz[]
│   └── perf_levels[]    PerfLevelSnapshot  (macOS P/E clusters)
├── memory               MemorySnapshot     (always collected)
│   ├── used_percent / swap_used_percent    (derived ratios)
│   └── compressed_bytes / pressure_level   (macOS)
├── load                 LoadSnapshot       (always collected)
├── disks[]              Option<…>          toggleable
├── networks[]           Option<…>          toggleable
├── processes[]          Option<Arc<…>>     toggleable, top-N
├── process_groups[]     Option<Arc<…>>     needs processes
├── total_processes      Option<u32>
├── temperatures[]       Option<…>          toggleable
├── battery              Option<…>          toggleable / no battery
├── io_totals            IoTotalsSnapshot   pure sum of per-device rates
└── extras               reserved
```

---

## 3. Fields

### 3.1 Host — `host`

| Field | Type | Notes |
|---|---|---|
| `hostname` | String | |
| `os_name`, `os_version` | String | |
| `kernel_version` | Option\<String\> | |
| `arch` | String | e.g. `aarch64` |
| `uptime_secs` | u64 | Format as `3d 4h`, not seconds |
| `labels` | Map | User-defined, from config; good for a title bar subtitle |

### 3.2 CPU — `cpu` *(always collected)*

| Field | Unit / range | Availability | Display notes |
|---|---|---|---|
| `usage_percent` | 0–100 | always | Whole-machine, already normalised by core count |
| `per_core_usage[]` | 0–100 each | `per_core_cpu` | The classic core grid; length = `logical_cores` |
| `logical_cores` | count | always | |
| `physical_cores` | count | may be `None` | |
| `frequency_mhz` | MHz | may be `None` | **Static-ish**: refreshed every 30s by default, and on Apple Silicon it is a nominal value — do not chart it as a live curve. First non-zero core frequency when available |
| `per_core_frequency_mhz[]` | MHz each | when any frequency is known | Same length as `logical_cores` (0 = unknown for that core); empty when the platform reports no frequencies. Refreshed with `frequency_mhz` — **not** a live per-core power curve |
| `brand` | String | may be `None` | OS brand string, e.g. `Apple M3 Pro`. Static identity for a subtitle, not a live metric |
| `perf_levels[]` | see below | macOS heterogeneous CPUs only | `None` on Intel/Linux/Windows |

`PerfLevelSnapshot` — ordered **highest-performance first** (P before E):

| Field | Unit | Notes |
|---|---|---|
| `name` | String | OS-reported, e.g. `Performance` / `Efficiency` |
| `logical_cores` | count | |
| `usage_percent` | 0–100 | Average over that cluster's cores; a pure partition of `per_core_usage` |

> Design value: "180% CPU, all of it on E-cores" and "180% on P-cores" mean
> very different things for heat and battery. Two small bars beat one number.

### 3.3 Memory — `memory` *(always collected)*

| Field | Unit | Availability | Display notes |
|---|---|---|---|
| `total_bytes` | bytes | always | |
| `used_bytes` | bytes | always | On macOS this runs high **by design** — do not paint it red |
| `available_bytes` | bytes | always | The number that actually answers "am I out of memory?" |
| `swap_total_bytes`, `swap_used_bytes` | bytes | always | |
| `used_percent` | 0–100 | always | Pure ratio: `used_bytes / total_bytes * 100` (0 if total is 0). Prefer this over recomputing in every frontend |
| `swap_used_percent` | 0–100 | always | Pure ratio: `swap_used / swap_total * 100` (0 if no swap) |
| `compressed_bytes` | bytes | **macOS only** | Growth here precedes any visible trouble; the honest "pressure is building" signal |
| `pressure_level` | 1 / 2 / 4 | **macOS only** | `1` normal, `2` warning, `4` critical — the kernel's own verdict. Map to green/amber/red; it is the single best memory indicator on macOS |

### 3.4 Disks — `disks[]` *(toggleable)*

| Field | Unit | Notes |
|---|---|---|
| `name` | String | Device name |
| `mount_point` | String | The key users recognise; also the key for per-volume alert overrides |
| `file_system` | String | |
| `kind` | String | `SSD` / `HDD` / `Unknown` |
| `is_removable` | bool | Worth a distinct icon — a full USB stick is not an emergency |
| `total_bytes`, `available_bytes` | bytes | **Refreshed every 60s** by default (~18ms syscall); a volume that just appeared is read immediately so a fresh mount never shows 0 |
| `used_percent` | 0–100 | Pure ratio: `(total - available) / total * 100` (0 if total is 0). Same number the CLI paints on the capacity bar |
| `read_bytes_per_sec`, `write_bytes_per_sec` | B/s | `None` on the first sample |

`dedupe_disks` (default on) collapses APFS synthetic mounts, so `/` and
`/System/Volumes/Data` appear once rather than double-counting the volume.

### 3.5 Networks — `networks[]` *(toggleable)*

| Field | Unit | Notes |
|---|---|---|
| `interface` | String | |
| `received_bytes_per_sec`, `transmitted_bytes_per_sec` | B/s | |
| `received_packets_per_sec`, `transmitted_packets_per_sec` | pkt/s | Optional |
| `received_errors_per_sec`, `transmitted_errors_per_sec` | err/s | Optional; `None` on the first sample |

> A machine has many interfaces and most are idle. The CLI keeps a **fixed**
> row count (top N by traffic, idle slots filled with `en*`/`lo*`) so the
> layout below it never jumps. A GUI list has the same problem in a milder
> form — prefer a stable ordering to a "only active interfaces" filter.

### 3.6 Machine-wide IO totals — `io_totals` *(always present)*

Pure aggregation of the per-device lists **after** collection (and after
disk dedupe). **No extra system calls.** Fields are independent `Option`s.

| Field | Unit | Notes |
|---|---|---|
| `disk_read_bytes_per_sec` | B/s | Sum of `disks[].read_bytes_per_sec`. `None` when disks are disabled or every disk still has `None` rates (first sample) |
| `disk_write_bytes_per_sec` | B/s | Same for writes |
| `network_received_bytes_per_sec` | B/s | Sum of all interfaces when `networks` is collected; `Some(0)` is valid on a quiet first sample. `None` only when network collection is off |
| `network_transmitted_bytes_per_sec` | B/s | Same for transmit |

> Prefer `io_totals` for an overview "how busy is storage / the wire" tile.
> Do not re-sum the tables in the frontend unless you intentionally filter
> interfaces (e.g. exclude `lo*`) — the library sums **every** collected
> device after its own dedupe rules.

### 3.7 Processes — `processes[]` *(toggleable, top-N)*

Selected by ranking on CPU **and** memory (the budget is split between both
rankings), returned sorted by CPU descending.

| Field | Unit | Notes |
|---|---|---|
| `pid` | u32 | Stable key for a row — and the key alerts link back on |
| `name` | String | |
| `cmd` | String | Full command line; a detail-panel field, too long for a table cell |
| `cpu_usage_percent` | single-core % | May exceed 100 |
| `cpu_time_ms` | single-core ms | **A counter, not a rate.** Lifetime CPU consumed. Diff two samples for the amount burned in between — the only way a steady low-percentage process becomes visible |
| `memory_bytes` | bytes | Resident |
| `phys_footprint_bytes` | bytes | **What the memory rules measure**, and the better number for a memory column: resident size cannot see compressed or paged-out pages, so a process under pressure reads as shrinking exactly when it squeezes hardest. macOS `phys_footprint`, Windows `PrivateUsage`, Linux `RssAnon + VmSwap`; `None` elsewhere, or where the kernel refused (EPERM on another user's process) |
| `virtual_memory_bytes` | bytes | Rarely useful; hide by default. On Windows `sysinfo` reports `PrivateUsage` here, i.e. the same number as `phys_footprint_bytes` |
| `run_time_secs` | seconds | |
| `parent_pid` | Option\<u32\> | Lets a UI build the tree itself if it wants |
| `user_id` | Option\<String\> | Text on purpose: numeric uid on unix, SID on Windows |
| `status` | String | `Run`, `Sleep`, … |
| `read_bytes_per_sec`, `write_bytes_per_sec` | B/s | Only when `process-disk-io` is enabled |

### 3.8 Applications — `process_groups[]`

One entry per process tree, rooted at a direct child of init/launchd. This is
what makes a browser with 37 helpers legible as one row.

| Field | Unit | Notes |
|---|---|---|
| `root_pid` | u32 | Row key; also the key app-level alerts use |
| `name` | String | Root process name |
| `process_count` | u32 | "Chrome — 37 processes" |
| `cpu_usage_percent` | single-core % | Sum over the whole tree; hundreds of percent is normal |
| `memory_bytes` | bytes | Sum over the whole tree |
| `phys_footprint_bytes` | bytes | Sum over the whole tree, a member's RSS standing in where the kernel refused a footprint; `None` off macOS. **This is what the app memory rule measures** — RSS falls as the kernel compresses, exactly when a group is squeezing the machine |
| `read_bytes_per_sec`, `write_bytes_per_sec` | B/s | Sum; only with `process-disk-io` |

> **macOS quirk worth designing around:** every terminal session's
> descendants group under a `login` root, not under the terminal app. So a
> build shows up as `login` burning 600%. Consider showing `process_count`
> and letting the row expand rather than trusting the name alone.

### 3.9 Load — `load`

`load1`, `load5`, `load15` (f64). Divide by `cpu.logical_cores` for a
meaningful "how loaded is this machine" ratio. On Windows these are emulated
by sysinfo from CPU samples.

### 3.10 Temperatures — `temperatures[]` *(toggleable)*

| Field | Unit | Notes |
|---|---|---|
| `label` | String | macOS returns raw firmware strings like `PMU tdie8` — not user-facing text |
| `celsius` | °C | Implausible readings are already filtered out by the collector |
| `max_celsius`, `critical_celsius` | °C | Optional |

Sorted hottest-first. An **empty vec** means "collection ran, nothing
readable" (common on Windows/WMI) — distinct from `None` = disabled.

### 3.11 Battery — `battery` *(toggleable)*

| Field | Unit | Notes |
|---|---|---|
| `state` | String | `Charging` / `Discharging` / `Full` / `Empty` / `Unknown` — derive "on AC" from this |
| `charge_percent` | 0–100 | Duplicates the menu bar; low information |
| `health_percent` | 0–100 | Wear; moves over months — a reference value, not a time series |
| `cycle_count` | count | Same |
| `temperature_celsius` | °C | Separate from the CPU sensors |
| `power_watts` | W | **The field that justifies this subsystem** — live draw, surfaced nowhere in the macOS UI. Pairs with CPU: "150% CPU while drawing 22 W" |
| `time_to_full_secs`, `time_to_empty_secs` | seconds | Optional estimates |

`None` on desktops and VMs. Only the first battery is reported.

**Deliberate:** there is no battery alert rule and there will not be one — the
OS already warns about low battery, and a second warning is pure noise.

---

## 4. Derived layer (`frontend` feature)

### 4.1 Rolling averages — `rolling::ProcessStats`

Per-pid values over a 60s window: `cpu_avg`, `cpu_time_delta_ms`,
`memory_avg_bytes`, `span`, `samples`.

Rank the process table by these, not by the instantaneous values — raw
per-tick CPU reshuffles rows every refresh and makes the table unreadable.

`cpu_time_delta_ms` is the odd one out: an **amount**, not a rate. It answers
"what did this cost over `span`" rather than "how busy is it", which is the
only framing where a process at a steady 8% is visible at all. A "top
spenders" view sorts by it; a live process table still sorts by `cpu_avg`.

### 4.2 Alerts — `alerts::AlertEvent`

Pure data, no baked wording, so a GUI can render alerts however it likes
(and localise them). `summary()` renders the default English line on demand.

```
AlertEvent
├── subject       AlertSubject   who
├── detail        AlertDetail    what, with every number in its own unit
└── repeat_after  Option<Duration>   Some(elapsed) = the 30-min follow-up
```

`AlertSubject` — the link back into the UI:

| Variant | Payload | Frontend action |
|---|---|---|
| `Process` | `pid`, `name` | Select that row in the process table |
| `App` | `root_pid`, `name`, `process_count` | Select that row in the app table |
| `Volume` | `mount_point` | Select that volume |
| `System` | — | Machine-level banner |

`AlertDetail` — tagged by `measure`:

| Variant | Fields |
|---|---|
| `cpu` | `avg_percent`, `threshold_percent`, `window`, `runaway` |
| `memory` | `avg_bytes`, `share_percent`, `threshold_bytes`, `threshold_percent`, `window` |
| `disk` | `used_percent`, `threshold_percent`, `available_bytes`, `total_bytes` |
| `pressure` | `level`, `sustained`, `swap_used_bytes`, `swap_total_bytes`, `compressed_bytes`, `top_consumers` |

Derived accessors (not stored, so they cannot disagree with the data):

- `kind()` → `Cpu` \| `Memory` \| `AppCpu` \| `AppMemory` \| `Disk` \| `Pressure`
- `severity()` → `Warning` \| `Critical` (runaway CPU, or pressure level 4)
- `summary()` / `Display` → the default English one-liner

Behaviour the UI should mirror: alerts are **episode-based**. One notification
when the condition crosses, exactly one follow-up after 30 minutes if it is
still true (`repeat_after.is_some()`), then silence until the value falls back
and re-arms. An alert list should therefore group by episode, not append a row
per evaluation.

`pressure` is the exception, in both directions, because it is a machine state
rather than a culprit — nothing to kill, and it can legitimately hold all day:

- its reminders **repeat indefinitely on a backoff** — 30m, 1h, 2h, then every
  4h — instead of stopping after one, so a UI must be ready for many
  `repeat_after` events in one episode;
- its episode **ends only after 5 minutes of continuous normal**, so brief dips
  back to level 1 must not clear the banner (the kernel level is a noisy step
  function; treating one normal sample as recovery is what made this alert
  repeat in the first place);
- `sustained` counts from when the level first went above normal, while
  `repeat_after` counts from the episode's first notification.

It also carries the **attribution**: `top_consumers` is up to 3
`MemoryConsumer { pid, name, bytes, share_percent, process_count }`, biggest
first, taken from the snapshot at the moment the alert fired — whole
applications where `process_groups` is collected (`process_count` > 1 and `pid`
is the group root, so a UI can select the app row), individual processes
otherwise. This is the actionable half of the alert: the level says the machine
is in trouble, these say what to close. It is not smoothed (memory is a state,
not a rate) and not configurable — everything above 5% of RAM, capped at 3.

Per-process memory alerts (`memory` on a `Process` subject) answer the earlier,
narrower question — "is any ONE process enormous" — and their bar is the **lower
of 25% of total RAM and 4 GiB**. Neither half works alone: a percentage is
unreachable on a large machine (25% of 64 GiB) and trivially reached on a small
one, so the percentage protects small machines and the ceiling protects large
ones. `threshold_bytes` is the bar that actually fired. A UI should not present
this as the same rule as pressure — pressure means "memory is short now", this
means "this one thing is enormous". `avg_bytes` is the physical footprint where
macOS provides one, not the resident size, and the two differ by more than
rounding (measured: 3.02 GiB of footprint against 0.17 GiB of RSS for one
language server).

### 4.3 History — `records::MetricRecord`

One JSON line per qualifying process per minute in
`<config-dir>/data/YYYY-MM-DD.jsonl` (local date), 30-day retention swept
automatically on append.

| Field | Notes |
|---|---|
| `timestamp` | UTC |
| `pid`, `name` | |
| `cpu_avg_percent` | 1-minute average |
| `memory_avg_bytes` | 1-minute average resident size |
| `memory_share_percent` | Share of total RAM |
| `memory_footprint_bytes` | 1-minute average physical footprint (macOS, absent where unreadable and in files written before it existed) — the figure the rules measure |
| `cpu_time_ms` | **Lifetime** CPU counter at this sample, absolute. Subtract a pid's first record of the day from its last for exactly what it consumed; a decrease means pid reuse |

Two criteria put a process in the file:

1. its 1-minute average exceeds the **base** alert thresholds — per-process
   overrides silence the notification but the data point is still written
   (recording is history, alerting is interruption);
2. it is one of the 5 biggest **CPU-time** spenders of that minute, whatever
   its percentages. This is the only criterion that can see a process no
   threshold will ever catch, and it is deliberately recording-only: a
   low-bar/long-window *alert* would fire on every legitimate resident daemon.

So a "what burned the CPU today" view is a `read_range` over the day grouped
by pid, ranked by `max(cpu_time_ms) - min(cpu_time_ms)` — not by any average.

`records::read_range(dir, from, to)` is the read API to chart from. Note the
machine sleeps: expect gaps, and do not interpolate across them.

---

## 5. Settings surface

All of it lives in `<config-dir>/config.toml` (default `~/.zstats`, shared by
every frontend). The library API is `settings::{load, save, apply_add,
apply_remove}` — a preferences panel should go through `apply_add`, since it
carries the validation.

**Collection toggles** — `collect-processes`, `collect-disks`,
`collect-networks`, `collect-temperatures`, `collect-battery`,
`process-groups`, `process-disk-io`, `per-core-cpu`, `dedupe-disks`,
`max-processes` (50), `process-boost` (2.0 cores).

**Cadences** — `interval` (daemon sampling), `process-interval`,
`disk-interval`, `disk-storage-interval` (60s), `network-interval`,
`temp-interval` (15s), `cpu-freq-interval` (30s), `battery-interval` (30s),
`history` (daemon ring buffer span). Durations accept `500ms` / `2s` / `5m` /
`1h` or a bare integer in milliseconds.

**Alert thresholds** — `alert-cpu` (30 single-core %), `alert-mem` (25% of
total) with `alert-mem-bytes` (4GiB ceiling, whichever is lower),
`alert-app-cpu` (200%), `alert-app-mem` (40% of total) with
`alert-app-mem-bytes` (8GiB ceiling, same lower-of-two shape),
`alert-disk` (90% per volume),
`alert-pressure` (`off`/`warning`/`critical`), `alert-cooldown` (600s),
`alert-template` (the builtin per-app exemption list).

The first four accept per-name overrides (`ghostty=100`, `0` disables that
name); `alert-disk` accepts per-mount overrides.

An override key may lead and/or end with `*` to claim a family of names —
`rust-analyzer*`, `*Helper (Renderer)`. This matters because a process name is
not a stable identifier: tool managers stamp the version into the binary
itself (Zed's rust-analyzer is `rust-analyzer-2026-08-10.1` and gets renamed on
every update), so an exact key silently stops matching. A `*` anywhere other
than the ends is rejected rather than treated as a literal.

Precedence when several keys match, highest first:

1. user exact name
2. user pattern — longer literal wins
3. builtin template exact name
4. builtin template pattern
5. the rule's base value

A settings UI should surface which rule actually applied to a process; the
merged view is `alerts::ActiveThresholds::from_config`.

The template layer itself is a TOML file, not a table in the source —
`templates/alerts.toml`, compiled in via `include_str!` and parsed into
`alerts::Template` (`version`, `[cpu]`, `[mem]`, `[app_cpu]`, `[app_mem]`). A copy at
`<config-dir>/template.toml` **replaces** it wholesale, so the table can be
refreshed on a schedule (`curl -o`) without a new binary; the daemon reloads it
within about a minute of an mtime change. A missing file means "use the
builtin", but a malformed or wrong-version one is an error rather than a silent
fallback. Load it with `settings::load_template(dir)` and pass it to
`ActiveThresholds::from_config_with_template` — a frontend that embeds
collection should do this rather than calling `from_config`, or it will ignore
the user's template.

---

## 6. Suggested screen decomposition

A mapping from the data above to views, as a starting point.

| View | Primary data | Notes |
|---|---|---|
| **Tray / menu bar** | `cpu.usage_percent`, `memory.pressure_level`, active alert count | One glance; pressure level is the better memory signal than used% |
| **Overview** | CPU (+ `brand`, `perf_levels`), memory (`used_percent` + compressed, pressure), load, uptime, `io_totals`, `battery.power_watts` | The tiles that answer "is anything wrong right now". Use `io_totals` for a single disk/net throughput strip |
| **Processes** | `processes[]` ranked by `rolling::ProcessStats` | Header shows "N of `total_processes`" |
| **Applications** | `process_groups[]` | The row users actually think in; expand to members via `parent_pid` |
| **Storage** | `disks[]` (+ `used_percent`, `io_totals.disk_*`) | Capacity is 60s-stale by design; IO rates are live; prefer field `used_percent` over recomputing |
| **Network** | `networks[]` (+ `io_totals.network_*`) | Fixed row count, stable ordering; machine total is already in `io_totals` |
| **Sensors & power** | `temperatures[]`, `battery` | Both platform-flaky — design for "unavailable" as a normal state |
| **Alerts** | `AlertEvent` stream, grouped by episode | Colour by `severity()`, icon by `kind()`, click-through via `subject` |
| **History** | `records::read_range` | Per-process daily trends, 30-day window, gaps where the machine slept |
| **Settings** | `settings::apply_add` | Mirrors §5; validation messages come back from the library |

---

## 7. Not available

So the design does not promise what the backend cannot deliver. Each was
evaluated and rejected — the library is `#![forbid(unsafe_code)]`, and these
all need private APIs or hand-written FFI on macOS.

| Metric | Why not |
|---|---|
| **GPU utilisation** | Needs the private IOReport API or shelling out to `powermetrics` (root). The one genuinely valuable gap left. |
| Disk IOPS / latency / util% (busy time) | sysinfo exposes **byte** counters only (`io_totals` and per-disk B/s are those). macOS busy-time/util needs IOKit (`IOBlockStorageDriver`). Capacity `used_percent` **is** available and is not the same thing |
| Swap in/out rates, page in/out rates | Only via Mach `host_statistics64` (unsafe FFI). The `vm.compressor.segment.*` sysctls look like counters but are gauges — deriving rates from them would print wrong numbers. `swap_used_percent` is a level, not a rate |
| Per-process network IO | Needs private APIs on macOS; without attribution a network alert cannot name a culprit, so it would not be actionable |
| Thread counts | sysinfo's `tasks()` is documented Linux-only and returns nothing on macOS |
| Per-cluster frequency / power | Root-only `powermetrics` or private IOReport. `per_core_frequency_mhz` / `frequency_mhz` are OS-reported nominal values only |

### Platform coverage

macOS is the reference platform. Linux and Windows compile and run, with
gaps: `perf_levels`, `compressed_bytes` and `pressure_level` are macOS-only
(the Linux analogues — sysfs CPU capacity, PSI — are known future work);
Windows load averages are emulated and temperatures are usually empty (and
so `collect_temperatures` defaults to **false** there — `sysinfo` reaches
them through WMI, which initialises COM process-wide on the collector
thread).

Rather than guess from its own build, a frontend should read
`capabilities` off the snapshot — `memory_footprint`, `memory_pressure`,
`cpu_perf_levels`, each a property of the build that produced the
snapshot. It answers "this platform has no such concept" and nothing
else: a `None` that means "the kernel refused for this process" or "not
sampled yet" still looks the same. The alert engine exposes the matching
question for rules: `ActiveThresholds::supports(AlertKind::Pressure)` is
false off macOS, and `any_enabled()` will not claim alerting is live when
the pressure rule is the only one left on.

The builtin alert template is per platform — `templates/alerts.toml`
(macOS), `alerts-linux.toml`, `alerts-windows.toml` — because process
names are not portable: `Google Chrome Helper (Renderer)` is
`chrome.exe` on Windows and `Isolated Web Co` on Linux, where the kernel
truncates every name to 15 bytes. `<config-dir>/template.toml` replaces
whichever one was compiled in.

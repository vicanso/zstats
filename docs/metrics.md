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
│   ├── compressed_bytes / pressure_level   (macOS)
│   └── swap_ins_per_sec / swap_outs_per_sec / swap_thrashing /
│       kernel_available_percent            (macOS)
├── load                 LoadSnapshot       (always collected)
├── disks[]              Option<…>          toggleable (per volume)
├── drives[]             Option<…>          toggleable (per physical disk; macOS)
├── networks[]           Option<…>          toggleable
├── processes[]          Option<Arc<…>>     toggleable, top-N
├── process_groups[]     Option<Arc<…>>     needs processes
├── total_processes      Option<u32>
├── temperatures[]       Option<…>          toggleable
├── battery              Option<…>          toggleable / no battery
├── gpus[]               Option<…>          toggleable (macOS)
├── io_totals            IoTotalsSnapshot   pure sum of per-device rates
├── capabilities         Capabilities       what this BUILD can measure
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
| `swap_ins_per_sec`, `swap_outs_per_sec` | segments/s | **macOS only** | Compressor segments moved per second — the kernel's unit, **not bytes**, so show them as activity ("out 120/s") and never convert. `pressure_level` says the machine is tight; these say how hard it is working to stay there. `None` on the first sample |
| `swap_thrashing` | bool | **macOS only** | The kernel's own thrash detector advanced since the last sample. A verdict, like `pressure_level`: paint it red, do not threshold it |
| `kernel_available_percent` | 0–100 | **macOS only** | `kern.memorystatus_level`: the available-memory figure the kernel's pressure verdict is derived from. Lower than `available_bytes / total` because it excludes what the kernel will not reclaim; show it beside the verdict, not beside `used_percent` |

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
| `disk_read_ops_per_sec`, `disk_write_ops_per_sec` | ops/s | Sum of `drives[]` operation rates (macOS). Operations only — the drives' **bytes** are deliberately not added to the two byte totals above, which already come from the volume list; adding both would count every APFS volume twice. `None` when drives are off or still without a baseline |

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
| `name` | String | The executable's own name — **and the only name alert thresholds, template entries and overrides match on** |
| `display_name` | Option\<String\> | The application `name` belongs to, when `name` does not say it: the `.app` bundle whose own executable this is (`<bundle>/Contents/MacOS/<exe>`). Show `display_name ?? name`. macOS only, `None` when the executable is not a bundle's own — not in one at all, or merely shipped inside one — or the bundle merely repeats `name`, so a value always carries new information. See the note below |
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
| `name` | String | Root process name; the key app-level thresholds match on |
| `display_name` | Option\<String\> | As above — this is the one that turns `Electron — 22 processes` into `CodeBuddy CN — 22 processes` |
| `process_count` | u32 | "Chrome — 37 processes" |
| `cpu_usage_percent` | single-core % | Sum over the whole tree; hundreds of percent is normal |
| `memory_bytes` | bytes | Sum over the whole tree |
| `phys_footprint_bytes` | bytes | Sum over the whole tree, a member's RSS standing in where the kernel refused a footprint; `None` off macOS. **This is what the app memory rule measures** — RSS falls as the kernel compresses, exactly when a group is squeezing the machine |
| `read_bytes_per_sec`, `write_bytes_per_sec` | B/s | Sum; only with `process-disk-io` |

> **Name your rows `display_name ?? name`, but never key anything on it.**
> A process's name is its executable's, and the stock Electron binary is
> called `Electron` — so every app that shipped without renaming it reports
> that single name to every kernel interface there is
> (`/Applications/CodeBuddy CN.app/Contents/MacOS/Electron`). A card titled
> "Electron — 22 processes" names nothing a person can act on, and two such
> apps are indistinguishable. `display_name` is the enclosing `.app` bundle,
> which is what Finder and Activity Monitor show. Measured on one live
> machine: 3 of 50 groups resolve — `MacPacketTunnel` → Shadowrocket, `node`
> → 企业微信, `zed` → Zed — because most apps do rename their binary, so this
> is a low-noise fallback rather than a second naming scheme.
>
> It is presentation only. `name` stays the identity thresholds match on, so
> a settings UI offering "raise the bar for this app" must write `name`, and
> a rule that fired has to stay explainable by `name`. The NEAREST bundle
> wins: Chromium nests helper bundles inside the browser's own, and
> resolving a renderer up to `Google Chrome` would erase the distinction the
> per-process template is built on.
>
> Only a bundle's OWN executable resolves — the path has to be
> `<bundle>/Contents/MacOS/<exe>`. Being merely inside a bundle does not
> count: Xcode ships a whole toolchain under `Xcode.app/Contents/Developer/`
> (`make`, `clang`, `ld`, `git`, …), and matching any `.app` ancestor named
> every one of them "Xcode" — a `make` in a terminal reported as Xcode while
> Xcode was not running. Apps' bundled CLIs (`Docker.app/Contents/Resources/
> bin/docker`) are the same case. Activity Monitor shows those by their own
> names; so does this.

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

### 3.12 GPUs — `gpus[]` *(toggleable, macOS)*

Read from the IORegistry's accelerator driver (`PerformanceStatistics`)
through the stock `ioreg` tool — no root, no private API, ~13ms per read on
its own cadence (`gpu_refresh_interval`, default 10s). One entry per GPU.

| Field | Unit | Notes |
|---|---|---|
| `name` | String | Marketing name where published (`Apple M4 Pro`), else the driver's registry name |
| `cores` | count | GPU cores, where published |
| `utilization_percent` | 0–100 | Device busy. An **instantaneous gauge**, not an average over the cadence: "12% right now". Chart it as samples, not as a smoothed rate |
| `renderer_utilization_percent`, `tiler_utilization_percent` | 0–100 | The two halves of the pipeline, where published |
| `memory_in_use_bytes`, `memory_allocated_bytes` | bytes | System memory the GPU holds. Unified memory on Apple Silicon — this competes with `memory.used_bytes`, and is where an Electron app's GPU process actually lives |

`None` when disabled, off macOS (`capabilities.gpu` is false), or when the
registry read failed or timed out this round — a stale "12% busy" presented
as current would be a lie, so nothing is cached across a failure. **No alert
rule**: the registry has no per-process split, so a GPU alert could never
name a culprit.

### 3.13 Drives — `drives[]` *(toggleable, macOS)*

Per **physical disk** (`disk0`), not per volume — `disks[]` is per mount
point, and APFS puts several volumes on one drive, so the two lists do not
line up and are deliberately not joined. This is the list that carries
operations, service time and queue depth; the volume layer exposes bytes
only. Source: the `IOBlockStorageDriver` statistics `iostat` reads, via
`ioreg`, on `drive_refresh_interval` (default 10s; rates diff across it, so
a longer cadence smooths).

| Field | Unit | Notes |
|---|---|---|
| `name` | String | BSD name of the whole disk (`disk0`) |
| `model` | String | `APPLE SSD AP0512Z`, `Apple Disk Image` |
| `size_bytes` | bytes | |
| `is_removable` | bool | |
| `read_ops_per_sec`, `write_ops_per_sec` | ops/s | IOPS. A drive can be saturated by small random IO while its byte rate looks idle — show these next to the byte rates, not instead of them |
| `read_bytes_per_sec`, `write_bytes_per_sec` | B/s | Same quantity `disks[]` has, at drive granularity |
| `read_latency_ms`, `write_latency_ms` | ms | Average time from the driver receiving an operation to its completion, over the window. Includes device queueing, so it climbs under load before the device itself slows. `None` in a window with no operations of that kind — an average over zero operations claims nothing |
| `queue_depth` | ops | Mean operations in flight over the window (summed service time ÷ wall clock; Linux `iostat`'s `aqu-sz`). Above 1 = operations overlapped. This is the closest the driver comes to a utilisation figure — it publishes no "device busy time", so a true `%util` that stops at 100 cannot be derived, and a UI must not draw this as one |
| `read_errors`, `write_errors` | count | **Since boot, cumulative.** Rare events where the total is the actionable number; any non-zero deserves a red mark |

All rates are `None` on the first sample. A counter that went backwards (the
drive was detached and re-attached) yields `None` for that window rather than
a diff against the previous drive's life. `None` for the whole list when
disabled or off macOS (`capabilities.drive_io`).

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
| `Process` | `pid`, `name`, `display_name` | Select that row in the process table |
| `App` | `root_pid`, `name`, `display_name`, `process_count` | Select that row in the app table |
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
`MemoryConsumer { pid, name, display_name, bytes, share_percent, process_count }`, biggest
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
| `pid`, `name` | `name` is what the rule matched on |
| `display_name` | The application `name` belongs to, where the executable does not say it (§3.7). Present so a history line can be read against the notification it explains — a stock-packaged Electron app notifies as `CodeBuddy CN` but records under `Electron`. Chart `display_name ?? name`, the same as a live row |
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
`collect-networks`, `collect-temperatures`, `collect-battery`, `collect-gpu`,
`collect-drives`, `process-groups`, `process-disk-io`, `per-core-cpu`,
`dedupe-disks`, `max-processes` (50), `process-boost` (default auto: 30% of
the machine's logical cores; an explicit value is a bar in core units, 0 =
off).

**Cadences** — `interval` (daemon sampling), `process-interval`,
`disk-interval`, `disk-storage-interval` (60s), `network-interval`,
`temp-interval` (15s), `cpu-freq-interval` (30s), `battery-interval` (30s),
`gpu-interval` (10s), `drive-interval` (10s), `history` (daemon ring buffer
span). Durations accept `500ms` / `2s` / `5m` / `1h` or a bare integer in
milliseconds.

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
`templates/alerts-<platform>.toml`, compiled in via `include_str!` and parsed into
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
| **Overview** | CPU (+ `brand`, `perf_levels`), `gpus[].utilization_percent`, memory (`used_percent` + compressed, pressure, `swap_*_per_sec`, `swap_thrashing`), load, uptime, `io_totals`, `battery.power_watts` | The tiles that answer "is anything wrong right now". Use `io_totals` for a single disk/net throughput strip, with `disk_*_ops_per_sec` beside the bytes |
| **Processes** | `processes[]` ranked by `rolling::ProcessStats` | Header shows "N of `total_processes`" |
| **Applications** | `process_groups[]` | The row users actually think in; expand to members via `parent_pid` |
| **Storage** | `disks[]` (+ `used_percent`, `io_totals.disk_*`), `drives[]` | Capacity is 60s-stale by design; IO rates are live; prefer field `used_percent` over recomputing. Volumes answer "is it full", drives answer "is it slow" (`queue_depth`, `*_latency_ms`, `*_errors`) — two panels, not one joined table |
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
| Disk **util%** (device busy time) | `IOBlockStorageDriver` publishes summed service time but no "device had an operation in flight" clock, so `drives[].queue_depth` (mean in-flight operations) is the honest figure; a `%util` that stops at 100 cannot be derived from it. Its `Latency Time` counters are also unpopulated on Apple SSDs, which is why `*_latency_ms` derives from total time ÷ operations |
| Page in/out rates (file-backed paging) | Only via Mach `host_statistics64` (unsafe FFI). The **compressor swapper's** counters are a different thing and *are* exposed (`swap_ins_per_sec` / `swap_outs_per_sec`); the `vm.compressor.segment.*` sysctls that look like counters are gauges and stay unused |
| Per-process GPU time | The accelerator's `PerformanceStatistics` is device-wide; per-client accounting is private IOReport. So `gpus[]` is a metric, never an alert |
| Per-process network IO | Needs private APIs on macOS; without attribution a network alert cannot name a culprit, so it would not be actionable |
| Thread counts | sysinfo's `tasks()` is documented Linux-only and returns nothing on macOS |
| Per-cluster frequency / power | Root-only `powermetrics` or private IOReport. `per_core_frequency_mhz` / `frequency_mhz` are OS-reported nominal values only |

Three former entries in this table — GPU utilisation, disk IOPS / service
time, swap in/out rates — were rejected on the belief that they needed IOKit
or Mach FFI. Probing the machine showed otherwise: the IORegistry is readable
without root through the stock `ioreg` tool (XML plist out, ~12ms per query,
the library's one child process, killed at a 1s deadline), and the swapper's
`_total` counters are ordinary sysctls. They are now §3.12, §3.13 and §3.3.

### Platform coverage

macOS is the reference platform. Linux and Windows compile and run, with
gaps: `perf_levels`, `compressed_bytes`, `pressure_level`, the swap activity
fields, `gpus[]` and `drives[]` are macOS-only (the Linux analogues — sysfs
CPU capacity, PSI, `/sys/class/drm/*/gpu_busy_percent`, `/proc/diskstats` —
are known future work); Windows load averages are emulated and temperatures
are usually empty (and so `collect_temperatures` defaults to **false** there
— `sysinfo` reaches them through WMI, which initialises COM process-wide on
the collector thread).

Rather than guess from its own build, a frontend should read
`capabilities` off the snapshot — `memory_footprint`, `memory_pressure`,
`cpu_perf_levels`, `gpu`, `drive_io`, `swap_rates`, each a property of the
build that produced the snapshot. It answers "this platform has no such concept" and nothing
else: a `None` that means "the kernel refused for this process" or "not
sampled yet" still looks the same. The alert engine exposes the matching
question for rules: `ActiveThresholds::supports(AlertKind::Pressure)` is
false off macOS, and `any_enabled()` will not claim alerting is live when
the pressure rule is the only one left on.

The builtin alert template is per platform — `templates/alerts-macos.toml`,
`alerts-linux.toml`, `alerts-windows.toml` — because process
names are not portable: `Google Chrome Helper (Renderer)` is
`chrome.exe` on Windows and `Isolated Web Co` on Linux, where the kernel
truncates every name to 15 bytes. `<config-dir>/template.toml` replaces
whichever one was compiled in.

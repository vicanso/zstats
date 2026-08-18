# Cross-platform status

What had to change here before a Windows or Linux **frontend** could be
believed, and what still would. Items 1–7 below were an audit compiled while
assessing whether [zstats.app](https://github.com/vicanso/zstats.app) could
ship beyond macOS; the verdict there was Windows-maybe, Linux-no, but for
frontend reasons (the tray emits no click events on Linux and Wayland cannot
position a window). This page is only about the data.

**All seven are now addressed in the collector.** What that means precisely:
each one compiles for `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`
under `make check-targets` with warnings-as-errors, and every piece of logic
that could be tested without the platform is tested on macOS — the `/proc`
parser, the recycled-pid guard, all three template files, the capability and
rule-support reporting. **None of it has been run on Windows or Linux.** The
platform-specific I/O is deliberately thin for exactly that reason (one
`sysinfo` field on Windows, one `read_to_string` on Linux, no new FFI, the
`forbid(unsafe_code)` rule intact), but a first real run should still be
treated as the moment these claims get tested rather than the moment they get
confirmed.

CLAUDE.md's platform policy carries the summary; this page carries the
reasoning and what is left.

---

## The shape of the problem

The seam in `src/collector/local.rs` is now seven cfg items rather than the
five this audit started with. Five are still macOS-only — `phys_footprint`,
`sysctl_u64`, `sysctl_string`, `detect_perf_levels`, `memory_pressure` — and
two came out of item 4: `process_footprint`, which is the only three-way
branch in the crate, and `parse_proc_status_footprint`, gated
`cfg(any(target_os = "linux", test))` so the reference platform still tests
it. Three cfg points live outside that file: the template selection
(`alerts.rs`), the Windows temperature default (`config.rs`) and
`Capabilities::current` (`snapshot.rs`). Everything else rides `sysinfo` and
`starship-battery`, both with real Windows and Linux backends.

Worth stating plainly because frontends build on it: **`cpu_time_ms` is
cross-platform** (Windows `GetProcessTimes`, Linux `/proc/[pid]/stat`), so
anything integrating CPU time over a window — a sustained-load watcher, a
daily ranking — survives a port unchanged.

So the work was never "port the collector". `Option`s reading `None` are the
*designed* degradation and frontends already handle it. The backlog was about
the places where a non-macOS build produced **a confident wrong answer
instead of no answer**, plus the places where a rule advertised itself and
then could not fire.

---

## 1. `dedupe_disks` collapsed unlabelled Windows volumes — fixed

`dedupe_disks_by_name` keys on `disk.name`. That is a device identity on
macOS and Linux; on Windows `sysinfo` fills it from `GetVolumeInformationW` —
the **volume label**, which is routinely empty. Two unlabelled volumes keyed
on `""`, collapsed into one row, and the volume that lost was never evaluated
by the disk rule again. A monitoring library silently ceasing to monitor a
disk is the worst shape a bug can take here, and it was invisible: the
remaining row looked fine.

**Done:** an empty name is never deduped — correct on every platform, rather
than a default that happens to be right on one.

## 2. The builtin template had zero Windows coverage — fixed

`Google Chrome Helper (Renderer)`, `WindowServer`, `kernel_task`,
`mds_stores` cannot match anything on Windows, where `sysinfo` reports
`chrome.exe` / `explorer.exe`. Every process fell through to the base bar
(30% CPU, min(25%, 4 GiB)) and the machine alerted constantly — which
destroys the property the template exists for. Linux coverage was partial and
accidental: `firefox`, `git`, `make`, `rustc`, `ninja`, `ffmpeg` matched; the
~40 Apple daemons did not.

**Done:** one file per platform, selected by `cfg(target_os)` at the
`include_str!` — `templates/alerts-macos.toml` (also the fallback for any
target without a file of its own), `alerts-linux.toml`,
`alerts-windows.toml`. Separate files rather than
sections inside one keep `Template::parse`'s `deny_unknown_fields` strict,
and `<config-dir>/template.toml` still replaces whichever was selected.

Two corrections to the original audit, both learned while writing the files:

- The entry counts were wrong — the macOS table is 215 entries (cpu 100,
  mem 63, app_cpu 26, app_mem 26), not 71. The `.exe` finding stands: zero.
- **Linux process names are truncated to 15 bytes.** `sysinfo` fills
  `Process::name()` from `/proc/[pid]/stat`'s `comm`, which the kernel caps
  at `TASK_COMM_LEN`. `systemd-journald` arrives as `systemd-journal`,
  `tracker-miner-fs-3` as `tracker-miner-f`, Firefox's content processes as
  `Isolated Web Co`. A key longer than 15 characters therefore matches
  *nothing* — the exact failure the template exists to prevent, and one that
  would have been silent. Every Linux key is within the cap or a pattern that
  survives it, and `every_platform_template_parses_from_any_platform` asserts
  it, alongside parsing all three files on whatever platform runs the tests.
  Only one file is compiled into a given binary, so without that test a
  Windows typo ships and looks exactly like a quiet machine.

## 3. Process-group aggregation merged unrelated trees on Windows — mitigated

`const INIT_PID: u32 = 1` is the walk's terminator. On Windows that arm never
fires, so the chain ended only when a parent was missing from the table.
**Unix reparents orphans to init; Windows leaves the stale ppid and recycles
pids** — so when a dead parent's pid was reused by an unrelated live process
that *was* in the table, the walk climbed into that process's tree and merged
two applications into one group. `process_groups` feeds `AppCpu` and
`AppMemory`, so this was not a display artefact: two live rules evaluated a
fabricated aggregate.

**Done, and better than the stopgap this backlog proposed.** Disabling
`collect_process_groups` on Windows would have retired two rules to avoid a
wrong answer. Instead `is_plausible_parent` rejects a link whose parent
started *after* its alleged child, which is proof the pid was recycled — no
FFI, no platform code, and it keeps Windows aligned with the other two rather
than degraded.

Deliberately conservative: only a start time known on **both** sides and
strictly later on the parent rejects the link, so nothing changes where the
data is incomplete. Seconds are the resolution every platform agrees on.

**Left:** a pid reused inside the same second as its successor's start still
slips through. Closing that needs the real fix — Job Objects
(`IsProcessInJob` / `QueryInformationJobObject`) or
`NtQuerySystemInformation(SystemProcessInformation)`'s
`InheritedFromUniqueProcessId` plus `SessionId`, which is also how Task
Manager groups. Both are Windows API work that cannot be built or tested
without a Windows runner, and untested FFI in a published crate buys false
confidence rather than portability.

## 4. Memory thresholds kept their number and lost their basis — fixed

The 4 GiB per-process ceiling and the 8 GiB whole-app ceiling are what they
are *because* the measurement is footprint: measured live, rust-analyzer held
3.02 GiB of footprint against 0.17 GiB of RSS, and a whole `zed` tree held
8.32 GiB against 1.30 GiB. Off macOS `phys_footprint_bytes` was `None`, the
rules took their `unwrap_or(memory_avg_bytes)` fallback, and **the same
number silently measured RSS instead**, firing on roughly a third as much.

**Done:** supply footprint's equivalent rather than re-tune the number.

| | Source | Cost |
|---|---|---|
| macOS | `ri_phys_footprint` (`proc_pid_rusage`) | ~0.5µs per process |
| Windows | `PROCESS_MEMORY_COUNTERS_EX.PrivateUsage` | free — `sysinfo` already reads it for every process |
| Linux | `RssAnon + VmSwap` from `/proc/[pid]/status` | one small read per process |

Windows note: `sysinfo` surfaces that same `PrivateUsage` as
`virtual_memory`, so `virtual_memory_bytes` and `phys_footprint_bytes` carry
the same number there. Documented rather than hidden.

Linux note: `smaps_rollup`'s `Pss + SwapPss` is the more precise answer but
walks every VMA in the kernel, far too expensive to run over the full process
table on each refresh. `RssAnon + VmSwap` is anonymous resident plus the
anonymous pages swapped out — which is where zram/zswap moves a leak — and is
the semantic match for what `phys_footprint` counts.

**Left:** the Linux per-process `/proc` read is *unmeasured*. On a busy
machine the full-table pass (which process groups require) is a few hundred
reads per process refresh; the design assumes that lands in single-digit
milliseconds on the process cadence, but nobody has timed it. If it does not,
`/proc/[pid]/statm` is the cheaper fallback at the cost of exactness.

## 5. The pressure rule was dead off macOS while reporting itself enabled — fixed

The rule short-circuits on `snapshot.memory.pressure_level`, and a test
asserts that silence for non-macOS, so the behaviour was intended. The gap
was the advertising: `pressure` defaults to `Warning`, so `any_enabled()`
returned true and `Monitor::alerts_enabled()` reported alerting as live. A
settings UI rendered a working pressure control that could never fire.

**Done:** `ActiveThresholds::supports(AlertKind)` answers *can this build
evaluate the rule at all*, separately from whether the user enabled it, and
`any_enabled()` now requires support. A Windows or Linux build whose only
remaining rule is pressure reports alerting as off, instead of offering a
control that silently does nothing.

**Left:** the Linux analogue itself. `/proc/pressure/memory` is a **stall
percentage over 10/60/300s windows**, not a 1/2/4 level, so `PressureAlert`
needs a deliberate remapping rather than a cast — and the existing rule's
whole design (episodes, 5-minute persistence, a backing-off reminder) was
calibrated against a noisy step function, not against a continuous
percentage. Worth doing as its own decision, not as a port.

## 6. `None` could not say why — half fixed

`phys_footprint_bytes: None` means "this platform cannot", "permission
denied" (`proc_pid_rusage` is EPERM for other users' processes on macOS) or
"the process exited mid-collect". `read_bytes_per_sec: None` means "first
sample" or "collection disabled". A frontend that wants to be honest cannot
distinguish them, so it either invents a reason or stays vague.

**Done:** `Capabilities` on every `SystemSnapshot` (`serde(default)`, so the
wire format stays compatible both ways) — `memory_footprint`,
`memory_pressure`, `cpu_perf_levels`. It is a property of the **build**, not
the machine: `cpu_perf_levels` is true on any macOS build, including an Intel
Mac whose `perf_levels` is legitimately empty. It travels with the snapshot
so a frontend attached to a daemon reads the *daemon's* answer rather than
guessing from its own build.

**Left:** the per-value half. `Unavailable { reason }` is what EPERM needs,
and it is a breaking change to every `Option` it touches — worth doing when
the API is next revised, not before. Until then a frontend disclosing "RSS
(footprint unavailable)" on a row is still doing it by hand.

## 7. Temperature collection was not free on Windows — fixed

`sysinfo`'s Windows implementation queries WMI
(`MSAcpi_ThermalZoneTemperature`), calls `CoInitializeEx` on the calling
thread and sets a **process-global** `CoInitializeSecurity`. For an embedded
frontend that is a real integration hazard — the host may have its own COM
apartment or security requirements — and it happens on the collector thread
every cadence. The payload is at most one component, hard-labelled
`"Computer"`, and on most consumer hardware there is none.

**Done:** `collect_temperatures` defaults to `false` on Windows only. Linux
is untouched and fine — `/sys/class/hwmon` gives many labelled sensors, in
practice more than macOS.

---

## Not in the original audit

Both found while writing up the platform comparison, not during the 0.5.1
pass. Neither is a defect — they are boundaries a frontend has to know about.

### Load average is the one field whose MEANING differs

macOS reads `getloadavg(3)` and Linux reads `/proc/loadavg`: both are the
kernel's own figure, and on Linux that famously includes uninterruptible
(`D`-state) tasks, so IO stalls show up as load. **Windows has no such
concept.** `sysinfo` emulates it from the PDH counter
`\System\Cpu Queue Length`, sampled every 5s and exponentially smoothed with
psutil's factors (0.9200 / 0.9835 / 0.9945 for 1/5/15 min). So the Windows
number is a *run-queue length*, it excludes anything blocked on IO, and it
reads **0.0 for all three windows** until the background PDH query has
initialised and accumulated.

Nothing in the rules depends on it — the process-refresh boost reads
`global_cpu_usage()`, not load — so this is a display concern only. But a
frontend that puts load next to CPU% should not present the Windows value as
the same quantity, and should not read 0.00/0.00/0.00 as an idle machine.

### The daemon layer is unix-only, and that costs Windows the alerting

`serve` / `attach` / `stop` are `cfg(unix)`, and `zstats::client` is
`cfg(all(feature = "client", unix))` — the transport is a Unix socket. The
consequence is bigger than the missing commands: **`AlertSink` is only ever
wired up inside `serve`, so a Windows CLI has no background alerting and
writes no `data/*.jsonl` history at all.** What is left there is the
foreground live view, `--json`, and `-add`/`-remove`/`-list`.

The library half is *not* limited that way. `zstats::alerts`,
`zstats::records` and `zstats::monitor` are feature-gated only, so an
embedded frontend on Windows can drive `Monitor::tick()` and get the same
rules, the same rolling windows and the same history files — it only has to
deliver its own notifications, since `send_notification` (osascript /
notify-send) lives in the bin.

**Left:** a Windows daemon would need a different transport — a named pipe,
or a localhost TCP socket with the obvious authentication question that
raises. Worth its own decision rather than a port; the wire format itself
(`ATTACH` / `HISTORY <n>` / JSON lines) is transport-agnostic already.

---

## Checked and portable

Recorded so nobody re-audits: `settings::default_dir()` already tries `HOME`
then `USERPROFILE` (`C:\Users\you\.zstats`, deliberately not `%APPDATA%`
because the directory is shared with the CLI); `cpu_time_ms`, disk IO rates,
network byte/packet/error counters and battery are all cross-platform; the
CPU and disk rules read only cross-platform inputs; records, history,
settings and the alert engine's plumbing are pure std/serde/jiff. Load
average is the exception to that list — same field, different meaning, see
above.

## What a first Windows or Linux run should check

In the order that a wrong answer would matter most:

1. Process names against the template — `zstats --json` and grep for names
   that fall through. This is the assumption with the widest blast radius,
   and the one most likely to be wrong in detail.
2. `phys_footprint_bytes` present and plausible on every process, and larger
   than `memory_bytes` for anything under memory pressure.
3. `process_groups` roots — whether trees look like applications, and whether
   the same-second pid-reuse window ever bites in practice.
4. The cost of the Linux `/proc/[pid]/status` pass, against the ~20ms the
   process refresh already costs.
5. Whether `collect_temperatures = true` on Windows actually breaks an
   embedded host's COM setup, which would turn item 7's default into a
   requirement.
6. What the Windows load average actually reads on an idle and on a busy
   machine — whether the PDH query initialises at all under a detached or
   service-hosted process, and how long it takes to stop reporting zeros.

// Copyright 2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Core data model: every collection result converges into
//! [`SystemSnapshot`], the single data contract inside and outside
//! the module.

use std::collections::HashMap;
use std::sync::Arc;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A complete system sampling result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSnapshot {
    /// Sampling time (UTC, serialized as an RFC 3339 string)
    pub timestamp: Timestamp,

    /// Host identification info
    pub host: HostInfo,

    /// CPU metrics
    pub cpu: CpuSnapshot,

    /// Memory metrics
    pub memory: MemorySnapshot,

    /// Disk metrics; None when disk collection is disabled
    pub disks: Option<Vec<DiskSnapshot>>,

    /// Network metrics; None when network collection is disabled
    pub networks: Option<Vec<NetworkSnapshot>>,

    /// Process list; None when process collection is disabled.
    /// Wrapped in [`Arc`] so cloning a snapshot (scheduler → sinks,
    /// daemon history) does not deep-copy the process table.
    pub processes: Option<Arc<Vec<ProcessSnapshot>>>,

    /// Per-application aggregates over whole process trees; None when
    /// process or group collection is disabled. Refreshes on the process
    /// cadence; same [`Arc`] sharing as `processes`.
    #[serde(default)]
    pub process_groups: Option<Arc<Vec<ProcessGroupSnapshot>>>,

    /// Total number of processes in the system table as of the last
    /// process refresh — `processes` only keeps the top N. None when
    /// process collection is disabled
    #[serde(default)]
    pub total_processes: Option<u32>,

    /// Main battery; None when battery collection is disabled or the
    /// machine has no battery (desktop, VM)
    #[serde(default)]
    pub battery: Option<BatterySnapshot>,

    /// Load averages
    pub load: LoadSnapshot,

    /// Hardware temperature sensors; None when temperature collection is
    /// disabled. Empty vec means collection ran but no plausible readings
    /// were available (platform/driver dependent). Sorted hottest-first.
    #[serde(default)]
    pub temperatures: Option<Vec<TemperatureSnapshot>>,

    /// Machine-wide disk and network byte rates, summed from the per-device
    /// lists after collection (and after disk dedupe when enabled). Pure
    /// aggregation — no extra system calls. Fields are None when the
    /// subsystem is disabled or rates are not yet available (first sample).
    #[serde(default)]
    pub io_totals: IoTotalsSnapshot,

    /// What this build can measure at all, so a `None` above can be read
    /// as "not on this platform" rather than "not right now"
    #[serde(default)]
    pub capabilities: Capabilities,

    /// Extension fields (reserved)
    #[serde(default)]
    pub extras: HashMap<String, serde_json::Value>,
}

/// Which platform-specific measurements this build can produce at all.
///
/// Every platform-specific metric here is an `Option` that reads `None`
/// where it is unavailable — the designed degradation. What an `Option`
/// alone cannot say is WHY it is empty: "this platform has no such
/// concept", "the kernel refused for this process", or "not sampled
/// yet". A frontend that wants to be honest has to distinguish them, and
/// without this it either invents a reason or stays vague.
///
/// This answers the first case only, and it is a property of the BUILD,
/// not of the machine: `cpu_perf_levels` is true on any macOS build,
/// including an Intel Mac whose `perf_levels` is legitimately empty
/// because the CPU is homogeneous. Per-value reasons (EPERM on another
/// user's process) would need an `Unavailable { reason }` on the value
/// itself; this is the cheap half, and it travels with the snapshot so a
/// frontend attached to a daemon reads the DAEMON's capabilities rather
/// than guessing from its own build
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `ProcessSnapshot::phys_footprint_bytes` and the group sum can
    /// carry a real footprint (macOS, Windows, Linux). When false the
    /// memory rules silently measure resident size instead
    pub memory_footprint: bool,
    /// `MemorySnapshot::pressure_level` / `compressed_bytes` exist, i.e.
    /// the pressure alert rule can fire (macOS only today; Linux PSI
    /// would need a deliberate remapping, being a stall percentage over
    /// 10/60/300s windows rather than a 1/2/4 level)
    pub memory_pressure: bool,
    /// `CpuSnapshot::perf_levels` can be reported (macOS only)
    pub cpu_perf_levels: bool,
}

impl Capabilities {
    /// What the build running this code can do
    pub const fn current() -> Self {
        Self {
            memory_footprint: cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            )),
            memory_pressure: cfg!(target_os = "macos"),
            cpu_perf_levels: cfg!(target_os = "macos"),
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::current()
    }
}

/// Summed disk/network throughput for the whole machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IoTotalsSnapshot {
    #[serde(default)]
    pub disk_read_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub disk_write_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub network_received_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub network_transmitted_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: Option<String>,
    pub arch: String,
    /// Seconds since boot
    #[serde(default)]
    pub uptime_secs: u64,
    /// User-defined labels for distinguishing multiple machines
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// The machine's main battery, when it has one.
///
/// Read through a safe cross-platform wrapper (IOKit on macOS, sysfs on
/// Linux), so every field is optional where the platform may not report
/// it. Desktops and VMs yield no battery at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySnapshot {
    /// "Charging", "Discharging", "Full", "Empty" or "Unknown" — the
    /// direction of travel; frontends derive "on AC" from it
    pub state: String,
    /// Current charge, 0-100
    pub charge_percent: f32,
    /// Full capacity relative to design capacity, 0-100: battery wear
    pub health_percent: Option<f32>,
    /// Charge cycles the battery has been through
    pub cycle_count: Option<u32>,
    /// Battery temperature in Celsius (separate from the CPU sensors)
    pub temperature_celsius: Option<f32>,
    /// Present power flow in watts (charging or discharging, per `state`)
    pub power_watts: Option<f32>,
    /// Estimated seconds until full (while charging)
    pub time_to_full_secs: Option<u64>,
    /// Estimated seconds until empty (while discharging)
    pub time_to_empty_secs: Option<u64>,
}

/// Usage of one CPU performance level (Apple Silicon P/E clusters).
///
/// Static topology (level names and core counts) comes from the OS at
/// collector startup; usage is the average of the level's cores from the
/// same sample as `per_core_usage`. Dynamic per-cluster frequency/power
/// are deliberately out of scope (they need root or private APIs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfLevelSnapshot {
    /// Level name as reported by the OS (e.g. "Performance", "Efficiency")
    pub name: String,
    /// Logical cores in this level
    pub logical_cores: u32,
    /// Average usage across this level's cores (0-100)
    pub usage_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSnapshot {
    /// Overall usage, 0.0 ~ 100.0
    pub usage_percent: f32,
    /// Per-core usage
    pub per_core_usage: Vec<f32>,
    /// Number of logical cores
    pub logical_cores: u32,
    /// Number of physical cores (if available)
    pub physical_cores: Option<u32>,
    /// Current frequency (MHz, optional). Refreshed on a slower cadence
    /// than usage; may lag a few tens of seconds. First non-zero core
    /// frequency when available (see also `per_core_frequency_mhz`).
    pub frequency_mhz: Option<u64>,
    /// Per-core frequency in MHz (0 when the platform does not report one
    /// for that core). Same length as logical cores when any frequency is
    /// known; empty otherwise. Refreshed with `frequency_mhz`.
    #[serde(default)]
    pub per_core_frequency_mhz: Vec<u64>,
    /// CPU brand string from the OS (e.g. "Apple M3 Pro"), when available
    #[serde(default)]
    pub brand: Option<String>,
    /// Per-performance-level usage (heterogeneous CPUs, e.g. Apple
    /// Silicon P/E clusters), highest-performance level first. None when
    /// the platform reports fewer than two levels
    #[serde(default)]
    pub perf_levels: Option<Vec<PerfLevelSnapshot>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    /// `used_bytes / total_bytes * 100` (0 when total is 0)
    #[serde(default)]
    pub used_percent: f32,
    /// `swap_used_bytes / swap_total_bytes * 100` (0 when no swap)
    #[serde(default)]
    pub swap_used_percent: f32,
    /// Bytes held by the OS memory compressor (macOS). Growth here is the
    /// first sign of real memory pressure — long before "used" looks bad.
    /// None on platforms without a compressor metric
    #[serde(default)]
    pub compressed_bytes: Option<u64>,
    /// The kernel's own memory-pressure verdict (macOS
    /// `kern.memorystatus_vm_pressure_level`): 1 = normal, 2 = warning,
    /// 4 = critical. None when the platform does not report one
    #[serde(default)]
    pub pressure_level: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSnapshot {
    pub name: String,
    pub mount_point: String,
    pub file_system: String,
    /// Disk kind as reported by the OS: "SSD", "HDD", or "Unknown"
    #[serde(default)]
    pub kind: String,
    /// Whether the volume is removable (USB, etc.)
    #[serde(default)]
    pub is_removable: bool,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Capacity used: `(total - available) / total * 100` (0 when total is 0)
    #[serde(default)]
    pub used_percent: f32,
    /// Read/write byte rates since the last sample (computed by diffing
    /// inside the Collector; None on the first sample)
    pub read_bytes_per_sec: Option<u64>,
    pub write_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSnapshot {
    pub interface: String,
    pub received_bytes_per_sec: u64,
    pub transmitted_bytes_per_sec: u64,
    pub received_packets_per_sec: Option<u64>,
    pub transmitted_packets_per_sec: Option<u64>,
    /// Receive/transmit error rates; None on the first sample
    #[serde(default)]
    pub received_errors_per_sec: Option<u64>,
    #[serde(default)]
    pub transmitted_errors_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    /// The application this process belongs to, when the executable's
    /// own name does not say it: `/Applications/CodeBuddy CN.app/
    /// Contents/MacOS/Electron` is named `Electron` by every kernel
    /// interface, which is the stock Electron binary name shared by
    /// every app that never renamed it. Derived from the enclosing
    /// `.app` bundle, so it is the name Finder and Activity Monitor
    /// show, and `None` whenever the executable is not inside a bundle
    /// (all of Linux and Windows, and most macOS daemons).
    ///
    /// PRESENTATION ONLY. `name` stays the identity that alert
    /// thresholds, templates and overrides match on — one process has
    /// exactly one matchable name, and a bundle holds several distinct
    /// processes (`Google Chrome` and `Google Chrome Helper (Renderer)`
    /// carry different bars on purpose) that this field would collapse.
    #[serde(default)]
    pub display_name: Option<String>,
    pub cmd: String,
    pub cpu_usage_percent: f32,
    /// Total CPU time consumed since the process started, in single-core
    /// milliseconds (2000 = two core-seconds). A COUNTER, not a rate:
    /// diff two samples to get what was burned in between.
    ///
    /// This is the field that makes "quietly 10% for twelve hours"
    /// visible. `cpu_usage_percent` never crosses any threshold there,
    /// but the integral does: 10% for 12h is 1.2 core-hours — seven
    /// times what a process pegged at 100% for ten minutes costs, and
    /// the latter is the one that alerts today. Counts only time
    /// actually spent running, so system sleep and the collector's own
    /// cadence cannot distort it.
    #[serde(default)]
    pub cpu_time_ms: u64,
    pub memory_bytes: u64,
    /// Physical footprint in bytes — what macOS bills the process for:
    /// private dirty memory, compressed pages, and GPU/IOKit allocations
    /// (IOSurface, Metal buffers). This is the figure Activity Monitor's
    /// Memory column shows and what memory-pressure/jetsam accounting
    /// uses, and it diverges from `memory_bytes` (RSS) in BOTH
    /// directions: RSS counts shared framework pages footprint excludes,
    /// footprint counts compressed and GPU memory RSS cannot see. A GUI
    /// app can legitimately read 80 MB in one and 300 MB in the other.
    ///
    /// macOS only, and only for processes the collector may inspect —
    /// `proc_pid_rusage` fails on other users' processes, so an
    /// unprivileged collector reports None for root-owned daemons
    /// (Activity Monitor sees them through the privileged sysmond).
    /// None on every other platform.
    #[serde(default)]
    pub phys_footprint_bytes: Option<u64>,
    /// Virtual address space size (when available)
    #[serde(default)]
    pub virtual_memory_bytes: u64,
    /// Seconds the process has been running
    #[serde(default)]
    pub run_time_secs: u64,
    pub parent_pid: Option<u32>,
    /// Owning user identifier, as text because platforms disagree on the
    /// type: a numeric uid on unix ("501"), a security identifier on
    /// Windows ("S-1-5-18"). None when permissions hide it
    #[serde(default)]
    pub user_id: Option<String>,
    pub status: String,
    /// Per-process disk IO rates; None when process disk IO collection
    /// is disabled or on the first sample for that pid
    #[serde(default)]
    pub read_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub write_bytes_per_sec: Option<u64>,
}

/// One application's whole process tree, aggregated: the root (a direct
/// child of init/launchd) plus every descendant. This is what makes a
/// multi-process app (a browser and its helpers, an Electron app) visible
/// as a single entity — per-process values never exceed thresholds that
/// the app as a whole does.
///
/// Computed over the FULL process table before top-N selection, so an app
/// made of many small helpers is summed correctly even when none of its
/// members rank into [`SystemSnapshot::processes`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessGroupSnapshot {
    /// Pid of the tree root
    pub root_pid: u32,
    /// Name of the root process
    pub name: String,
    /// The application this tree belongs to, when the root executable's
    /// own name does not say it — see `ProcessSnapshot::display_name`.
    /// Presentation only, for the same reason: the group's matchable
    /// identity has to stay the one name a threshold key can be written
    /// against.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Number of processes in the group (root included)
    pub process_count: u32,
    /// Sum of member CPU usage (single-core units, like per-process CPU)
    pub cpu_usage_percent: f32,
    /// Sum of member resident memory
    pub memory_bytes: u64,
    /// Sum of member physical footprints, falling back to a member's
    /// resident size where the kernel refused to report one; `None` when
    /// no member had a footprint at all (every platform but macOS).
    ///
    /// This is the figure the whole-application memory rule measures, for
    /// the same reason the per-process rule does: RSS cannot see
    /// compressed pages, so a group whose pages the kernel compressed
    /// reads as SHRINKING exactly while it is squeezing the machine —
    /// and a browser split across dozens of helpers is the case that
    /// compresses hardest
    #[serde(default)]
    pub phys_footprint_bytes: Option<u64>,
    /// Sum of member disk read/write rates; None unless
    /// `collect_process_disk_io` is enabled
    #[serde(default)]
    pub read_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub write_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSnapshot {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

/// One hardware temperature sensor (CPU die, battery, NAND, …).
///
/// Labels and availability are platform-specific. On macOS the names are
/// often raw firmware strings (e.g. `PMU tdie8`); values outside a
/// plausible range are dropped by the collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureSnapshot {
    /// Sensor label as reported by the OS / sysinfo
    pub label: String,
    /// Current temperature in Celsius
    pub celsius: f32,
    /// Highest reading observed for this sensor (when available)
    #[serde(default)]
    pub max_celsius: Option<f32>,
    /// Critical / shutdown threshold (when available)
    #[serde(default)]
    pub critical_celsius: Option<f32>,
}

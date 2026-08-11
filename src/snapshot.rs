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

    /// Extension fields (reserved)
    #[serde(default)]
    pub extras: HashMap<String, serde_json::Value>,
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
    /// than usage; may lag a few tens of seconds.
    pub frequency_mhz: Option<u64>,
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
    pub cmd: String,
    pub cpu_usage_percent: f32,
    pub memory_bytes: u64,
    /// Virtual address space size (when available)
    #[serde(default)]
    pub virtual_memory_bytes: u64,
    /// Seconds the process has been running
    #[serde(default)]
    pub run_time_secs: u64,
    pub parent_pid: Option<u32>,
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
    /// Number of processes in the group (root included)
    pub process_count: u32,
    /// Sum of member CPU usage (single-core units, like per-process CPU)
    pub cpu_usage_percent: f32,
    /// Sum of member resident memory
    pub memory_bytes: u64,
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

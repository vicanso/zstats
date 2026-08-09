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

    /// Load averages
    pub load: LoadSnapshot,

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSnapshot {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

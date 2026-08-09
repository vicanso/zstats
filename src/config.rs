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

//! Collector configuration.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    /// Whether to collect process info (relatively expensive)
    pub collect_processes: bool,

    /// How often to refresh the process list. Zero (the default) refreshes
    /// on every collect. A tray-style embedder that only needs a coarse
    /// "top processes" view can set e.g. 10s: between refreshes the
    /// snapshot carries the last collected list, and per-process CPU% is
    /// averaged over the longer window (smoother rankings)
    pub process_refresh_interval: Duration,

    /// While overall CPU load is at or above this many *logical cores* of
    /// work, the process list refreshes on every collect regardless of
    /// `process_refresh_interval` — busy periods get precise per-process
    /// attribution, idle periods stay cheap. None disables the boost.
    ///
    /// Units match per-process CPU% (single-core units): `1.0` means "at
    /// least one full core busy". The effective overall-usage threshold is
    /// therefore `cores / logical_cores * 100`, so the same setting scales
    /// across machine sizes:
    /// - 4 cores, threshold 1.0 → boost at ≥ 25% overall
    /// - 64 cores, threshold 1.0 → boost at ≥ ~1.6% overall
    ///
    /// Default `Some(1.0)` catches a single busy process on both laptops
    /// and big servers. Raise it (e.g. 4.0) when baseline load is high and
    /// you want fewer forced process refreshes.
    pub process_boost_cpu_cores: Option<f32>,

    /// Max number of processes to keep. The budget is split between the
    /// top-by-CPU and top-by-memory rankings so that both views stay
    /// meaningful; the returned list is sorted by CPU desc
    pub max_processes: usize,

    /// Also collect per-process disk read/write byte rates. Off by default
    /// because it requires an extra refresh kind on every process pass.
    /// When disabled, `ProcessSnapshot::{read,write}_bytes_per_sec` stay
    /// `None`.
    pub collect_process_disk_io: bool,

    /// Whether to collect per-core CPU usage
    pub per_core_cpu: bool,

    /// How often to refresh CPU frequency. Frequency changes slowly relative
    /// to usage; refreshing it every collect is wasted work. Usage is still
    /// refreshed on every collect. Zero refreshes frequency every collect.
    pub cpu_frequency_refresh_interval: Duration,

    /// Whether to collect disk metrics
    pub collect_disks: bool,

    /// How often to refresh disk capacity (total/available bytes). The
    /// capacity query is by far the most expensive part of disk collection
    /// (~18ms per round on macOS vs ~0.7ms for IO counters) and the data
    /// barely changes, so it runs on its own slower cadence; IO counters
    /// still refresh on every collect. Capacity values between refreshes
    /// are the last known ones
    pub disk_storage_refresh_interval: Duration,

    /// When true, keep a single entry per disk device name (preferring the
    /// shortest mount point). Collapses APFS synthetic mounts such as `/`
    /// and `/System/Volumes/Data` that report the same underlying volume.
    pub dedupe_disks: bool,

    /// Whether to collect network interface metrics
    pub collect_networks: bool,

    /// Whether to collect hardware temperature sensors (sysinfo Components).
    /// Platform-dependent: macOS returns many named sensors with occasional
    /// garbage values (filtered); some environments report none.
    pub collect_temperatures: bool,

    /// How often to refresh temperatures. Sensors change slowly compared to
    /// CPU counters, so a multi-second cadence is enough and avoids extra
    /// IOKit/hwmon work every collect. Between refreshes the last reading
    /// is reused. Zero refreshes every collect.
    pub temperature_refresh_interval: Duration,

    /// Custom host labels
    pub labels: HashMap<String, String>,

    /// Collect timeout (guards against platform calls that hang)
    pub collect_timeout: Duration,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            collect_processes: true,
            process_refresh_interval: Duration::ZERO,
            process_boost_cpu_cores: Some(1.0),
            max_processes: 50,
            collect_process_disk_io: false,
            per_core_cpu: true,
            cpu_frequency_refresh_interval: Duration::from_secs(30),
            collect_disks: true,
            disk_storage_refresh_interval: Duration::from_secs(60),
            dedupe_disks: true,
            collect_networks: true,
            collect_temperatures: true,
            // Temps drift over seconds/minutes, not milliseconds
            temperature_refresh_interval: Duration::from_secs(15),
            labels: HashMap::new(),
            collect_timeout: Duration::from_secs(2),
        }
    }
}

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

    /// Max number of processes to collect (sorted by CPU then memory,
    /// then truncated)
    pub max_processes: usize,

    /// Whether to collect per-core CPU usage
    pub per_core_cpu: bool,

    /// Whether to collect disk metrics (volume enumeration is the most
    /// expensive refresh on some platforms, e.g. ~20ms on macOS)
    pub collect_disks: bool,

    /// Whether to collect network interface metrics
    pub collect_networks: bool,

    /// Custom host labels
    pub labels: HashMap<String, String>,

    /// Collect timeout (guards against platform calls that hang)
    pub collect_timeout: Duration,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            collect_processes: true,
            max_processes: 50,
            per_core_cpu: true,
            collect_disks: true,
            collect_networks: true,
            labels: HashMap::new(),
            collect_timeout: Duration::from_secs(2),
        }
    }
}

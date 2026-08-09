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

//! Local machine collector implementation (based on sysinfo).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

use crate::collector::Collector;
use crate::config::CollectorConfig;
use crate::error::CollectError;
use crate::snapshot::{
    CpuSnapshot, DiskSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, NetworkSnapshot,
    ProcessSnapshot, SystemSnapshot,
};
use crate::utils::rate::rate_per_sec;

#[derive(Debug, Clone, Copy)]
struct DiskCounters {
    total_read_bytes: u64,
    total_written_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct NetCounters {
    received_bytes: u64,
    transmitted_bytes: u64,
    received_packets: u64,
    transmitted_packets: u64,
}

pub struct LocalCollector {
    config: CollectorConfig,
    system: System,
    disks: Disks,
    networks: Networks,
    /// Static host info, gathered once at construction
    host: HostInfo,
    // Internal state: the previous sample used for rate calculation
    last_disk_counters: HashMap<String, DiskCounters>,
    last_net_counters: HashMap<String, NetCounters>,
    last_collect_time: Option<Instant>,
}

impl LocalCollector {
    pub fn new(config: CollectorConfig) -> Self {
        let mut system = System::new();
        // Refresh once up front so the first collect has a baseline for CPU usage
        system.refresh_cpu_all();
        system.refresh_memory();

        let host = HostInfo {
            hostname: System::host_name().unwrap_or_default(),
            os_name: System::name().unwrap_or_default(),
            os_version: System::os_version().unwrap_or_default(),
            kernel_version: System::kernel_version(),
            arch: std::env::consts::ARCH.to_string(),
            labels: config.labels.clone(),
        };

        Self {
            config,
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            host,
            last_disk_counters: HashMap::new(),
            last_net_counters: HashMap::new(),
            last_collect_time: None,
        }
    }

    fn refresh(&mut self) {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        if self.config.collect_processes {
            self.system.refresh_processes(ProcessesToUpdate::All, true);
        }
        self.disks.refresh(true);
        self.networks.refresh(true);
    }

    fn collect_cpu(&self) -> CpuSnapshot {
        let cpus = self.system.cpus();
        let per_core_usage = if self.config.per_core_cpu {
            cpus.iter().map(|c| c.cpu_usage()).collect()
        } else {
            Vec::new()
        };
        // Some platforms report no frequency (0); map that to None uniformly
        let frequency_mhz = cpus.iter().map(|c| c.frequency()).find(|f| *f > 0);

        CpuSnapshot {
            usage_percent: self.system.global_cpu_usage(),
            per_core_usage,
            logical_cores: cpus.len() as u32,
            physical_cores: System::physical_core_count().map(|n| n as u32),
            frequency_mhz,
        }
    }

    fn collect_memory(&self) -> MemorySnapshot {
        MemorySnapshot {
            total_bytes: self.system.total_memory(),
            used_bytes: self.system.used_memory(),
            available_bytes: self.system.available_memory(),
            swap_total_bytes: self.system.total_swap(),
            swap_used_bytes: self.system.used_swap(),
        }
    }

    fn collect_disks(&mut self, elapsed: Option<Duration>) -> Vec<DiskSnapshot> {
        let mut snapshots = Vec::new();
        let mut counters = HashMap::new();

        for disk in self.disks.list() {
            let name = disk.name().to_string_lossy().to_string();
            let mount_point = disk.mount_point().to_string_lossy().to_string();
            let key = format!("{name}:{mount_point}");

            let usage = disk.usage();
            let current = DiskCounters {
                total_read_bytes: usage.total_read_bytes,
                total_written_bytes: usage.total_written_bytes,
            };

            let (read_bytes_per_sec, write_bytes_per_sec) =
                match (elapsed, self.last_disk_counters.get(&key)) {
                    (Some(elapsed), Some(prev)) => (
                        Some(rate_per_sec(
                            prev.total_read_bytes,
                            current.total_read_bytes,
                            elapsed,
                        )),
                        Some(rate_per_sec(
                            prev.total_written_bytes,
                            current.total_written_bytes,
                            elapsed,
                        )),
                    ),
                    _ => (None, None),
                };

            counters.insert(key, current);
            snapshots.push(DiskSnapshot {
                name,
                mount_point,
                file_system: disk.file_system().to_string_lossy().to_string(),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                read_bytes_per_sec,
                write_bytes_per_sec,
            });
        }

        // Replace wholesale so counters of removed disks are pruned automatically
        self.last_disk_counters = counters;
        snapshots
    }

    fn collect_networks(&mut self, elapsed: Option<Duration>) -> Vec<NetworkSnapshot> {
        let mut snapshots = Vec::new();
        let mut counters = HashMap::new();

        for (interface, data) in self.networks.iter() {
            let current = NetCounters {
                received_bytes: data.total_received(),
                transmitted_bytes: data.total_transmitted(),
                received_packets: data.total_packets_received(),
                transmitted_packets: data.total_packets_transmitted(),
            };

            let snapshot = match (elapsed, self.last_net_counters.get(interface)) {
                (Some(elapsed), Some(prev)) => NetworkSnapshot {
                    interface: interface.clone(),
                    received_bytes_per_sec: rate_per_sec(
                        prev.received_bytes,
                        current.received_bytes,
                        elapsed,
                    ),
                    transmitted_bytes_per_sec: rate_per_sec(
                        prev.transmitted_bytes,
                        current.transmitted_bytes,
                        elapsed,
                    ),
                    received_packets_per_sec: Some(rate_per_sec(
                        prev.received_packets,
                        current.received_packets,
                        elapsed,
                    )),
                    transmitted_packets_per_sec: Some(rate_per_sec(
                        prev.transmitted_packets,
                        current.transmitted_packets,
                        elapsed,
                    )),
                },
                _ => NetworkSnapshot {
                    interface: interface.clone(),
                    received_bytes_per_sec: 0,
                    transmitted_bytes_per_sec: 0,
                    received_packets_per_sec: None,
                    transmitted_packets_per_sec: None,
                },
            };

            counters.insert(interface.clone(), current);
            snapshots.push(snapshot);
        }

        self.last_net_counters = counters;
        snapshots
    }

    fn collect_processes(&self) -> Option<Vec<ProcessSnapshot>> {
        if !self.config.collect_processes {
            return None;
        }

        let mut processes: Vec<ProcessSnapshot> = self
            .system
            .processes()
            .values()
            .map(|p| ProcessSnapshot {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().to_string(),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|c| c.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                cpu_usage_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
                parent_pid: p.parent().map(|pp| pp.as_u32()),
                status: p.status().to_string(),
            })
            .collect();

        // Sort by CPU desc, then memory desc, then truncate
        processes.sort_by(|a, b| {
            b.cpu_usage_percent
                .total_cmp(&a.cpu_usage_percent)
                .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
        });
        processes.truncate(self.config.max_processes);
        Some(processes)
    }

    fn collect_load(&self) -> LoadSnapshot {
        let load = System::load_average();
        LoadSnapshot {
            load1: load.one,
            load5: load.five,
            load15: load.fifteen,
        }
    }
}

impl Collector for LocalCollector {
    fn collect(&mut self) -> Result<SystemSnapshot, CollectError> {
        self.refresh();

        let elapsed = self.last_collect_time.map(|t| t.elapsed());
        self.last_collect_time = Some(Instant::now());

        Ok(SystemSnapshot {
            timestamp: Timestamp::now(),
            host: self.host.clone(),
            cpu: self.collect_cpu(),
            memory: self.collect_memory(),
            disks: self.collect_disks(elapsed),
            networks: self.collect_networks(elapsed),
            processes: self.collect_processes(),
            load: self.collect_load(),
            extras: HashMap::new(),
        })
    }

    fn config(&self) -> &CollectorConfig {
        &self.config
    }
}

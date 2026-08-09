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
use sysinfo::{
    DiskRefreshKind, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind,
};

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
    last_disk_storage_refresh: Option<Instant>,
    last_process_refresh: Option<Instant>,
    /// Last collected process list, reused between process refreshes when
    /// `process_refresh_interval` is non-zero
    cached_processes: Option<Vec<ProcessSnapshot>>,
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

        // Skip building refreshed lists for disabled subsystems: the initial
        // enumeration itself is costly (disks are the most expensive refresh
        // on some platforms)
        // The initial enumeration includes capacity, so the storage cadence
        // starts satisfied
        let (disks, last_disk_storage_refresh) = if config.collect_disks {
            (Disks::new_with_refreshed_list(), Some(Instant::now()))
        } else {
            (Disks::new(), None)
        };
        let networks = if config.collect_networks {
            Networks::new_with_refreshed_list()
        } else {
            Networks::new()
        };

        Self {
            config,
            system,
            disks,
            networks,
            host,
            last_disk_counters: HashMap::new(),
            last_net_counters: HashMap::new(),
            last_collect_time: None,
            last_disk_storage_refresh,
            last_process_refresh: None,
            cached_processes: None,
        }
    }

    fn refresh(&mut self, refresh_processes: bool) {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        if refresh_processes {
            // Explicit refresh kind: the `refresh_processes` shortcut does
            // NOT fetch cmd (and wastes time on disk_usage/exe we don't
            // expose). cmd is immutable per process, so OnlyIfNotSet
            // fetches it exactly once per process
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_memory()
                    .with_cpu()
                    .with_cmd(UpdateKind::OnlyIfNotSet),
            );
            self.last_process_refresh = Some(Instant::now());
        }
        if self.config.collect_disks {
            // IO counters are cheap and refresh every round; the capacity
            // query costs ~25x more and barely changes, so it runs on the
            // configured slower cadence
            let storage_due = self
                .last_disk_storage_refresh
                .is_none_or(|t| t.elapsed() >= self.config.disk_storage_refresh_interval);
            let mut kind = DiskRefreshKind::nothing().with_io_usage();
            if storage_due {
                kind = kind.with_storage();
                self.last_disk_storage_refresh = Some(Instant::now());
            }
            self.disks.refresh_specifics(true, kind);
        }
        if self.config.collect_networks {
            self.networks.refresh(true);
        }
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

    fn collect_disks(&mut self, elapsed: Option<Duration>) -> Option<Vec<DiskSnapshot>> {
        if !self.config.collect_disks {
            return None;
        }

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
        Some(snapshots)
    }

    fn collect_networks(&mut self, elapsed: Option<Duration>) -> Option<Vec<NetworkSnapshot>> {
        if !self.config.collect_networks {
            return None;
        }

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
        Some(snapshots)
    }

    fn collect_processes(&mut self, refreshed: bool) -> Option<Vec<ProcessSnapshot>> {
        if !self.config.collect_processes {
            return None;
        }
        // Between process refreshes, reuse the last collected list
        if !refreshed {
            return self.cached_processes.clone();
        }

        let processes: Vec<ProcessSnapshot> = self
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

        let selected = select_top_processes(processes, self.config.max_processes);
        self.cached_processes = Some(selected.clone());
        Some(selected)
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

fn by_cpu_then_memory(a: &ProcessSnapshot, b: &ProcessSnapshot) -> std::cmp::Ordering {
    b.cpu_usage_percent
        .total_cmp(&a.cpu_usage_percent)
        .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
}

/// Select up to `max` processes, splitting the budget between the
/// top-by-CPU and top-by-memory rankings: a pure CPU cut would let a busy
/// system (with `max`+ CPU-active processes) push idle memory hogs out
/// entirely, making any downstream "top by memory" view meaningless.
/// The result is sorted by CPU desc, then memory desc.
fn select_top_processes(mut processes: Vec<ProcessSnapshot>, max: usize) -> Vec<ProcessSnapshot> {
    processes.sort_by(by_cpu_then_memory);
    if processes.len() > max {
        let cpu_budget = max / 2;
        let mut rest = processes.split_off(cpu_budget);
        rest.sort_by_key(|p| std::cmp::Reverse(p.memory_bytes));
        processes.extend(rest.into_iter().take(max - cpu_budget));
        processes.sort_by(by_cpu_then_memory);
    }
    processes
}

impl Collector for LocalCollector {
    fn collect(&mut self) -> Result<SystemSnapshot, CollectError> {
        let processes_due = self.config.collect_processes
            && self
                .last_process_refresh
                .is_none_or(|t| t.elapsed() >= self.config.process_refresh_interval);
        self.refresh(processes_due);

        let elapsed = self.last_collect_time.map(|t| t.elapsed());
        self.last_collect_time = Some(Instant::now());

        Ok(SystemSnapshot {
            timestamp: Timestamp::now(),
            host: self.host.clone(),
            cpu: self.collect_cpu(),
            memory: self.collect_memory(),
            disks: self.collect_disks(elapsed),
            networks: self.collect_networks(elapsed),
            processes: self.collect_processes(processes_due),
            load: self.collect_load(),
            extras: HashMap::new(),
        })
    }

    fn config(&self) -> &CollectorConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu: f32, mem: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            name: format!("p{pid}"),
            cmd: String::new(),
            cpu_usage_percent: cpu,
            memory_bytes: mem,
            parent_pid: None,
            status: "Runnable".into(),
        }
    }

    #[test]
    fn memory_hogs_survive_cpu_heavy_selection() {
        // 10 CPU-active processes + 1 idle memory hog, budget 4
        let mut procs: Vec<_> = (0..10).map(|i| proc(i, 10.0 + i as f32, 100)).collect();
        procs.push(proc(99, 0.0, 1_000_000));

        let selected = select_top_processes(procs, 4);
        assert_eq!(selected.len(), 4);
        assert!(
            selected.iter().any(|p| p.pid == 99),
            "idle memory hog must survive selection"
        );
        // Result stays CPU-sorted
        assert!(selected[0].cpu_usage_percent >= selected[1].cpu_usage_percent);
    }

    #[test]
    fn under_budget_keeps_all_sorted_by_cpu() {
        let procs = vec![proc(1, 1.0, 10), proc(2, 5.0, 10), proc(3, 3.0, 10)];
        let selected = select_top_processes(procs, 50);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].pid, 2);
        assert_eq!(selected[1].pid, 3);
    }
}

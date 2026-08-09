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
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use sysinfo::{
    Components, CpuRefreshKind, DiskRefreshKind, Disks, Networks, Pid, ProcessRefreshKind,
    ProcessesToUpdate, System, UpdateKind,
};

use crate::collector::Collector;
use crate::config::CollectorConfig;
use crate::error::CollectError;
use crate::snapshot::{
    CpuSnapshot, DiskSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, NetworkSnapshot,
    ProcessSnapshot, SystemSnapshot, TemperatureSnapshot,
};
use crate::utils::rate::rate_per_sec;

/// Plausible ambient / silicon sensor range; drops firmware garbage such as
/// the -9200°C placeholders sometimes reported on Apple Silicon.
const TEMP_CELSIUS_MIN: f32 = -20.0;
const TEMP_CELSIUS_MAX: f32 = 150.0;

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
    received_errors: u64,
    transmitted_errors: u64,
}

/// Lightweight rank key: select top-N before materializing expensive strings
#[derive(Debug, Clone, Copy)]
struct ProcKey {
    pid: u32,
    cpu: f32,
    mem: u64,
}

pub struct LocalCollector {
    config: CollectorConfig,
    system: System,
    disks: Disks,
    networks: Networks,
    components: Components,
    /// Static host identity (uptime is filled in at collect time)
    host: HostInfo,
    // Internal state: the previous sample used for rate calculation
    last_disk_counters: HashMap<String, DiskCounters>,
    last_net_counters: HashMap<String, NetCounters>,
    last_process_disk_counters: HashMap<u32, DiskCounters>,
    last_disk_storage_refresh: Option<Instant>,
    last_disk_io_refresh: Option<Instant>,
    last_network_refresh: Option<Instant>,
    last_process_refresh: Option<Instant>,
    last_cpu_frequency_refresh: Option<Instant>,
    last_temperature_refresh: Option<Instant>,
    /// Last collected process list, reused between process refreshes when
    /// `process_refresh_interval` is non-zero. Arc so cache hits and
    /// snapshot clones share the same allocation.
    cached_processes: Option<Arc<Vec<ProcessSnapshot>>>,
    /// Last disk / network snapshots, reused between their refreshes when
    /// the corresponding interval is non-zero
    cached_disks: Option<Vec<DiskSnapshot>>,
    cached_networks: Option<Vec<NetworkSnapshot>>,
    /// Last temperature sample, reused between temperature refreshes
    cached_temperatures: Option<Vec<TemperatureSnapshot>>,
}

impl LocalCollector {
    pub fn new(config: CollectorConfig) -> Self {
        let mut system = System::new();
        // Refresh once up front so the first collect has a baseline for CPU usage
        // and an initial frequency reading.
        system.refresh_cpu_specifics(CpuRefreshKind::everything());
        system.refresh_memory();

        let host = HostInfo {
            hostname: System::host_name().unwrap_or_default(),
            os_name: System::name().unwrap_or_default(),
            os_version: System::os_version().unwrap_or_default(),
            kernel_version: System::kernel_version(),
            arch: std::env::consts::ARCH.to_string(),
            uptime_secs: System::uptime(),
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
        // Temperatures are refreshed on their own slow cadence; start empty
        // so the first collect that is due performs a real read
        let components = Components::new();

        Self {
            config,
            system,
            disks,
            networks,
            components,
            host,
            last_disk_counters: HashMap::new(),
            last_net_counters: HashMap::new(),
            last_process_disk_counters: HashMap::new(),
            last_disk_storage_refresh,
            last_disk_io_refresh: None,
            last_network_refresh: None,
            last_process_refresh: None,
            // Frequency was just refreshed above
            last_cpu_frequency_refresh: Some(Instant::now()),
            last_temperature_refresh: None,
            cached_processes: None,
            cached_disks: None,
            cached_networks: None,
            cached_temperatures: None,
        }
    }

    /// Refresh CPU usage every round; frequency only on its own cadence
    fn refresh_cpu(&mut self) {
        let freq_due = self.config.cpu_frequency_refresh_interval.is_zero()
            || self
                .last_cpu_frequency_refresh
                .is_none_or(|t| t.elapsed() >= self.config.cpu_frequency_refresh_interval);
        let mut kind = CpuRefreshKind::nothing().with_cpu_usage();
        if freq_due {
            kind = kind.with_frequency();
            self.last_cpu_frequency_refresh = Some(Instant::now());
        }
        self.system.refresh_cpu_specifics(kind);
    }

    /// Refresh the process table when due.
    ///
    /// Returns the elapsed time since the previous process refresh when
    /// processes were refreshed this round (for process disk IO rates).
    fn refresh_process_table(&mut self, refresh_processes: bool) -> Option<Duration> {
        if !refresh_processes {
            return None;
        }
        let elapsed = self.last_process_refresh.map(|t| t.elapsed());
        // Explicit refresh kind: the `refresh_processes` shortcut does
        // NOT fetch cmd (and wastes time on disk_usage/exe we don't
        // always need). cmd is immutable per process, so OnlyIfNotSet
        // fetches it exactly once per process.
        let mut kind = ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_cmd(UpdateKind::OnlyIfNotSet);
        if self.config.collect_process_disk_io {
            kind = kind.with_disk_usage();
        }
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        self.last_process_refresh = Some(Instant::now());
        elapsed
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

    fn collect_disks(&mut self) -> Option<Vec<DiskSnapshot>> {
        if !self.config.collect_disks {
            return None;
        }
        let due = self.config.disk_io_refresh_interval.is_zero()
            || self
                .last_disk_io_refresh
                .is_none_or(|t| t.elapsed() >= self.config.disk_io_refresh_interval);
        if !due {
            return self.cached_disks.clone();
        }
        // Rate diffs span the time since the previous disk refresh
        let elapsed = self.last_disk_io_refresh.map(|t| t.elapsed());

        // IO counters are cheap; the capacity query costs ~25x more and
        // barely changes, so it runs on its own, slower cadence
        let storage_due = self
            .last_disk_storage_refresh
            .is_none_or(|t| t.elapsed() >= self.config.disk_storage_refresh_interval);
        let mut kind = DiskRefreshKind::nothing().with_io_usage();
        if storage_due {
            kind = kind.with_storage();
            self.last_disk_storage_refresh = Some(Instant::now());
        }
        self.disks.refresh_specifics(true, kind);
        self.last_disk_io_refresh = Some(Instant::now());

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
                kind: disk.kind().to_string(),
                is_removable: disk.is_removable(),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                read_bytes_per_sec,
                write_bytes_per_sec,
            });
        }

        // Replace wholesale so counters of removed disks are pruned automatically
        self.last_disk_counters = counters;

        if self.config.dedupe_disks {
            snapshots = dedupe_disks_by_name(snapshots);
        }

        self.cached_disks = Some(snapshots.clone());
        Some(snapshots)
    }

    fn collect_networks(&mut self) -> Option<Vec<NetworkSnapshot>> {
        if !self.config.collect_networks {
            return None;
        }
        let due = self.config.network_refresh_interval.is_zero()
            || self
                .last_network_refresh
                .is_none_or(|t| t.elapsed() >= self.config.network_refresh_interval);
        if !due {
            return self.cached_networks.clone();
        }
        // Rate diffs span the time since the previous network refresh
        let elapsed = self.last_network_refresh.map(|t| t.elapsed());
        self.networks.refresh(true);
        self.last_network_refresh = Some(Instant::now());

        let mut snapshots = Vec::new();
        let mut counters = HashMap::new();

        for (interface, data) in self.networks.iter() {
            let current = NetCounters {
                received_bytes: data.total_received(),
                transmitted_bytes: data.total_transmitted(),
                received_packets: data.total_packets_received(),
                transmitted_packets: data.total_packets_transmitted(),
                received_errors: data.total_errors_on_received(),
                transmitted_errors: data.total_errors_on_transmitted(),
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
                    received_errors_per_sec: Some(rate_per_sec(
                        prev.received_errors,
                        current.received_errors,
                        elapsed,
                    )),
                    transmitted_errors_per_sec: Some(rate_per_sec(
                        prev.transmitted_errors,
                        current.transmitted_errors,
                        elapsed,
                    )),
                },
                _ => NetworkSnapshot {
                    interface: interface.clone(),
                    received_bytes_per_sec: 0,
                    transmitted_bytes_per_sec: 0,
                    received_packets_per_sec: None,
                    transmitted_packets_per_sec: None,
                    received_errors_per_sec: None,
                    transmitted_errors_per_sec: None,
                },
            };

            counters.insert(interface.clone(), current);
            snapshots.push(snapshot);
        }

        self.last_net_counters = counters;
        self.cached_networks = Some(snapshots.clone());
        Some(snapshots)
    }

    fn collect_processes(
        &mut self,
        refreshed: bool,
        process_elapsed: Option<Duration>,
    ) -> Option<Arc<Vec<ProcessSnapshot>>> {
        if !self.config.collect_processes {
            return None;
        }
        // Between process refreshes, reuse the last collected list
        if !refreshed {
            return self.cached_processes.clone();
        }

        // Pass 1: rank by cpu/mem only — no string materialization yet
        let mut keys: Vec<ProcKey> = Vec::with_capacity(self.system.processes().len());
        let mut disk_counters = if self.config.collect_process_disk_io {
            HashMap::with_capacity(self.system.processes().len())
        } else {
            HashMap::new()
        };

        for p in self.system.processes().values() {
            let pid = p.pid().as_u32();
            keys.push(ProcKey {
                pid,
                cpu: p.cpu_usage(),
                mem: p.memory(),
            });
            if self.config.collect_process_disk_io {
                let usage = p.disk_usage();
                disk_counters.insert(
                    pid,
                    DiskCounters {
                        total_read_bytes: usage.total_read_bytes,
                        total_written_bytes: usage.total_written_bytes,
                    },
                );
            }
        }

        let selected_pids = select_top_pids(keys, self.config.max_processes);

        // Pass 2: materialize only the selected processes
        let mut selected = Vec::with_capacity(selected_pids.len());
        for pid in selected_pids {
            let Some(p) = self.system.process(Pid::from(pid as usize)) else {
                continue;
            };

            let (read_bytes_per_sec, write_bytes_per_sec) = if self.config.collect_process_disk_io {
                match (process_elapsed, self.last_process_disk_counters.get(&pid)) {
                    (Some(elapsed), Some(prev)) => {
                        let current = disk_counters.get(&pid).copied().unwrap_or(DiskCounters {
                            total_read_bytes: 0,
                            total_written_bytes: 0,
                        });
                        (
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
                        )
                    }
                    _ => (None, None),
                }
            } else {
                (None, None)
            };

            selected.push(ProcessSnapshot {
                pid,
                name: p.name().to_string_lossy().to_string(),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|c| c.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                cpu_usage_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
                virtual_memory_bytes: p.virtual_memory(),
                run_time_secs: p.run_time(),
                parent_pid: p.parent().map(|pp| pp.as_u32()),
                status: p.status().to_string(),
                read_bytes_per_sec,
                write_bytes_per_sec,
            });
        }

        // Result stays CPU-sorted (select_top_pids returns that order)
        if self.config.collect_process_disk_io {
            self.last_process_disk_counters = disk_counters;
        } else {
            self.last_process_disk_counters.clear();
        }

        let selected = Arc::new(selected);
        self.cached_processes = Some(Arc::clone(&selected));
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

    fn collect_temperatures(&mut self) -> Option<Vec<TemperatureSnapshot>> {
        if !self.config.collect_temperatures {
            return None;
        }

        let due = self.config.temperature_refresh_interval.is_zero()
            || self
                .last_temperature_refresh
                .is_none_or(|t| t.elapsed() >= self.config.temperature_refresh_interval);

        if !due {
            return self.cached_temperatures.clone();
        }

        self.components.refresh(true);
        let mut sensors: Vec<TemperatureSnapshot> = self
            .components
            .list()
            .iter()
            .filter_map(|c| {
                let celsius = c.temperature().filter(|t| is_plausible_celsius(*t))?;
                let max_celsius = c.max().filter(|t| is_plausible_celsius(*t));
                let critical_celsius = c.critical().filter(|t| is_plausible_celsius(*t));
                Some(TemperatureSnapshot {
                    label: c.label().to_string(),
                    celsius,
                    max_celsius,
                    critical_celsius,
                })
            })
            .collect();
        // Hottest first so UIs can take a prefix without sorting again
        sensors.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));

        self.last_temperature_refresh = Some(Instant::now());
        self.cached_temperatures = Some(sensors.clone());
        Some(sensors)
    }

    fn collect_host(&mut self) -> HostInfo {
        self.host.uptime_secs = System::uptime();
        self.host.clone()
    }
}

/// Reject NaN/inf and firmware garbage outside a sane silicon/ambient range
fn is_plausible_celsius(t: f32) -> bool {
    t.is_finite() && (TEMP_CELSIUS_MIN..=TEMP_CELSIUS_MAX).contains(&t)
}

/// Whether overall load (in logical-core units) should force a process
/// refresh this round.
///
/// `cpu_usage_percent` is sysinfo's global usage (0..=100 over *all* cores).
/// Converting to core-units: `logical_cores * usage / 100`.
fn process_boost_active(
    threshold_cores: Option<f32>,
    cpu_usage_percent: f32,
    logical_cores: u32,
) -> bool {
    let Some(threshold) = threshold_cores else {
        return false;
    };
    if logical_cores == 0 || !threshold.is_finite() || threshold <= 0.0 {
        return false;
    }
    let busy_cores = logical_cores as f32 * (cpu_usage_percent / 100.0);
    busy_cores >= threshold
}

fn by_cpu_then_memory(a: &ProcKey, b: &ProcKey) -> std::cmp::Ordering {
    b.cpu
        .total_cmp(&a.cpu)
        .then_with(|| b.mem.cmp(&a.mem))
        .then_with(|| a.pid.cmp(&b.pid))
}

/// Select up to `max` process pids, splitting the budget between the
/// top-by-CPU and top-by-memory rankings: a pure CPU cut would let a busy
/// system (with `max`+ CPU-active processes) push idle memory hogs out
/// entirely, making any downstream "top by memory" view meaningless.
/// The result is sorted by CPU desc, then memory desc.
fn select_top_pids(mut keys: Vec<ProcKey>, max: usize) -> Vec<u32> {
    keys.sort_by(by_cpu_then_memory);
    if keys.len() > max {
        let cpu_budget = max / 2;
        let mut rest = keys.split_off(cpu_budget);
        rest.sort_by_key(|k| std::cmp::Reverse(k.mem));
        keys.extend(rest.into_iter().take(max - cpu_budget));
        keys.sort_by(by_cpu_then_memory);
    }
    keys.into_iter().map(|k| k.pid).collect()
}

/// Keep one entry per device name, preferring the shortest mount point
/// (collapses APFS synthetic mounts that share a volume).
fn dedupe_disks_by_name(disks: Vec<DiskSnapshot>) -> Vec<DiskSnapshot> {
    let mut best: HashMap<String, DiskSnapshot> = HashMap::new();
    for disk in disks {
        match best.get(&disk.name) {
            Some(existing) if existing.mount_point.len() <= disk.mount_point.len() => {}
            _ => {
                best.insert(disk.name.clone(), disk);
            }
        }
    }
    let mut out: Vec<_> = best.into_values().collect();
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

impl Collector for LocalCollector {
    fn collect(&mut self) -> Result<SystemSnapshot, CollectError> {
        // CPU and memory refresh first (they cost microseconds): the fresh
        // global CPU usage then decides whether the process boost kicks in
        self.refresh_cpu();
        self.system.refresh_memory();

        let logical_cores = self.system.cpus().len() as u32;
        let boost = process_boost_active(
            self.config.process_boost_cpu_cores,
            self.system.global_cpu_usage(),
            logical_cores,
        );
        let processes_due = self.config.collect_processes
            && (boost
                || self
                    .last_process_refresh
                    .is_none_or(|t| t.elapsed() >= self.config.process_refresh_interval));
        let process_elapsed = self.refresh_process_table(processes_due);

        Ok(SystemSnapshot {
            timestamp: Timestamp::now(),
            host: self.collect_host(),
            cpu: self.collect_cpu(),
            memory: self.collect_memory(),
            disks: self.collect_disks(),
            networks: self.collect_networks(),
            processes: self.collect_processes(processes_due, process_elapsed),
            load: self.collect_load(),
            temperatures: self.collect_temperatures(),
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

    fn key(pid: u32, cpu: f32, mem: u64) -> ProcKey {
        ProcKey { pid, cpu, mem }
    }

    #[test]
    fn plausible_celsius_filters_garbage() {
        assert!(is_plausible_celsius(42.0));
        assert!(is_plausible_celsius(0.0));
        assert!(is_plausible_celsius(99.5));
        assert!(!is_plausible_celsius(f32::NAN));
        assert!(!is_plausible_celsius(f32::INFINITY));
        assert!(!is_plausible_celsius(-9201.0));
        assert!(!is_plausible_celsius(200.0));
        assert!(!is_plausible_celsius(-50.0));
    }

    #[test]
    fn boost_uses_core_units_not_raw_percent() {
        // 1.0 core busy: 25% overall on 4 cores, ~1.56% on 64 cores
        assert!(process_boost_active(Some(1.0), 25.0, 4));
        assert!(process_boost_active(Some(1.0), 100.0 / 64.0, 64)); // exactly 1 core
        assert!(!process_boost_active(Some(1.0), 10.0, 4));
        // 15% overall is only ~0.6 cores on 4U, but ~9.6 cores on 64U
        assert!(!process_boost_active(Some(1.0), 15.0, 4));
        assert!(process_boost_active(Some(1.0), 15.0, 64));
        assert!(process_boost_active(Some(2.0), 50.0, 4));
        assert!(!process_boost_active(Some(2.0), 40.0, 4));
        assert!(!process_boost_active(None, 99.0, 64));
        assert!(!process_boost_active(Some(1.0), 100.0, 0));
        assert!(!process_boost_active(Some(0.0), 100.0, 8));
    }

    #[test]
    fn memory_hogs_survive_cpu_heavy_selection() {
        // 10 CPU-active processes + 1 idle memory hog, budget 4
        let mut keys: Vec<_> = (0..10).map(|i| key(i, 10.0 + i as f32, 100)).collect();
        keys.push(key(99, 0.0, 1_000_000));

        let selected = select_top_pids(keys, 4);
        assert_eq!(selected.len(), 4);
        assert!(
            selected.contains(&99),
            "idle memory hog must survive selection"
        );
        // Result stays CPU-sorted: first selected has highest cpu among kept
        // (we only have pids; re-derive via known inputs)
        assert_eq!(selected[0], 9); // cpu 19.0
    }

    #[test]
    fn under_budget_keeps_all_sorted_by_cpu() {
        let keys = vec![key(1, 1.0, 10), key(2, 5.0, 10), key(3, 3.0, 10)];
        let selected = select_top_pids(keys, 50);
        assert_eq!(selected, vec![2, 3, 1]);
    }

    #[test]
    fn dedupe_keeps_shortest_mount() {
        let disks = vec![
            DiskSnapshot {
                name: "disk0".into(),
                mount_point: "/System/Volumes/Data".into(),
                file_system: "apfs".into(),
                kind: "SSD".into(),
                is_removable: false,
                total_bytes: 100,
                available_bytes: 40,
                read_bytes_per_sec: Some(1),
                write_bytes_per_sec: Some(2),
            },
            DiskSnapshot {
                name: "disk0".into(),
                mount_point: "/".into(),
                file_system: "apfs".into(),
                kind: "SSD".into(),
                is_removable: false,
                total_bytes: 100,
                available_bytes: 40,
                read_bytes_per_sec: Some(3),
                write_bytes_per_sec: Some(4),
            },
            DiskSnapshot {
                name: "disk1".into(),
                mount_point: "/Volumes/X".into(),
                file_system: "exfat".into(),
                kind: "HDD".into(),
                is_removable: true,
                total_bytes: 50,
                available_bytes: 10,
                read_bytes_per_sec: None,
                write_bytes_per_sec: None,
            },
        ];
        let out = dedupe_disks_by_name(disks);
        assert_eq!(out.len(), 2);
        let d0 = out.iter().find(|d| d.name == "disk0").unwrap();
        assert_eq!(d0.mount_point, "/");
        assert_eq!(d0.read_bytes_per_sec, Some(3));
        assert!(out.iter().any(|d| d.name == "disk1"));
    }
}

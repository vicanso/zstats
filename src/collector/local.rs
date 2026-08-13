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
    BatterySnapshot, CpuSnapshot, DiskSnapshot, HostInfo, IoTotalsSnapshot, LoadSnapshot,
    MemorySnapshot, NetworkSnapshot, PerfLevelSnapshot, ProcessGroupSnapshot, ProcessSnapshot,
    SystemSnapshot, TemperatureSnapshot,
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
    /// Last per-application aggregates, refreshed on the process cadence
    cached_process_groups: Option<Arc<Vec<ProcessGroupSnapshot>>>,
    /// Static CPU performance-level topology (name, logical cores),
    /// highest-performance level first; empty when the platform has
    /// fewer than two levels. Read once at startup
    perf_levels: Vec<(String, u32)>,
    /// Battery access handle; None when the platform has no battery
    /// support at all. Built once — creating it per collect would be
    /// wasteful
    battery_manager: Option<starship_battery::Manager>,
    last_battery_refresh: Option<Instant>,
    cached_battery: Option<BatterySnapshot>,
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
        let config_collect_battery = config.collect_battery;

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
            cached_process_groups: None,
            perf_levels: detect_perf_levels(),
            battery_manager: config_collect_battery
                .then(starship_battery::Manager::new)
                .and_then(|result| match result {
                    Ok(manager) => Some(manager),
                    Err(e) => {
                        // Not fatal: a machine without battery support
                        // just reports None forever
                        let _ = e;
                        None
                    }
                }),
            last_battery_refresh: None,
            cached_battery: None,
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
        // cmd and user are immutable per process, so OnlyIfNotSet fetches
        // each exactly once per process
        let mut kind = ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet);
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
        // Perf levels average the same sample even when the per-core list
        // is disabled in the snapshot
        let usages: Vec<f32> = cpus.iter().map(|c| c.cpu_usage()).collect();
        let perf_levels = split_perf_levels(&usages, &self.perf_levels);
        let per_core_usage = if self.config.per_core_cpu {
            usages
        } else {
            Vec::new()
        };
        // Frequencies refresh on a slow cadence with usage; 0 means unknown
        let per_core_frequency_mhz: Vec<u64> = cpus.iter().map(|c| c.frequency()).collect();
        let frequency_mhz = per_core_frequency_mhz.iter().copied().find(|f| *f > 0);
        let per_core_frequency_mhz = if frequency_mhz.is_some() {
            per_core_frequency_mhz
        } else {
            Vec::new()
        };
        let brand = cpus
            .iter()
            .map(|c| c.brand().trim())
            .find(|b| !b.is_empty())
            .map(|b| b.to_string());

        CpuSnapshot {
            usage_percent: self.system.global_cpu_usage(),
            per_core_usage,
            logical_cores: cpus.len() as u32,
            physical_cores: System::physical_core_count().map(|n| n as u32),
            frequency_mhz,
            per_core_frequency_mhz,
            brand,
            perf_levels,
        }
    }

    fn collect_memory(&self) -> MemorySnapshot {
        let (compressed_bytes, pressure_level) = memory_pressure();
        let total_bytes = self.system.total_memory();
        let used_bytes = self.system.used_memory();
        let swap_total_bytes = self.system.total_swap();
        let swap_used_bytes = self.system.used_swap();
        MemorySnapshot {
            total_bytes,
            used_bytes,
            available_bytes: self.system.available_memory(),
            swap_total_bytes,
            swap_used_bytes,
            used_percent: ratio_percent(used_bytes, total_bytes),
            swap_used_percent: ratio_percent(swap_used_bytes, swap_total_bytes),
            compressed_bytes,
            pressure_level,
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
            let total_bytes = disk.total_space();
            let available_bytes = disk.available_space();
            let used_bytes = total_bytes.saturating_sub(available_bytes);
            snapshots.push(DiskSnapshot {
                name,
                mount_point,
                file_system: disk.file_system().to_string_lossy().to_string(),
                kind: disk.kind().to_string(),
                is_removable: disk.is_removable(),
                total_bytes,
                available_bytes,
                used_percent: ratio_percent(used_bytes, total_bytes),
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
                // Free: sysinfo fills this from the same task_info call
                // that produces cpu_usage, under the same refresh kind
                cpu_time_ms: p.accumulated_cpu_time(),
                memory_bytes: p.memory(),
                virtual_memory_bytes: p.virtual_memory(),
                run_time_secs: p.run_time(),
                parent_pid: p.parent().map(|pp| pp.as_u32()),
                user_id: p.user_id().map(|uid| uid.to_string()),
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

    /// Aggregate the FULL process table into per-application groups.
    /// Runs on the process cadence, right after a process refresh, and
    /// reuses the cached list between refreshes. One in-memory pass over
    /// the table sysinfo already holds — no extra system calls
    fn collect_process_groups(
        &mut self,
        refreshed: bool,
        process_elapsed: Option<Duration>,
    ) -> Option<Arc<Vec<ProcessGroupSnapshot>>> {
        if !(self.config.collect_processes && self.config.collect_process_groups) {
            return None;
        }
        if !refreshed {
            return self.cached_process_groups.clone();
        }

        let mut table = HashMap::with_capacity(self.system.processes().len());
        for p in self.system.processes().values() {
            let pid = p.pid().as_u32();
            // Per-process IO rates were computed for the selected top-N
            // only; the full table diffs its own counters here so a group
            // total covers every member, not just the ranked ones
            let io = self.config.collect_process_disk_io.then(|| {
                let usage = p.disk_usage();
                (usage.read_bytes, usage.written_bytes)
            });
            table.insert(
                pid,
                ProcNode {
                    parent: p.parent().map(|pp| pp.as_u32()),
                    cpu: p.cpu_usage(),
                    mem: p.memory(),
                    io,
                },
            );
        }

        // Names are materialized only for the selected group roots
        let groups: Vec<ProcessGroupSnapshot> =
            aggregate_process_groups(&table, self.config.max_processes)
                .into_iter()
                .filter_map(|g| {
                    let root = self.system.process(Pid::from(g.root_pid as usize))?;
                    // Bytes since the previous refresh become a rate only
                    // once there is an interval to divide by
                    let rate = |bytes: Option<u64>| match (bytes, process_elapsed) {
                        (Some(b), Some(elapsed)) if !elapsed.is_zero() => {
                            Some((b as f64 / elapsed.as_secs_f64()) as u64)
                        }
                        _ => None,
                    };
                    Some(ProcessGroupSnapshot {
                        root_pid: g.root_pid,
                        name: root.name().to_string_lossy().to_string(),
                        process_count: g.process_count,
                        cpu_usage_percent: g.cpu,
                        memory_bytes: g.mem,
                        read_bytes_per_sec: rate(g.read_bytes),
                        write_bytes_per_sec: rate(g.written_bytes),
                    })
                })
                .collect();

        let groups = Arc::new(groups);
        self.cached_process_groups = Some(Arc::clone(&groups));
        Some(groups)
    }

    /// The full table size at the last process refresh (the snapshot's
    /// process list is truncated to top-N)
    fn collect_total_processes(&self) -> Option<u32> {
        self.config
            .collect_processes
            .then(|| self.system.processes().len() as u32)
    }

    /// Read the main battery on its own cadence, reusing the last
    /// reading in between. `None` on machines without one
    fn collect_battery(&mut self) -> Option<BatterySnapshot> {
        let manager = self.battery_manager.as_ref()?;
        let due = self.config.battery_refresh_interval.is_zero()
            || self
                .last_battery_refresh
                .is_none_or(|t| t.elapsed() >= self.config.battery_refresh_interval);
        if !due {
            return self.cached_battery.clone();
        }
        self.last_battery_refresh = Some(Instant::now());

        // First battery only: multi-battery machines are vanishingly rare
        // outside of some laptops, and a merged total would be a lie
        let battery = manager
            .batteries()
            .ok()?
            .next()?
            .ok()
            .map(|b| BatterySnapshot {
                state: b.state().to_string(),
                charge_percent: b.state_of_charge().value * 100.0,
                health_percent: Some(b.state_of_health().value * 100.0),
                cycle_count: b.cycle_count(),
                // The wrapper reports Kelvin
                temperature_celsius: b.temperature().map(|t| t.value - 273.15),
                power_watts: Some(b.energy_rate().value),
                time_to_full_secs: b.time_to_full().map(|t| t.value as u64),
                time_to_empty_secs: b.time_to_empty().map(|t| t.value as u64),
            });
        self.cached_battery = battery.clone();
        battery
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

/// `used / total * 100`, or 0 when total is zero
fn ratio_percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0) as f32
    }
}

/// Sum optional rates: None when every input is None (or the list is empty /
/// missing); otherwise the sum of present values (missing treated as 0 once
/// at least one rate is known).
fn sum_optional_rates<'a>(rates: impl IntoIterator<Item = Option<&'a u64>>) -> Option<u64> {
    let mut any = false;
    let mut sum = 0u64;
    for &v in rates.into_iter().flatten() {
        any = true;
        sum = sum.saturating_add(v);
    }
    any.then_some(sum)
}

fn io_totals_from(
    disks: Option<&[DiskSnapshot]>,
    networks: Option<&[NetworkSnapshot]>,
) -> IoTotalsSnapshot {
    let (disk_read_bytes_per_sec, disk_write_bytes_per_sec) = match disks {
        Some(disks) => (
            sum_optional_rates(disks.iter().map(|d| d.read_bytes_per_sec.as_ref())),
            sum_optional_rates(disks.iter().map(|d| d.write_bytes_per_sec.as_ref())),
        ),
        None => (None, None),
    };
    let (network_received_bytes_per_sec, network_transmitted_bytes_per_sec) = match networks {
        // Network rates are plain u64 (0 on the first sample), so a present
        // list always yields a total — including 0 on the baseline frame
        Some(networks) => (
            Some(
                networks
                    .iter()
                    .map(|n| n.received_bytes_per_sec)
                    .fold(0u64, u64::saturating_add),
            ),
            Some(
                networks
                    .iter()
                    .map(|n| n.transmitted_bytes_per_sec)
                    .fold(0u64, u64::saturating_add),
            ),
        ),
        None => (None, None),
    };
    IoTotalsSnapshot {
        disk_read_bytes_per_sec,
        disk_write_bytes_per_sec,
        network_received_bytes_per_sec,
        network_transmitted_bytes_per_sec,
    }
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

/// One process table entry for group aggregation
#[derive(Debug, Clone, Copy)]
struct ProcNode {
    parent: Option<u32>,
    cpu: f32,
    mem: u64,
    /// Bytes read/written since the previous process refresh; None when
    /// process disk IO collection is off
    io: Option<(u64, u64)>,
}

/// Aggregated tree totals before name materialization
#[derive(Debug, Clone, Copy)]
struct GroupTotals {
    root_pid: u32,
    process_count: u32,
    cpu: f32,
    mem: u64,
    /// Summed bytes since the previous refresh (turned into rates by the
    /// caller, which knows the elapsed time)
    read_bytes: Option<u64>,
    written_bytes: Option<u64>,
}

/// Pid of init / launchd — the boundary that defines application roots
const INIT_PID: u32 = 1;

/// Resolve every process to the root of its tree (the ancestor whose
/// parent is init/launchd, missing from the table, or absent) and sum
/// CPU/memory per root. Returns at most `max` groups ranked by CPU
/// (memory, then pid, as tie-breaks).
fn aggregate_process_groups(table: &HashMap<u32, ProcNode>, max: usize) -> Vec<GroupTotals> {
    // Memoized root resolution: each pid's chain is walked once
    let mut roots: HashMap<u32, u32> = HashMap::with_capacity(table.len());
    let mut path = Vec::new();
    for &pid in table.keys() {
        let mut current = pid;
        let root = loop {
            if let Some(&known) = roots.get(&current) {
                break known;
            }
            path.push(current);
            match table.get(&current).and_then(|node| node.parent) {
                // `path.len()` guards against ppid cycles (should not
                // happen, but a corrupt table must not hang collection)
                Some(parent)
                    if parent != INIT_PID
                        && parent != current
                        && table.contains_key(&parent)
                        && path.len() < 512 =>
                {
                    current = parent;
                }
                _ => break current,
            }
        };
        for p in path.drain(..) {
            roots.insert(p, root);
        }
    }

    let mut totals: HashMap<u32, GroupTotals> = HashMap::new();
    for (pid, node) in table {
        let root_pid = roots[pid];
        let entry = totals.entry(root_pid).or_insert(GroupTotals {
            root_pid,
            process_count: 0,
            cpu: 0.0,
            mem: 0,
            read_bytes: None,
            written_bytes: None,
        });
        entry.process_count += 1;
        entry.cpu += node.cpu;
        entry.mem = entry.mem.saturating_add(node.mem);
        if let Some((read, written)) = node.io {
            entry.read_bytes = Some(entry.read_bytes.unwrap_or(0).saturating_add(read));
            entry.written_bytes = Some(entry.written_bytes.unwrap_or(0).saturating_add(written));
        }
    }

    let mut groups: Vec<GroupTotals> = totals.into_values().collect();
    groups.sort_by(|a, b| {
        b.cpu
            .total_cmp(&a.cpu)
            .then_with(|| b.mem.cmp(&a.mem))
            .then_with(|| a.root_pid.cmp(&b.root_pid))
    });
    groups.truncate(max);
    groups
}

/// Safe sysctl readers via the `sysctl` crate (the library forbids
/// unsafe code; the FFI lives inside that crate)
#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    use sysctl::{Ctl, CtlValue, Sysctl};

    match Ctl::new(name).ok()?.value().ok()? {
        CtlValue::Int(v) => u64::try_from(v).ok(),
        CtlValue::S64(v) => u64::try_from(v).ok(),
        CtlValue::Long(v) => u64::try_from(v).ok(),
        CtlValue::Uint(v) => Some(u64::from(v)),
        CtlValue::U32(v) => Some(u64::from(v)),
        CtlValue::Ulong(v) => Some(v),
        CtlValue::U64(v) => Some(v),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &str) -> Option<String> {
    use sysctl::{Ctl, CtlValue, Sysctl};

    match Ctl::new(name).ok()?.value().ok()? {
        CtlValue::String(s) => Some(s),
        _ => None,
    }
}

/// Read the CPU performance-level topology (Apple Silicon P/E clusters)
/// once via public sysctls: `hw.nperflevels` + `hw.perflevelN.*`.
/// Returns (name, logical cores) with perflevel0 — the HIGHEST
/// performance level — first; empty on platforms with fewer than two
/// levels (Intel Macs, or any inconsistent reading)
#[cfg(target_os = "macos")]
fn detect_perf_levels() -> Vec<(String, u32)> {
    let Some(n) = sysctl_u64("hw.nperflevels") else {
        return Vec::new();
    };
    if n < 2 {
        return Vec::new();
    }
    let mut levels = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(cores) =
            sysctl_u64(&format!("hw.perflevel{i}.logicalcpu")).and_then(|c| u32::try_from(c).ok())
        else {
            return Vec::new();
        };
        if cores == 0 {
            return Vec::new();
        }
        let name =
            sysctl_string(&format!("hw.perflevel{i}.name")).unwrap_or_else(|| format!("level{i}"));
        levels.push((name, cores));
    }
    levels
}

#[cfg(not(target_os = "macos"))]
fn detect_perf_levels() -> Vec<(String, u32)> {
    Vec::new()
}

/// Memory-pressure signals via public sysctls: the memory compressor's
/// footprint and the kernel's own pressure verdict. Pagein/pageout rates
/// would need Mach `host_statistics64` (unsafe FFI) and are deliberately
/// skipped — the compressor size and the kernel's level are the headline
/// signals
#[cfg(target_os = "macos")]
fn memory_pressure() -> (Option<u64>, Option<u32>) {
    (
        sysctl_u64("vm.compressor_bytes_used"),
        sysctl_u64("kern.memorystatus_vm_pressure_level").and_then(|v| u32::try_from(v).ok()),
    )
}

#[cfg(not(target_os = "macos"))]
fn memory_pressure() -> (Option<u64>, Option<u32>) {
    (None, None)
}

/// Split per-core usage into per-performance-level averages. `levels` is
/// highest-performance first (perflevel0), while core NUMBERING starts
/// with the LOWEST level — E-cores occupy the first indices on Apple
/// Silicon (verified empirically: a background-QoS load lands on
/// cpu0..E-count) — so each level reads from the tail backwards. Returns
/// None unless the topology exactly covers the core list
fn split_perf_levels(per_core: &[f32], levels: &[(String, u32)]) -> Option<Vec<PerfLevelSnapshot>> {
    if levels.len() < 2 {
        return None;
    }
    let total: usize = levels.iter().map(|(_, c)| *c as usize).sum();
    if total != per_core.len() || per_core.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(levels.len());
    let mut end = per_core.len();
    for (name, count) in levels {
        let start = end - *count as usize;
        let cores = &per_core[start..end];
        out.push(PerfLevelSnapshot {
            name: name.clone(),
            logical_cores: *count,
            usage_percent: cores.iter().sum::<f32>() / cores.len() as f32,
        });
        end = start;
    }
    Some(out)
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

        let disks = self.collect_disks();
        let networks = self.collect_networks();
        let io_totals = io_totals_from(disks.as_deref(), networks.as_deref());

        Ok(SystemSnapshot {
            timestamp: Timestamp::now(),
            host: self.collect_host(),
            cpu: self.collect_cpu(),
            memory: self.collect_memory(),
            disks,
            networks,
            processes: self.collect_processes(processes_due, process_elapsed),
            process_groups: self.collect_process_groups(processes_due, process_elapsed),
            total_processes: self.collect_total_processes(),
            battery: self.collect_battery(),
            load: self.collect_load(),
            temperatures: self.collect_temperatures(),
            io_totals,
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

    fn node(parent: Option<u32>, cpu: f32, mem: u64) -> ProcNode {
        ProcNode {
            parent,
            cpu,
            mem,
            io: None,
        }
    }

    #[test]
    fn groups_aggregate_whole_trees_to_launchd_children() {
        // launchd(1) → app(10) → helper(11) → grandchild(12)
        //           → other(20)
        // kernel-ish orphan(30) with a parent outside the table
        let table = HashMap::from([
            (1, node(None, 0.0, 10)),
            (10, node(Some(1), 1.0, 100)),
            (11, node(Some(10), 20.0, 200)),
            (12, node(Some(11), 30.0, 400)),
            (20, node(Some(1), 5.0, 50)),
            (30, node(Some(999), 2.0, 25)),
        ]);

        let groups = aggregate_process_groups(&table, 50);
        let by_root: HashMap<u32, GroupTotals> = groups.iter().map(|g| (g.root_pid, *g)).collect();

        // The app tree sums root + every descendant
        let app = &by_root[&10];
        assert_eq!(app.process_count, 3);
        assert!((app.cpu - 51.0).abs() < 1e-6);
        assert_eq!(app.mem, 700);
        // Single-process trees are their own group
        assert_eq!(by_root[&20].process_count, 1);
        assert_eq!(by_root[&30].process_count, 1);
        assert_eq!(by_root[&1].process_count, 1);
        // Ranked by CPU: the app tree first
        assert_eq!(groups[0].root_pid, 10);
    }

    #[test]
    fn groups_cap_at_max_and_survive_cycles() {
        // A ppid cycle (40 ↔ 41) must not hang; extra trees beyond `max`
        // are dropped lowest-CPU-first
        let table = HashMap::from([
            (10, node(Some(1), 30.0, 0)),
            (20, node(Some(1), 20.0, 0)),
            (30, node(Some(1), 10.0, 0)),
            (40, node(Some(41), 1.0, 0)),
            (41, node(Some(40), 1.0, 0)),
        ]);

        let groups = aggregate_process_groups(&table, 2);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].root_pid, 10);
        assert_eq!(groups[1].root_pid, 20);
    }

    #[test]
    fn perf_levels_read_from_the_tail_backwards() {
        // 8P + 4E on 12 cores: numbering is E-first, so Efficiency reads
        // cpu0-3 and Performance reads cpu4-11
        let levels = vec![
            ("Performance".to_string(), 8),
            ("Efficiency".to_string(), 4),
        ];
        let mut per_core = vec![100.0f32; 4]; // E cores pegged
        per_core.extend(vec![10.0f32; 8]); // P cores light

        let split = split_perf_levels(&per_core, &levels).expect("split");
        assert_eq!(split[0].name, "Performance");
        assert_eq!(split[0].logical_cores, 8);
        assert!((split[0].usage_percent - 10.0).abs() < 1e-4);
        assert_eq!(split[1].name, "Efficiency");
        assert_eq!(split[1].logical_cores, 4);
        assert!((split[1].usage_percent - 100.0).abs() < 1e-4);
    }

    #[test]
    fn perf_levels_require_exact_topology_match() {
        let levels = vec![
            ("Performance".to_string(), 8),
            ("Efficiency".to_string(), 4),
        ];
        // Core count mismatch: stay honest and report nothing
        assert!(split_perf_levels(&[0.0; 10], &levels).is_none());
        // Fewer than two levels: not a heterogeneous CPU
        assert!(split_perf_levels(&[0.0; 8], &[("all".to_string(), 8)]).is_none());
        assert!(split_perf_levels(&[], &[]).is_none());
    }

    #[test]
    fn reparented_orphan_becomes_its_own_root() {
        // A helper whose parent died gets reparented to launchd: it must
        // not merge into an unrelated tree
        let table = HashMap::from([(10, node(Some(1), 5.0, 0)), (50, node(Some(1), 3.0, 0))]);
        let groups = aggregate_process_groups(&table, 50);
        assert_eq!(groups.len(), 2);
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
                used_percent: 60.0,
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
                used_percent: 60.0,
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
                used_percent: 80.0,
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

    #[test]
    fn ratio_percent_handles_zero_total() {
        assert!((ratio_percent(50, 100) - 50.0).abs() < f32::EPSILON);
        assert_eq!(ratio_percent(1, 0), 0.0);
        assert_eq!(ratio_percent(0, 0), 0.0);
    }

    #[test]
    fn io_totals_sums_known_rates() {
        let disks = [
            DiskSnapshot {
                name: "a".into(),
                mount_point: "/".into(),
                file_system: "apfs".into(),
                kind: "SSD".into(),
                is_removable: false,
                total_bytes: 1,
                available_bytes: 0,
                used_percent: 100.0,
                read_bytes_per_sec: Some(10),
                write_bytes_per_sec: Some(20),
            },
            DiskSnapshot {
                name: "b".into(),
                mount_point: "/data".into(),
                file_system: "apfs".into(),
                kind: "SSD".into(),
                is_removable: false,
                total_bytes: 1,
                available_bytes: 0,
                used_percent: 100.0,
                read_bytes_per_sec: Some(5),
                write_bytes_per_sec: None,
            },
        ];
        let nets = [
            NetworkSnapshot {
                interface: "en0".into(),
                received_bytes_per_sec: 100,
                transmitted_bytes_per_sec: 50,
                received_packets_per_sec: None,
                transmitted_packets_per_sec: None,
                received_errors_per_sec: None,
                transmitted_errors_per_sec: None,
            },
            NetworkSnapshot {
                interface: "lo0".into(),
                received_bytes_per_sec: 3,
                transmitted_bytes_per_sec: 3,
                received_packets_per_sec: None,
                transmitted_packets_per_sec: None,
                received_errors_per_sec: None,
                transmitted_errors_per_sec: None,
            },
        ];
        let totals = io_totals_from(Some(&disks), Some(&nets));
        assert_eq!(totals.disk_read_bytes_per_sec, Some(15));
        assert_eq!(totals.disk_write_bytes_per_sec, Some(20));
        assert_eq!(totals.network_received_bytes_per_sec, Some(103));
        assert_eq!(totals.network_transmitted_bytes_per_sec, Some(53));
        assert!(io_totals_from(None, None).disk_read_bytes_per_sec.is_none());
    }
}

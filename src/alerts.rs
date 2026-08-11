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

//! Per-process alert evaluation: sustained high CPU or a large share of
//! total memory over the last minute.
//!
//! This module is the pure rule engine — thresholds, rolling windows,
//! cooldowns, and the metrics-recording criteria — shared by every
//! frontend so CLI and GUI agree on what "over threshold" means. How an
//! [`AlertEvent`] reaches the user (desktop notification, in-app banner)
//! is deliberately the frontend's business, as is persisting the returned
//! [`MetricRecord`]s (see [`crate::records`]).
//!
//! Only sustained behavior alerts, in two tiers sharing one threshold
//! and one cooldown per (pid, kind):
//! - acute/runaway: 1-minute average CPU ≥ [`ACUTE_FACTOR`] × the
//!   process's effective threshold — fast notification;
//! - chronic: 5-minute average CPU (or memory share) at or above the
//!   effective threshold — "quietly always busy", confirmed by
//!   persistence; self-limiting bursts never get that far.
//!
//! The volume-capacity rule is different in kind: capacity is a slow
//! state, so it alerts on the upward crossing of the threshold and
//! re-arms only after dropping [`DISK_REARM_MARGIN`] below — one
//! notification per episode, no cooldown, per-mount overrides.
//!
//! System memory pressure is driven by the kernel's own verdict
//! (`pressure_level`) but requires PERSISTENCE, not just a crossing: a
//! memory-heavy machine sits at warning as its steady state and would
//! otherwise alert all day. One alert per severity per episode
//! (worsening escalates, lingering never nags), re-armed at normal.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::records::MetricRecord;
use crate::rolling::ProcessWindows;
use crate::settings::AlertsConfig;
use crate::snapshot::SystemSnapshot;

/// Fast window: the acute (runaway) rule and metrics recording
const WINDOW: Duration = Duration::from_secs(60);
/// The fast window only fires once it is this full: startup blips never
/// alert
const MIN_SPAN: Duration = Duration::from_secs(50);
/// The chronic window spans this many acute windows: "chronic" is defined
/// as persistence across several acute periods, not as an independent
/// absolute — tuning [`WINDOW`] keeps the relationship
const SLOW_FACTOR: u64 = 5;
/// Slow window: the chronic rules (5 minutes with the current acute
/// window). One acute period cannot distinguish a self-limiting burst
/// (an editor re-indexing, a page load) from a process that is genuinely
/// always busy — several can
const SLOW_WINDOW: Duration = Duration::from_secs(WINDOW.as_secs() * SLOW_FACTOR);
const SLOW_MIN_SPAN: Duration = Duration::from_secs(MIN_SPAN.as_secs() * SLOW_FACTOR);
/// Acute rule: 1-minute average at or above this many times the
/// process's EFFECTIVE threshold (override or base — so template'd apps
/// like browsers get 3x their raised bar, not 3x the default, and a
/// video call does not read as a runaway)
const ACUTE_FACTOR: f64 = 3.0;
/// How often qualifying processes are reported as metric records
const RECORD_EVERY: Duration = Duration::from_secs(60);

/// Builtin defaults when the config file leaves a value unset
const DEFAULT_CPU_PERCENT: f32 = 30.0;
const DEFAULT_MEMORY_FRACTION: f64 = 0.25;
const DEFAULT_DISK_FRACTION: f32 = 0.90;
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(600);

/// The disk rule is crossing-based, not windowed (capacity is a slow
/// state, not a rate): one alert when a volume crosses its threshold,
/// then silence until it drops this far below and crosses again — no
/// "still full" nagging, no cooldown involved
const DISK_REARM_MARGIN: f64 = 0.05;

/// Builtin CPU-override template: resident apps that legitimately sustain
/// high CPU in normal use. Applied below the user's own overrides (a
/// same-name user entry always wins) unless `[alerts] template = false`.
///
/// Only long-running apps qualify — short-lived bursts (a single rustc)
/// exit before the alert window fills and never notify anyway. Values:
/// 100 = "interrupt me only at a full core" (interactive apps), 200 =
/// "multi-core is its job" (IDE indexers, VM/container hosts), 0 =
/// "never interrupt, still record" (self-started long jobs and periodic
/// macOS system work). Keep the reference copy in `config.example.toml`
/// in sync with this list.
pub const TEMPLATE_CPU_OVERRIDES: &[(&str, f32)] = &[
    // Browsers: page loads / video / heavy-JS sites run for minutes
    ("Google Chrome", 100.0),
    ("Google Chrome Helper (Renderer)", 100.0),
    ("Google Chrome Helper (GPU)", 100.0),
    ("Microsoft Edge Helper (Renderer)", 100.0),
    ("Brave Browser Helper (Renderer)", 100.0),
    ("Arc Helper (Renderer)", 100.0),
    ("firefox", 100.0),
    ("plugin-container", 100.0),
    ("com.apple.WebKit.WebContent", 100.0),
    ("com.apple.WebKit.GPU", 100.0),
    // Electron / Chromium desktop apps: same engine, same bursts
    ("Slack Helper (Renderer)", 100.0),
    ("Discord Helper (Renderer)", 100.0),
    ("Notion Helper (Renderer)", 100.0),
    ("Obsidian Helper (Renderer)", 100.0),
    ("Figma Helper (Renderer)", 100.0),
    ("Spotify Helper", 100.0),
    ("QQ Helper (Renderer)", 100.0),
    ("Lark Helper (Renderer)", 100.0),
    ("DingTalk", 100.0),
    ("WeCom Helper (Renderer)", 100.0),
    // Editors / IDEs / language servers: re-index after every edit burst
    ("Cursor Helper (Renderer)", 100.0),
    ("Code Helper (Renderer)", 100.0),
    ("gopls", 100.0),
    ("rust-analyzer", 100.0),
    ("clangd", 100.0),
    ("sourcekit-lsp", 100.0),
    ("SourceKitService", 200.0),
    ("idea", 200.0),
    ("goland", 200.0),
    ("clion", 200.0),
    // VMs / containers: guest workload is supposed to use cores
    ("com.docker.backend", 200.0),
    ("com.docker.virtualization", 200.0),
    ("OrbStack Helper", 200.0),
    ("qemu-system-aarch64", 200.0),
    ("prl_vm_app", 200.0),
    ("vmware-vmx", 200.0),
    // Terminals rendering streaming AI output, and the AI CLIs themselves
    ("ghostty", 100.0),
    ("iTerm2", 100.0),
    ("kitty", 100.0),
    ("alacritty", 100.0),
    ("claude", 100.0),
    // Calls / screen sharing: video encode while the call lasts
    ("zoom.us", 100.0),
    ("WeChat", 100.0),
    ("Microsoft Teams", 100.0),
    ("TencentMeeting", 100.0),
    ("avconferenced", 100.0),
    // Long jobs you started yourself: never actionable, history only
    ("OBS", 0.0),
    ("ffmpeg", 0.0),
    ("HandBrake", 0.0),
    ("Final Cut Pro", 0.0),
    ("ollama", 0.0),
    // macOS periodic system work: not actionable, history only
    ("mds_stores", 0.0),
    ("mdworker_shared", 0.0),
    ("photoanalysisd", 0.0),
    ("mediaanalysisd", 0.0),
    ("backupd", 0.0),
    ("syspolicyd", 0.0),
    ("installd", 0.0),
    ("softwareupdated", 0.0),
    ("kernel_task", 0.0),
    ("wdavdaemon", 0.0),
    // Sustained ≥100% here usually means some app is spamming redraws
    ("WindowServer", 100.0),
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AlertKind {
    Cpu,
    Memory,
}

/// An alert that should reach the user now
#[derive(Debug)]
pub struct AlertEvent {
    pub message: String,
}

/// Result of one evaluation pass
#[derive(Default)]
pub struct Evaluation {
    pub events: Vec<AlertEvent>,
    /// Data points for the daily metrics history (non-empty only on
    /// minute boundaries); persisting them is the caller's business
    pub records: Vec<MetricRecord>,
}

/// A rule threshold: a default plus per-process-name overrides
/// (case-insensitive). An override of `None` disables the rule for that
/// process — so a legitimately busy app (e.g. a terminal rendering
/// streaming AI output) can get a higher bar without being blind to a
/// real runaway.
pub struct Thresholds<T> {
    default: Option<T>,
    overrides: Vec<(String, Option<T>)>,
}

impl<T: Copy> Thresholds<T> {
    pub fn new(default: Option<T>) -> Self {
        Self {
            default,
            overrides: Vec::new(),
        }
    }

    pub fn with_override(mut self, name: String, value: Option<T>) -> Self {
        self.overrides.push((name, value));
        self
    }

    /// The effective threshold for a process: first name match wins,
    /// otherwise the default. `None` = rule disabled for this process
    pub fn for_process(&self, name: &str) -> Option<T> {
        for (n, value) in &self.overrides {
            if n.eq_ignore_ascii_case(name) {
                return *value;
            }
        }
        self.default
    }

    /// The base threshold, ignoring per-process overrides — this is what
    /// metrics recording uses
    pub fn base(&self) -> Option<T> {
        self.default
    }

    pub fn any_enabled(&self) -> bool {
        self.default.is_some() || self.overrides.iter().any(|(_, v)| v.is_some())
    }
}

/// The currently effective thresholds (config file over builtin defaults)
pub struct ActiveThresholds {
    pub cpu: Thresholds<f32>,
    pub memory: Thresholds<f64>,
    /// Volume used-capacity fractions, override key = mount point
    pub disk: Thresholds<f32>,
    /// Kernel memory-pressure level at which to start alerting (2 =
    /// warning, 4 = critical); None disables the rule
    pub pressure: Option<u32>,
    /// Per (process, rule) re-alert cooldown while a condition persists
    pub cooldown: Duration,
}

impl ActiveThresholds {
    /// Merge the `[alerts]` config section over the builtin template over
    /// the builtin defaults (CPU 30%, memory 25%, cooldown 10m). Per-name
    /// precedence: a user override always beats the same-name
    /// [`TEMPLATE_CPU_OVERRIDES`] entry, template entries fill the names
    /// the user did not configure, and every other process falls through
    /// to the base value. `[alerts] template = false` drops the template
    /// layer entirely. A configured 0 disables the rule; a 0 override
    /// disables it for that process only
    pub fn from_config(file: &AlertsConfig) -> Self {
        let cpu_default = match file.cpu {
            Some(p) if p > 0.0 => Some(p),
            Some(_) => None,
            None => Some(DEFAULT_CPU_PERCENT),
        };
        let mem_default = match file.mem {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(DEFAULT_MEMORY_FRACTION),
        };

        // User entries are pushed first: Thresholds::for_process returns
        // the first name match, so they shadow the template below
        let mut cpu = Thresholds::new(cpu_default);
        for (name, pct) in &file.cpu_overrides {
            cpu = cpu.with_override(name.clone(), (*pct > 0.0).then_some(*pct));
        }
        if file.template.unwrap_or(true) {
            for (name, pct) in TEMPLATE_CPU_OVERRIDES {
                cpu = cpu.with_override((*name).to_string(), (*pct > 0.0).then_some(*pct));
            }
        }
        let mut memory = Thresholds::new(mem_default);
        for (name, pct) in &file.mem_overrides {
            memory = memory.with_override(name.clone(), (*pct > 0.0).then_some(*pct / 100.0));
        }
        let disk_default = match file.disk {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(DEFAULT_DISK_FRACTION),
        };
        let mut disk = Thresholds::new(disk_default);
        for (mount, pct) in &file.disk_overrides {
            disk = disk.with_override(mount.clone(), (*pct > 0.0).then_some(*pct / 100.0));
        }

        Self {
            cpu,
            memory,
            disk,
            pressure: file
                .pressure
                .unwrap_or(crate::settings::PressureAlert::Warning)
                .level(),
            cooldown: file.cooldown.unwrap_or(DEFAULT_COOLDOWN),
        }
    }

    pub fn any_enabled(&self) -> bool {
        self.cpu.any_enabled()
            || self.memory.any_enabled()
            || self.disk.any_enabled()
            || self.pressure.is_some()
    }
}

/// The alert rule engine: rolling windows, cooldown state, and the
/// once-a-minute recording gate. Feed it every snapshot via
/// [`AlertEngine::evaluate`]; thresholds are passed per call so the
/// caller can hot-reload them without touching accumulated state
#[derive(Default)]
pub struct AlertEngine {
    /// 1-minute windows: acute rule + recording
    fast: Option<ProcessWindows>,
    /// 5-minute windows: chronic rules
    slow: Option<ProcessWindows>,
    last_alert: HashMap<(u32, AlertKind), Instant>,
    /// Mounts that already fired the disk alert and have not yet dropped
    /// [`DISK_REARM_MARGIN`] below their threshold
    disk_disarmed: std::collections::HashSet<String>,
    /// When the current above-normal pressure episode began; None while
    /// the kernel reports normal
    pressure_since: Option<Instant>,
    /// Highest memory-pressure level already alerted this episode; reset
    /// to 0 when the kernel reports normal again. One alert per severity
    /// per episode — worsening escalates, lingering does not nag
    pressure_alerted: u32,
    /// When the last metrics-recording pass ran (`None` = record on the
    /// next evaluation)
    last_record: Option<Instant>,
}

impl AlertEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate one snapshot observed at `now` (injectable for tests).
    ///
    /// Two tiers per CPU rule, sharing one cooldown per (pid, kind) so
    /// the same situation never notifies twice:
    /// - acute: 1-minute average ≥ [`ACUTE_FACTOR`] × the effective
    ///   threshold — a runaway, worth interrupting fast;
    /// - chronic: 5-minute average ≥ the effective threshold — the
    ///   "quietly always busy" case, interrupted only once persistence
    ///   is established (self-limiting bursts never get that far).
    ///
    /// Memory has no acute tier by design: transient legitimate spikes
    /// are exactly what must not alert. Recording stays on the 1-minute
    /// window with BASE thresholds — an override or the template
    /// suppresses the notification, not the data point
    pub fn evaluate(
        &mut self,
        now: Instant,
        snapshot: &SystemSnapshot,
        active: &ActiveThresholds,
    ) -> Evaluation {
        let mut evaluation = Evaluation::default();

        // Disk capacity runs first: it does not depend on process data.
        // Crossing detection with hysteresis — see DISK_REARM_MARGIN
        if let Some(disks) = &snapshot.disks {
            self.disk_disarmed
                .retain(|mount| disks.iter().any(|d| d.mount_point == *mount));
            for d in disks {
                let Some(threshold) = active.disk.for_process(&d.mount_point) else {
                    continue;
                };
                if d.total_bytes == 0 {
                    continue;
                }
                let used =
                    d.total_bytes.saturating_sub(d.available_bytes) as f64 / d.total_bytes as f64;
                if self.disk_disarmed.contains(&d.mount_point) {
                    if used < f64::from(threshold) - DISK_REARM_MARGIN {
                        self.disk_disarmed.remove(&d.mount_point);
                    }
                } else if used >= f64::from(threshold) {
                    self.disk_disarmed.insert(d.mount_point.clone());
                    evaluation.events.push(AlertEvent {
                        message: format!(
                            "disk {} is {:.0}% full — {:.1} GiB free of {:.1} GiB",
                            d.mount_point,
                            used * 100.0,
                            d.available_bytes as f64 / f64::from(1 << 30),
                            d.total_bytes as f64 / f64::from(1 << 30),
                        ),
                    });
                }
            }
        }

        // System memory pressure: the kernel's own verdict, but only once
        // it PERSISTS. A memory-heavy machine sits at warning as its
        // normal state and crosses the line all day, and a build spike is
        // over in a minute — neither is actionable, and plain crossing
        // detection reports both. compressed_bytes deliberately has no
        // rule of its own: the kernel already folds compressor growth
        // into this level
        if let Some(threshold) = active.pressure
            && let Some(level) = snapshot.memory.pressure_level
        {
            if level <= 1 {
                self.pressure_since = None;
                self.pressure_alerted = 0;
            } else {
                let since = *self.pressure_since.get_or_insert(now);
                let elapsed = now.duration_since(since);
                // More severe means faster notification, mirroring the
                // acute/chronic split of the per-process rules
                let required = if level >= 4 { WINDOW } else { SLOW_WINDOW };
                if level >= threshold && level > self.pressure_alerted && elapsed >= required {
                    self.pressure_alerted = level;
                    let label = if level >= 4 { "critical" } else { "warning" };
                    let compressed = snapshot
                        .memory
                        .compressed_bytes
                        .map(|b| format!(", compressor {:.1} GiB", b as f64 / f64::from(1 << 30)))
                        .unwrap_or_default();
                    evaluation.events.push(AlertEvent {
                        message: format!(
                            "system memory pressure: {label} for {} min — \
                             swap {:.1}/{:.1} GiB{compressed}",
                            elapsed.as_secs() / 60,
                            snapshot.memory.swap_used_bytes as f64 / f64::from(1 << 30),
                            snapshot.memory.swap_total_bytes as f64 / f64::from(1 << 30),
                        ),
                    });
                }
            }
        }

        let Some(processes) = snapshot.processes.as_deref() else {
            tracing::debug!("alert evaluation skipped: snapshot has no processes");
            return evaluation;
        };
        let total_memory = snapshot.memory.total_bytes;
        let mem_share = |avg_bytes: f64| {
            if total_memory > 0 {
                avg_bytes / total_memory as f64
            } else {
                0.0
            }
        };

        // Once a minute, qualifying processes become metric records
        let record_due = self
            .last_record
            .is_none_or(|t| now.duration_since(t) >= RECORD_EVERY);
        if record_due {
            self.last_record = Some(now);
        }

        let fast_stats = self
            .fast
            .get_or_insert_with(|| ProcessWindows::new(WINDOW))
            .record(now, processes);
        let slow_stats = self
            .slow
            .get_or_insert_with(|| ProcessWindows::new(SLOW_WINDOW))
            .record(now, processes);

        // Forget cooldowns of processes that disappeared
        self.last_alert
            .retain(|(pid, _), _| fast_stats.contains_key(pid));

        for p in processes {
            let (Some(fast), Some(slow)) = (fast_stats.get(&p.pid), slow_stats.get(&p.pid)) else {
                continue;
            };
            if p.cpu_usage_percent > 20.0 {
                tracing::debug!(
                    pid = p.pid,
                    name = %p.name,
                    cpu = p.cpu_usage_percent,
                    samples = fast.samples,
                    span_secs = fast.span.as_secs(),
                    slow_span_secs = slow.span.as_secs(),
                    "alert window state"
                );
            }

            if let Some(threshold) = active.cpu.for_process(&p.name) {
                let acute =
                    fast.span >= MIN_SPAN && fast.cpu_avg >= ACUTE_FACTOR * f64::from(threshold);
                let chronic = slow.span >= SLOW_MIN_SPAN && slow.cpu_avg >= f64::from(threshold);
                if (acute || chronic)
                    && cooldown_elapsed(
                        &mut self.last_alert,
                        (p.pid, AlertKind::Cpu),
                        now,
                        active.cooldown,
                    )
                {
                    let message = if acute {
                        format!(
                            "{} (pid {}) runaway: averaged {:.0}% CPU over the last minute \
                             ({ACUTE_FACTOR:.0}x the {threshold:.0}% threshold)",
                            p.name, p.pid, fast.cpu_avg
                        )
                    } else {
                        format!(
                            "{} (pid {}) averaged {:.0}% CPU over the last {} minutes \
                             (threshold {threshold:.0}%)",
                            p.name,
                            p.pid,
                            slow.cpu_avg,
                            SLOW_WINDOW.as_secs() / 60
                        )
                    };
                    evaluation.events.push(AlertEvent { message });
                }
            }

            if let Some(fraction) = active.memory.for_process(&p.name)
                && total_memory > 0
                && slow.span >= SLOW_MIN_SPAN
                && mem_share(slow.memory_avg_bytes) >= fraction
                && cooldown_elapsed(
                    &mut self.last_alert,
                    (p.pid, AlertKind::Memory),
                    now,
                    active.cooldown,
                )
            {
                evaluation.events.push(AlertEvent {
                    message: format!(
                        "{} (pid {}) averaged {:.1} GiB — {:.0}% of total memory — \
                         over the last {} minutes",
                        p.name,
                        p.pid,
                        slow.memory_avg_bytes / f64::from(1 << 30),
                        mem_share(slow.memory_avg_bytes) * 100.0,
                        SLOW_WINDOW.as_secs() / 60
                    ),
                });
            }

            // Metrics recording: 1-minute window, BASE thresholds only
            if record_due && fast.span >= MIN_SPAN {
                let over_cpu = active
                    .cpu
                    .base()
                    .is_some_and(|threshold| fast.cpu_avg >= f64::from(threshold));
                let over_mem = active.memory.base().is_some_and(|fraction| {
                    total_memory > 0 && mem_share(fast.memory_avg_bytes) >= fraction
                });
                if over_cpu || over_mem {
                    evaluation.records.push(MetricRecord {
                        timestamp: snapshot.timestamp,
                        pid: p.pid,
                        name: p.name.clone(),
                        cpu_avg_percent: fast.cpu_avg as f32,
                        memory_avg_bytes: fast.memory_avg_bytes as u64,
                        memory_share_percent: (mem_share(fast.memory_avg_bytes) * 100.0) as f32,
                    });
                }
            }
        }
        evaluation
    }
}

/// True (and records `now`) when the cooldown for this key has elapsed
fn cooldown_elapsed(
    last_alert: &mut HashMap<(u32, AlertKind), Instant>,
    key: (u32, AlertKind),
    now: Instant,
    cooldown: Duration,
) -> bool {
    if let Some(previous) = last_alert.get(&key)
        && now.duration_since(*previous) < cooldown
    {
        return false;
    }
    last_alert.insert(key, now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{CpuSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, ProcessSnapshot};

    fn proc(pid: u32, cpu: f32, mem: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            name: format!("p{pid}"),
            cmd: String::new(),
            cpu_usage_percent: cpu,
            memory_bytes: mem,
            virtual_memory_bytes: mem,
            run_time_secs: 0,
            parent_pid: None,
            status: "Runnable".into(),
            read_bytes_per_sec: None,
            write_bytes_per_sec: None,
        }
    }

    fn snapshot(processes: Vec<ProcessSnapshot>, total_memory: u64) -> SystemSnapshot {
        SystemSnapshot {
            timestamp: jiff::Timestamp::from_second(0).expect("valid"),
            host: HostInfo {
                hostname: String::new(),
                os_name: String::new(),
                os_version: String::new(),
                kernel_version: None,
                arch: String::new(),
                uptime_secs: 0,
                labels: Default::default(),
            },
            cpu: CpuSnapshot {
                usage_percent: 0.0,
                per_core_usage: Vec::new(),
                logical_cores: 0,
                physical_cores: None,
                frequency_mhz: None,
                perf_levels: None,
            },
            memory: MemorySnapshot {
                total_bytes: total_memory,
                used_bytes: 0,
                available_bytes: 0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                compressed_bytes: None,
                pressure_level: None,
            },
            disks: None,
            networks: None,
            processes: Some(std::sync::Arc::new(processes)),
            process_groups: None,
            total_processes: None,
            battery: None,
            load: LoadSnapshot {
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            temperatures: None,
            extras: Default::default(),
        }
    }

    fn thresholds(
        cpu: Thresholds<f32>,
        memory: Thresholds<f64>,
        cooldown: Duration,
    ) -> ActiveThresholds {
        ActiveThresholds {
            cpu,
            memory,
            disk: Thresholds::new(None),
            pressure: None,
            cooldown,
        }
    }

    fn disk(mount: &str, total: u64, available: u64) -> crate::snapshot::DiskSnapshot {
        crate::snapshot::DiskSnapshot {
            name: "disk0".into(),
            mount_point: mount.into(),
            file_system: "apfs".into(),
            kind: "SSD".into(),
            is_removable: false,
            total_bytes: total,
            available_bytes: available,
            read_bytes_per_sec: None,
            write_bytes_per_sec: None,
        }
    }

    fn snapshot_disks(disks: Vec<crate::snapshot::DiskSnapshot>) -> SystemSnapshot {
        let mut s = snapshot(Vec::new(), 100);
        s.disks = Some(disks);
        s
    }

    /// Feed one sample every 2s for `steps` steps starting at
    /// `base + offset`, returning how many alerts fired
    fn drive(
        engine: &mut AlertEngine,
        active: &ActiveThresholds,
        base: Instant,
        offset: Duration,
        steps: u64,
        make_proc: impl Fn(u64) -> ProcessSnapshot,
        total_memory: u64,
    ) -> usize {
        let mut fired = 0;
        for i in 0..steps {
            let now = base + offset + Duration::from_secs(2 * i);
            fired += engine
                .evaluate(now, &snapshot(vec![make_proc(i)], total_memory), active)
                .events
                .len();
        }
        fired
    }

    #[test]
    fn chronic_cpu_needs_full_slow_window_then_respects_cooldown() {
        // 150% is below the acute bar (3 x 100 = 300), so only the chronic
        // rule applies: nothing before the 5-minute window is ~full, then
        // one alert, then the cooldown suppresses re-fires
        let active = thresholds(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;

        for i in 0..160u64 {
            let now = base + Duration::from_secs(2 * i);
            fired += engine
                .evaluate(now, &snapshot(vec![proc(1, 150.0, 0)], 100), &active)
                .events
                .len();
            if 2 * i < 250 {
                assert_eq!(fired, 0, "no chronic alert before t=250s (t={}s)", 2 * i);
            }
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn acute_runaway_fires_within_a_minute() {
        // 150% >= 3 x 30: the acute tier fires as soon as the 1-minute
        // window is ~full, long before the chronic window could
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let events = engine
                .evaluate(now, &snapshot(vec![proc(1, 150.0, 0)], 100), &active)
                .events;
            if 2 * i < 50 {
                assert!(events.is_empty(), "no alert before t=50s (t={}s)", 2 * i);
            }
            messages.extend(events.into_iter().map(|e| e.message));
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("runaway"), "got: {}", messages[0]);
    }

    #[test]
    fn acute_scales_with_the_effective_override() {
        // ghostty's override (100) raises its acute bar to 300: the same
        // 150% that is a runaway for a default process stays silent for it
        let cpu = Thresholds::new(Some(30.0)).with_override("ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 150.0, 0);
            ghostty.name = "ghostty".into();
            let other = proc(2, 150.0, 0);
            messages.extend(
                engine
                    .evaluate(now, &snapshot(vec![ghostty, other], 100), &active)
                    .events
                    .into_iter()
                    .map(|e| e.message),
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("p2 (pid 2)"), "got: {}", messages[0]);
    }

    #[test]
    fn acute_and_chronic_share_one_cooldown() {
        // A runaway alerts acutely at ~50s; the chronic rule matching the
        // same process at ~250s must NOT re-notify within the cooldown
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 150.0, 0),
            100,
        );
        assert_eq!(fired, 1);
    }

    #[test]
    fn cpu_below_threshold_never_fires() {
        let active = thresholds(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 60.0, 0),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn sustained_memory_fires_once_after_slow_window_fills() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        // 30% of total for 320s: one alert (memory is chronic-only, after
        // the 250s span), then cooldown
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 0.0, 30),
            100,
        );
        assert_eq!(fired, 1);
    }

    #[test]
    fn transient_memory_spike_does_not_alert() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        // A 10s spike at 60% averages away inside the 5-minute window
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |i| proc(1, 0.0, if i < 5 { 60 } else { 5 }),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn per_process_override_beats_default() {
        let cpu = Thresholds::new(Some(30.0)).with_override("Ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();

        // Two processes both averaging 60%: the default-threshold one fires
        // (chronic, 60 >= 30), the overridden one stays quiet (60 < 100);
        // name match ignores case
        let mut messages = Vec::new();
        for i in 0..160u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 60.0, 0);
            ghostty.name = "ghostty".into();
            let other = proc(2, 60.0, 0);
            for event in engine
                .evaluate(now, &snapshot(vec![ghostty, other], 100), &active)
                .events
            {
                messages.push(event.message);
            }
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("p2 (pid 2)"), "got: {}", messages[0]);
    }

    #[test]
    fn zero_override_disables_rule_for_that_process() {
        let cpu = Thresholds::new(Some(30.0)).with_override("p1".into(), None);
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 150.0, 0),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn custom_cooldown_controls_realert_rate() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(20),
        );
        let mut engine = AlertEngine::new();
        // Sustained hog for 320s at a 2s cadence with a 20s cooldown:
        // fires at 250s (slow window full), then at 270s, 290s, 310s
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 0.0, 30),
            100,
        );
        assert_eq!(fired, 4);
    }

    #[test]
    fn dead_process_resets_state() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        // Sustained hog fires once (chronic, at ~250s)
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::ZERO,
                130,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
        // pid 1 disappears for one round: its history and cooldown reset
        let _ = engine.evaluate(
            base + Duration::from_secs(260),
            &snapshot(vec![proc(2, 0.0, 1)], 100),
            &active,
        );
        // The same pid reappears as a hog: refills its slow window, then
        // fires again well within the original cooldown span
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::from_secs(262),
                130,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
    }

    #[test]
    fn records_qualifying_process_once_per_minute() {
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut records = Vec::new();

        // 80s at a 2s cadence: the t=0 recording pass has an empty window,
        // the t=60s pass has a full one — exactly one record
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            records.extend(
                engine
                    .evaluate(now, &snapshot(vec![proc(1, 150.0, 40)], 100), &active)
                    .records,
            );
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 1);
        assert_eq!(records[0].name, "p1");
        assert!((records[0].cpu_avg_percent - 150.0).abs() < 0.5);
        assert!((records[0].memory_share_percent - 40.0).abs() < 0.5);
    }

    #[test]
    fn recording_uses_base_threshold_despite_override() {
        // Override silences ghostty's notification at 60% (< 100), but the
        // base threshold (30) still records it
        let cpu = Thresholds::new(Some(30.0)).with_override("ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut records = Vec::new();
        let mut events = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 60.0, 0);
            ghostty.name = "ghostty".into();
            let evaluation = engine.evaluate(now, &snapshot(vec![ghostty], 100), &active);
            events.extend(evaluation.events);
            records.extend(evaluation.records);
        }
        assert!(events.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "ghostty");
    }

    #[test]
    fn below_base_thresholds_records_nothing() {
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let evaluation = engine.evaluate(now, &snapshot(vec![proc(1, 10.0, 5)], 100), &active);
            assert!(evaluation.records.is_empty());
        }
    }

    #[test]
    fn disk_crossing_alerts_once_then_rearms_below_margin() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(Some(0.90)),
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut step = |i: u64, used_pct: u64| {
            engine
                .evaluate(
                    base + Duration::from_secs(2 * i),
                    &snapshot_disks(vec![disk("/", 1000, 1000 - used_pct * 10)]),
                    &active,
                )
                .events
        };

        assert!(step(0, 85).is_empty(), "below threshold");
        let fired = step(1, 91);
        assert_eq!(fired.len(), 1, "crossing fires once");
        assert!(fired[0].message.contains("disk / is 91% full"));
        assert!(step(2, 93).is_empty(), "still full: no nagging");
        assert!(step(3, 87).is_empty(), "above the re-arm line: still quiet");
        assert!(step(4, 84).is_empty(), "re-armed silently");
        assert_eq!(step(5, 92).len(), 1, "second crossing fires again");
    }

    #[test]
    fn disk_override_disables_backup_volume() {
        // A Time-Machine-style volume runs full by design: override 0
        // silences it while the system volume still alerts
        let disk_rule = Thresholds::new(Some(0.90)).with_override("/Volumes/Backup".into(), None);
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: disk_rule,
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let events = engine
            .evaluate(
                Instant::now(),
                &snapshot_disks(vec![disk("/Volumes/Backup", 1000, 10), disk("/", 1000, 50)]),
                &active,
            )
            .events;
        assert_eq!(events.len(), 1);
        assert!(events[0].message.contains("disk / is 95% full"));
    }

    #[test]
    fn pressure_alerts_on_warning_escalates_to_critical_then_rearms() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut step = |i: u64, level: u32| {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(level);
            engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events
        };

        // Steps are 2s apart and the episode starts at step 1 (t=2s), so
        // the warning's SLOW_WINDOW (300s) is served at step 151
        assert!(step(0, 1).is_empty(), "normal is silent");
        for i in 1..151 {
            assert!(
                step(i, 2).is_empty(),
                "transient warning must stay silent (t={}s)",
                2 * i
            );
        }
        let warn = step(151, 2);
        assert_eq!(warn.len(), 1, "sustained warning alerts once");
        assert!(
            warn[0].message.contains("warning for 5 min"),
            "got: {}",
            warn[0].message
        );
        assert!(step(152, 2).is_empty(), "lingering warning does not nag");
        let crit = step(153, 4);
        assert_eq!(crit.len(), 1, "worsening escalates without waiting again");
        assert!(crit[0].message.contains("critical"));
        assert!(
            step(154, 2).is_empty(),
            "improving within the episode is silent"
        );
        assert!(step(155, 1).is_empty(), "back to normal re-arms silently");
        // A new episode must serve its own persistence requirement
        assert!(step(156, 2).is_empty(), "new episode restarts the clock");
        assert_eq!(step(306, 2).len(), 1, "and alerts once sustained again");
    }

    #[test]
    fn critical_pressure_alerts_faster_than_warning() {
        // An episode that starts at critical needs only WINDOW (60s),
        // not the 5 minutes a warning must persist
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;
        for i in 0..40u64 {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(4);
            let events = engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events;
            if 2 * i < 60 {
                assert!(events.is_empty(), "not before 60s (t={}s)", 2 * i);
            }
            fired += events.len();
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn critical_only_setting_ignores_sustained_warning() {
        // The memory-heavy case: warning is this machine's normal state,
        // so only critical should ever interrupt
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            pressure: Some(4),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;
        for i in 0..200u64 {
            let mut s = snapshot(Vec::new(), 100);
            // Warning for 400s straight, then critical
            s.memory.pressure_level = Some(if i < 200 { 2 } else { 4 });
            fired += engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events
                .len();
        }
        assert_eq!(fired, 0, "sustained warning never alerts at critical-only");
    }

    #[test]
    fn pressure_rule_can_be_disabled_and_skips_absent_data() {
        let mut active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let mut s = snapshot(Vec::new(), 100);
        s.memory.pressure_level = Some(4);
        assert!(
            engine
                .evaluate(Instant::now(), &s, &active)
                .events
                .is_empty(),
            "disabled rule stays silent even at critical"
        );

        // Enabled but the platform reports no level (non-macOS): silent
        active.pressure = Some(2);
        let s = snapshot(Vec::new(), 100);
        assert!(
            engine
                .evaluate(Instant::now(), &s, &active)
                .events
                .is_empty()
        );
    }

    #[test]
    fn from_config_uses_builtin_defaults_when_nothing_is_set() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        assert_eq!(merged.cpu.for_process("any"), Some(30.0));
        assert_eq!(merged.memory.for_process("any"), Some(0.25));
        assert_eq!(merged.disk.for_process("/"), Some(0.90));
        assert_eq!(merged.pressure, Some(2));
        assert_eq!(merged.cooldown, Duration::from_secs(600));
        assert!(merged.any_enabled());

        // The key's original boolean form still parses, and the
        // three-way form maps to kernel levels
        let off: AlertsConfig = toml::from_str("pressure = false").expect("bool form");
        assert_eq!(ActiveThresholds::from_config(&off).pressure, None);
        let warn: AlertsConfig = toml::from_str("pressure = true").expect("bool form");
        assert_eq!(ActiveThresholds::from_config(&warn).pressure, Some(2));
        let crit: AlertsConfig = toml::from_str(r#"pressure = "critical""#).expect("name form");
        assert_eq!(ActiveThresholds::from_config(&crit).pressure, Some(4));
        assert!(toml::from_str::<AlertsConfig>(r#"pressure = "loud""#).is_err());
    }

    #[test]
    fn from_config_disk_zero_disables_and_overrides_map() {
        let mut file = AlertsConfig {
            disk: Some(0.0),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.disk.for_process("/"), None);

        file.disk = Some(80.0);
        file.disk_overrides.insert("/Volumes/Backup".into(), 0.0);
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.disk.for_process("/"), Some(0.80));
        assert_eq!(merged.disk.for_process("/Volumes/Backup"), None);
    }

    #[test]
    fn from_config_file_beats_builtin() {
        let mut file = AlertsConfig {
            cpu: Some(50.0),
            cooldown: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        file.cpu_overrides.insert("ghostty".into(), 100.0);

        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.cpu.for_process("other"), Some(50.0));
        assert_eq!(merged.cpu.for_process("Ghostty"), Some(100.0));
        assert_eq!(merged.cpu.base(), Some(50.0));
        assert_eq!(merged.cooldown, Duration::from_secs(300));
    }

    #[test]
    fn template_fills_names_the_user_did_not_configure() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        // Template entries are active out of the box
        assert_eq!(merged.cpu.for_process("Google Chrome"), Some(100.0));
        assert_eq!(merged.cpu.for_process("SourceKitService"), Some(200.0));
        // A template value of 0 disables the rule for that process
        assert_eq!(merged.cpu.for_process("mds_stores"), None);
        // Everything not templated falls through to the base default
        assert_eq!(merged.cpu.for_process("some-random-app"), Some(30.0));
    }

    #[test]
    fn user_override_shadows_template_entry() {
        let mut file = AlertsConfig::default();
        file.cpu_overrides.insert("ghostty".into(), 60.0);
        file.cpu_overrides.insert("mds_stores".into(), 50.0);
        let merged = ActiveThresholds::from_config(&file);

        // User values win over same-name template entries (case-insensitive)
        assert_eq!(merged.cpu.for_process("Ghostty"), Some(60.0));
        assert_eq!(merged.cpu.for_process("mds_stores"), Some(50.0));
        // Untouched template entries stay active
        assert_eq!(merged.cpu.for_process("gopls"), Some(100.0));
    }

    #[test]
    fn template_can_be_disabled_entirely() {
        let file = AlertsConfig {
            template: Some(false),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&file);
        // Template names fall back to the base default like any other app
        assert_eq!(merged.cpu.for_process("Google Chrome"), Some(30.0));
        assert_eq!(merged.cpu.for_process("mds_stores"), Some(30.0));
    }

    #[test]
    fn from_config_zero_disables() {
        let mut file = AlertsConfig {
            mem: Some(0.0),
            ..Default::default()
        };
        file.cpu_overrides.insert("chrome".into(), 0.0);

        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.memory.for_process("any"), None);
        assert_eq!(merged.cpu.for_process("chrome"), None);
        assert_eq!(merged.cpu.for_process("other"), Some(30.0));
    }
}

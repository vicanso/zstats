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

//! Per-process alerting for the daemon: desktop notifications when a
//! process sustains high CPU over the last minute or holds a large share
//! of total memory. Alerting deliberately lives in the CLI layer — the
//! core library only delivers snapshots.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use zstats::{MetricSink, SinkError, SystemSnapshot, async_trait};

use crate::settings;

/// Averaging window for both rules
const WINDOW: Duration = Duration::from_secs(60);
/// Rules only fire once the window is this full: short-lived processes and
/// transient spikes (a brief legitimate memory burst, a startup CPU blip)
/// never alert — only sustained behavior does
const MIN_SPAN: Duration = Duration::from_secs(50);
/// Stat the config file's mtime once every this many collects (about a
/// minute at the default 2s interval) and hot-reload thresholds on change
const RELOAD_CHECK_EVERY: u32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AlertKind {
    Cpu,
    Memory,
}

#[derive(Debug)]
pub struct AlertEvent {
    pub message: String,
}

/// One recorded sample: (when, cpu %, memory bytes)
type Sample = (Instant, f32, u64);

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

    fn for_process(&self, name: &str) -> Option<T> {
        for (n, value) in &self.overrides {
            if n.eq_ignore_ascii_case(name) {
                return *value;
            }
        }
        self.default
    }

    fn any_enabled(&self) -> bool {
        self.default.is_some() || self.overrides.iter().any(|(_, v)| v.is_some())
    }
}

/// Alert settings the CLI provided at startup, kept so hot reloads can
/// re-run the "CLI flags > config file > builtin defaults" merge
#[derive(Default)]
pub struct AlertCliOptions {
    /// Outer None = flag not given; inner None = disabled with 0
    pub cpu: Option<Option<f32>>,
    pub cpu_overrides: Vec<(String, Option<f32>)>,
    pub mem: Option<Option<f64>>,
    pub mem_overrides: Vec<(String, Option<f64>)>,
    pub cooldown: Option<Duration>,
}

/// The currently effective, merged settings
struct ActiveThresholds {
    cpu: Thresholds<f32>,
    memory: Thresholds<f64>,
    /// Per (process, rule) re-alert cooldown while a condition persists
    cooldown: Duration,
}

/// Merge CLI options over the config file over builtin defaults
fn merge_thresholds(cli: &AlertCliOptions, file: &settings::AlertsConfig) -> ActiveThresholds {
    let cpu_default = match cli.cpu {
        Some(explicit) => explicit,
        None => match file.cpu {
            Some(p) if p > 0.0 => Some(p),
            Some(_) => None,
            None => Some(30.0),
        },
    };
    let mem_default = match cli.mem {
        Some(explicit) => explicit,
        None => match file.mem {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(0.25),
        },
    };
    let cooldown = cli
        .cooldown
        .unwrap_or_else(|| file.cooldown.unwrap_or(Duration::from_secs(600)));

    // CLI overrides go first: Thresholds returns the first name match
    let mut cpu = Thresholds::new(cpu_default);
    for (name, value) in &cli.cpu_overrides {
        cpu = cpu.with_override(name.clone(), *value);
    }
    for (name, pct) in &file.cpu_overrides {
        cpu = cpu.with_override(name.clone(), (*pct > 0.0).then_some(*pct));
    }
    let mut memory = Thresholds::new(mem_default);
    for (name, value) in &cli.mem_overrides {
        memory = memory.with_override(name.clone(), *value);
    }
    for (name, pct) in &file.mem_overrides {
        memory = memory.with_override(name.clone(), (*pct > 0.0).then_some(*pct / 100.0));
    }

    ActiveThresholds {
        cpu,
        memory,
        cooldown,
    }
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(settings::path())
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Hot-reload bookkeeping
struct ReloadState {
    rounds_since_check: u32,
    last_mtime: Option<SystemTime>,
}

/// Sink that watches snapshots and fires desktop notifications.
///
/// Both rules evaluate 1-minute averages, so only sustained behavior
/// alerts:
/// - CPU rule: average CPU (single-core units, same as the PROC table)
///   at or above the threshold.
/// - Memory rule: average share of total system memory at or above the
///   given fraction.
pub struct AlertSink {
    active: Mutex<ActiveThresholds>,
    /// Present when built from CLI options: enables config hot reload
    cli: Option<AlertCliOptions>,
    reload: Mutex<ReloadState>,
    history: Mutex<HashMap<u32, VecDeque<Sample>>>,
    last_alert: Mutex<HashMap<(u32, AlertKind), Instant>>,
}

impl AlertSink {
    /// Fixed thresholds, no config-file reloading (test constructor)
    #[cfg(test)]
    fn new(cpu: Thresholds<f32>, memory: Thresholds<f64>, cooldown: Duration) -> Self {
        Self {
            active: Mutex::new(ActiveThresholds {
                cpu,
                memory,
                cooldown,
            }),
            cli: None,
            reload: Mutex::new(ReloadState {
                rounds_since_check: 0,
                last_mtime: None,
            }),
            history: Mutex::new(HashMap::new()),
            last_alert: Mutex::new(HashMap::new()),
        }
    }

    /// Build from CLI options merged over the config file; the file is
    /// then re-checked every [`RELOAD_CHECK_EVERY`] collects and reloaded
    /// on mtime change
    pub fn from_options(cli: AlertCliOptions, file: &settings::AlertsConfig) -> Self {
        let active = merge_thresholds(&cli, file);
        Self {
            active: Mutex::new(active),
            cli: Some(cli),
            reload: Mutex::new(ReloadState {
                rounds_since_check: 0,
                last_mtime: config_mtime(),
            }),
            history: Mutex::new(HashMap::new()),
            last_alert: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.cpu.any_enabled() || active.memory.any_enabled()
    }

    /// Every [`RELOAD_CHECK_EVERY`] rounds, stat the config file and
    /// re-merge thresholds when its mtime changed. A file that fails to
    /// parse keeps the previous settings (unlike startup, a running
    /// daemon should not die over a config typo)
    fn maybe_reload(&self) {
        let Some(cli) = &self.cli else {
            return;
        };

        let mut reload = self.reload.lock().unwrap_or_else(|e| e.into_inner());
        reload.rounds_since_check += 1;
        if reload.rounds_since_check < RELOAD_CHECK_EVERY {
            return;
        }
        reload.rounds_since_check = 0;

        let mtime = config_mtime();
        if std::env::var_os("ZSTATS_ALERT_DEBUG").is_some() {
            eprintln!(
                "[alert-debug] reload check: mtime={mtime:?} last={:?}",
                reload.last_mtime
            );
        }
        if mtime == reload.last_mtime {
            return;
        }
        reload.last_mtime = mtime;

        match settings::load() {
            Ok(file) => {
                *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
                    merge_thresholds(cli, &file.alerts);
                eprintln!(
                    "[zstats alert] reloaded config from {}",
                    settings::path().display()
                );
            }
            Err(e) => {
                eprintln!("[zstats alert] config reload failed, keeping previous settings: {e}");
            }
        }
    }

    /// Record the snapshot and return the alerts that should fire now.
    /// Separated from `write` (with an injectable clock) for testability
    fn record_and_evaluate(&self, now: Instant, snapshot: &SystemSnapshot) -> Vec<AlertEvent> {
        let debug = std::env::var_os("ZSTATS_ALERT_DEBUG").is_some();
        let Some(processes) = snapshot.processes.as_deref() else {
            if debug {
                eprintln!("[alert-debug] snapshot has no processes");
            }
            return Vec::new();
        };
        let total_memory = snapshot.memory.total_bytes;
        let mut events = Vec::new();

        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let mut last_alert = self.last_alert.lock().unwrap_or_else(|e| e.into_inner());

        // Forget processes that disappeared
        let current: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        history.retain(|pid, _| current.contains(pid));
        last_alert.retain(|(pid, _), _| current.contains(pid));

        for p in processes {
            let samples = history.entry(p.pid).or_default();
            samples.push_back((now, p.cpu_usage_percent, p.memory_bytes));
            while let Some((t, ..)) = samples.front() {
                if now.duration_since(*t) > WINDOW {
                    samples.pop_front();
                } else {
                    break;
                }
            }

            // Both rules require a reasonably full window
            let span = samples
                .front()
                .map(|(t, ..)| now.duration_since(*t))
                .unwrap_or(Duration::ZERO);
            if debug && p.cpu_usage_percent > 20.0 {
                eprintln!(
                    "[alert-debug] pid={} name={} cpu={:.1} samples={} span={}s",
                    p.pid,
                    p.name,
                    p.cpu_usage_percent,
                    samples.len(),
                    span.as_secs()
                );
            }
            if span < MIN_SPAN {
                continue;
            }
            let count = samples.len() as f64;

            if let Some(threshold) = active.cpu.for_process(&p.name) {
                let avg = samples.iter().map(|(_, c, _)| f64::from(*c)).sum::<f64>() / count;
                if avg >= f64::from(threshold)
                    && cooldown_elapsed(
                        &mut last_alert,
                        (p.pid, AlertKind::Cpu),
                        now,
                        active.cooldown,
                    )
                {
                    events.push(AlertEvent {
                        message: format!(
                            "{} (pid {}) averaged {avg:.0}% CPU over the last minute \
                             (threshold {threshold:.0}%)",
                            p.name, p.pid
                        ),
                    });
                }
            }

            if let Some(fraction) = active.memory.for_process(&p.name)
                && total_memory > 0
            {
                let avg_bytes = samples.iter().map(|(.., m)| *m as f64).sum::<f64>() / count;
                let share = avg_bytes / total_memory as f64;
                if share >= fraction
                    && cooldown_elapsed(
                        &mut last_alert,
                        (p.pid, AlertKind::Memory),
                        now,
                        active.cooldown,
                    )
                {
                    events.push(AlertEvent {
                        message: format!(
                            "{} (pid {}) averaged {:.1} GiB — {:.0}% of total memory — \
                             over the last minute",
                            p.name,
                            p.pid,
                            avg_bytes / f64::from(1 << 30),
                            share * 100.0
                        ),
                    });
                }
            }
        }
        events
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

/// Fire a desktop notification, best-effort: macOS uses osascript (whose
/// Script Editor identity reliably displays banners — notification-center
/// APIs masquerading as Terminal.app get silently dropped on this setup),
/// other unixes try notify-send. `spawn` doesn't block; failures are
/// ignored — the daemon's stdio is detached anyway
fn send_notification(message: &str) {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"zstats\"",
            escape_applescript(message)
        );
        let _ = Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = Command::new("notify-send")
            .arg("zstats")
            .arg(message)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(target_os = "macos")]
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[async_trait]
impl MetricSink for AlertSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        self.maybe_reload();
        for event in self.record_and_evaluate(Instant::now(), snapshot) {
            // Also log to stderr: visible when serve runs in the foreground,
            // and it separates "rule fired" from "notification displayed"
            // when debugging delivery issues
            eprintln!("[zstats alert] {}", event.message);
            send_notification(&event.message);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "alerts"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zstats::{CpuSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, ProcessSnapshot};

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
            },
            memory: MemorySnapshot {
                total_bytes: total_memory,
                used_bytes: 0,
                available_bytes: 0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: None,
            networks: None,
            processes: Some(std::sync::Arc::new(processes)),
            load: LoadSnapshot {
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            temperatures: None,
            extras: Default::default(),
        }
    }

    #[test]
    fn cpu_alert_needs_full_window_then_respects_cooldown() {
        let sink = AlertSink::new(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let base = Instant::now();
        let mut fired = 0;

        // 2s cadence for 80s: nothing before the 50s window fills, then one
        // alert, then the cooldown suppresses re-fires
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let events = sink.record_and_evaluate(now, &snapshot(vec![proc(1, 150.0, 0)], 100));
            fired += events.len();
            if 2 * i < 50 {
                assert_eq!(fired, 0, "no alert before the window fills (t={}s)", 2 * i);
            }
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn cpu_below_threshold_never_fires() {
        let sink = AlertSink::new(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let base = Instant::now();
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let events = sink.record_and_evaluate(now, &snapshot(vec![proc(1, 60.0, 0)], 100));
            assert!(events.is_empty());
        }
    }

    /// Feed `sink` one sample every 2s for `steps` steps starting at
    /// `base + offset`, returning how many alerts fired
    fn drive(
        sink: &AlertSink,
        base: Instant,
        offset: Duration,
        steps: u64,
        make_proc: impl Fn(u64) -> ProcessSnapshot,
        total_memory: u64,
    ) -> usize {
        let mut fired = 0;
        for i in 0..steps {
            let now = base + offset + Duration::from_secs(2 * i);
            fired += sink
                .record_and_evaluate(now, &snapshot(vec![make_proc(i)], total_memory))
                .len();
        }
        fired
    }

    #[test]
    fn sustained_memory_fires_once_after_window_fills() {
        let sink = AlertSink::new(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let base = Instant::now();
        // 30% of total for 80s: one alert (after the 50s window), then cooldown
        let fired = drive(&sink, base, Duration::ZERO, 40, |_| proc(1, 0.0, 30), 100);
        assert_eq!(fired, 1);
    }

    #[test]
    fn transient_memory_spike_does_not_alert() {
        let sink = AlertSink::new(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let base = Instant::now();
        // 10s at 60% of total, then back to 5%: the 1-minute average never
        // reaches 25%, so a legitimate short burst stays silent
        let fired = drive(
            &sink,
            base,
            Duration::ZERO,
            40,
            |i| proc(1, 0.0, if i < 5 { 60 } else { 5 }),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn per_process_override_beats_default() {
        let cpu = Thresholds::new(Some(30.0)).with_override("Ghostty".into(), Some(100.0));
        let sink = AlertSink::new(cpu, Thresholds::new(None), Duration::from_secs(600));
        let base = Instant::now();

        // Two processes both averaging 60%: the default-threshold one fires,
        // the overridden one stays quiet (60 < 100); name match ignores case
        let mut messages = Vec::new();
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 60.0, 0);
            ghostty.name = "ghostty".into();
            let other = proc(2, 60.0, 0);
            for event in sink.record_and_evaluate(now, &snapshot(vec![ghostty, other], 100)) {
                messages.push(event.message);
            }
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("p2 (pid 2)"), "got: {}", messages[0]);
    }

    #[test]
    fn zero_override_disables_rule_for_that_process() {
        let cpu = Thresholds::new(Some(30.0)).with_override("p1".into(), None);
        let sink = AlertSink::new(cpu, Thresholds::new(None), Duration::from_secs(600));
        let fired = drive(
            &sink,
            Instant::now(),
            Duration::ZERO,
            40,
            |_| proc(1, 150.0, 0),
            100,
        );
        assert_eq!(fired, 0);
    }

    fn empty_cli() -> AlertCliOptions {
        AlertCliOptions {
            cpu: None,
            cpu_overrides: Vec::new(),
            mem: None,
            mem_overrides: Vec::new(),
            cooldown: None,
        }
    }

    #[test]
    fn merge_uses_builtin_defaults_when_nothing_is_set() {
        let merged = merge_thresholds(&empty_cli(), &settings::AlertsConfig::default());
        assert_eq!(merged.cpu.for_process("any"), Some(30.0));
        assert_eq!(merged.memory.for_process("any"), Some(0.25));
        assert_eq!(merged.cooldown, Duration::from_secs(600));
    }

    #[test]
    fn merge_file_beats_builtin_and_cli_beats_file() {
        let mut file = settings::AlertsConfig {
            cpu: Some(50.0),
            cooldown: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        file.cpu_overrides.insert("ghostty".into(), 100.0);

        // File over builtin
        let merged = merge_thresholds(&empty_cli(), &file);
        assert_eq!(merged.cpu.for_process("other"), Some(50.0));
        assert_eq!(merged.cpu.for_process("Ghostty"), Some(100.0));
        assert_eq!(merged.cooldown, Duration::from_secs(300));

        // CLI over file, including a same-name override
        let cli = AlertCliOptions {
            cpu: Some(Some(40.0)),
            cpu_overrides: vec![("ghostty".into(), Some(120.0))],
            cooldown: Some(Duration::from_secs(60)),
            ..empty_cli()
        };
        let merged = merge_thresholds(&cli, &file);
        assert_eq!(merged.cpu.for_process("other"), Some(40.0));
        assert_eq!(merged.cpu.for_process("ghostty"), Some(120.0));
        assert_eq!(merged.cooldown, Duration::from_secs(60));
    }

    #[test]
    fn merge_zero_in_file_disables() {
        let mut file = settings::AlertsConfig {
            mem: Some(0.0),
            ..Default::default()
        };
        file.cpu_overrides.insert("chrome".into(), 0.0);

        let merged = merge_thresholds(&empty_cli(), &file);
        assert_eq!(merged.memory.for_process("any"), None);
        assert_eq!(merged.cpu.for_process("chrome"), None);
        assert_eq!(merged.cpu.for_process("other"), Some(30.0));
    }

    #[test]
    fn custom_cooldown_controls_realert_rate() {
        let sink = AlertSink::new(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(10),
        );
        let base = Instant::now();
        // Sustained hog for 80s at a 2s cadence with a 10s cooldown:
        // fires at 50s (window full), then again at 60s and 70s
        let fired = drive(&sink, base, Duration::ZERO, 40, |_| proc(1, 0.0, 30), 100);
        assert_eq!(fired, 3);
    }

    #[test]
    fn dead_process_resets_state() {
        let sink = AlertSink::new(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let base = Instant::now();
        // Sustained hog fires once
        assert_eq!(
            drive(&sink, base, Duration::ZERO, 30, |_| proc(1, 0.0, 30), 100),
            1
        );
        // pid 1 disappears for one round: its history and cooldown reset
        let _ = sink.record_and_evaluate(
            base + Duration::from_secs(60),
            &snapshot(vec![proc(2, 0.0, 1)], 100),
        );
        // The same pid reappears as a hog: refills its window, then fires
        // again well within the original cooldown span
        assert_eq!(
            drive(
                &sink,
                base,
                Duration::from_secs(62),
                30,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
    }
}

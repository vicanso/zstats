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

//! One-call frontend loop: collect, evaluate, smooth, persist.
//!
//! The pieces below it ([`crate::collector`], [`crate::alerts`],
//! [`crate::rolling`], [`crate::records`], [`crate::settings`]) are
//! deliberately separate so a frontend can take only what it needs.
//! [`Monitor`] is the assembly every frontend would otherwise write by
//! hand:
//!
//! ```no_run
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut monitor = zstats::Monitor::new(zstats::settings::default_dir())?;
//! loop {
//!     let tick = monitor.tick()?;
//!     for alert in &tick.alerts {
//!         // Deliver however this frontend wants: in-app banner,
//!         // system notification, a log line
//!         println!("{}", alert.summary());
//!     }
//!     // tick.snapshot / tick.process_stats drive the views
//!     std::thread::sleep(std::time::Duration::from_secs(2));
//! }
//! # }
//! ```
//!
//! Synchronous on purpose: collection is a blocking call, so a GUI runs
//! this on its own thread or timer and keeps full control over which
//! thread touches the UI. Nothing here spawns threads or calls back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::alerts::{ActiveThresholds, AlertEngine, AlertEvent};
use crate::collector::{Collector, LocalCollector};
use crate::error::{CollectError, ConfigError};
use crate::records::MetricRecord;
use crate::rolling::{ProcessStats, ProcessWindows};
use crate::settings::FileConfig;
use crate::snapshot::SystemSnapshot;

/// Averaging window for [`Tick::process_stats`] — the smoothed values a
/// process table should rank by, so rows do not reshuffle every tick
const DISPLAY_WINDOW: Duration = Duration::from_secs(60);

/// What one [`Monitor::tick`] produced
pub struct Tick {
    /// The raw sample
    pub snapshot: SystemSnapshot,
    /// Alerts that fired this tick; delivering them is the caller's job
    pub alerts: Vec<AlertEvent>,
    /// Rolling per-pid averages over the last minute, for display
    pub process_stats: HashMap<u32, ProcessStats>,
    /// Data points appended to the daily history this tick (empty except
    /// on minute boundaries); already persisted, exposed for the caller
    /// that wants to react to them live
    pub records: Vec<MetricRecord>,
}

/// Collector + alert engine + rolling averages + history persistence,
/// wired together and driven one [`Monitor::tick`] at a time.
pub struct Monitor {
    config_dir: PathBuf,
    settings: FileConfig,
    collector: LocalCollector,
    engine: AlertEngine,
    thresholds: ActiveThresholds,
    windows: ProcessWindows,
}

impl Monitor {
    /// Load `<config_dir>/config.toml` and build everything from it. A
    /// missing file means builtin defaults; a malformed one is an error,
    /// so a typo never silently discards the user's settings
    pub fn new(config_dir: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let config_dir = config_dir.into();
        let settings = crate::settings::load(&config_dir)?;
        Self::with_settings(config_dir, settings)
    }

    /// Same, for a frontend that already holds the parsed settings (a
    /// settings panel, say) and does not want them re-read. Still
    /// fallible: `<config-dir>/template.toml` is read here, and a
    /// template that failed to load is a rule set that silently did not
    /// apply
    pub fn with_settings(
        config_dir: impl Into<PathBuf>,
        settings: FileConfig,
    ) -> Result<Self, ConfigError> {
        let config_dir = config_dir.into();
        let collector_config = settings.collector.clone().unwrap_or_default();
        let template = crate::settings::load_template(&config_dir)?;
        let thresholds = ActiveThresholds::from_config_with_template(&settings.alerts, &template);
        Ok(Self {
            config_dir,
            settings,
            collector: LocalCollector::new(collector_config),
            engine: AlertEngine::new(),
            thresholds,
            windows: ProcessWindows::new(DISPLAY_WINDOW),
        })
    }

    /// Collect once, evaluate the alert rules, update the rolling
    /// averages, and append any due history records.
    ///
    /// Rate metrics (disk, network, per-process IO) need a previous
    /// sample to diff against, so the FIRST tick reports them as
    /// `None`/0 — that is the collector's contract, not a fault here.
    ///
    /// A failure to persist history is logged and swallowed: a full disk
    /// should not stop monitoring.
    pub fn tick(&mut self) -> Result<Tick, CollectError> {
        let snapshot = self.collector.collect()?;
        let now = Instant::now();
        let evaluation = self.engine.evaluate(now, &snapshot, &self.thresholds);
        let process_stats = snapshot
            .processes
            .as_deref()
            .map(|processes| self.windows.record(now, processes))
            .unwrap_or_default();

        if !evaluation.records.is_empty() {
            let today = jiff::Zoned::now().date();
            if let Err(e) = crate::records::append(&self.config_dir, today, &evaluation.records) {
                tracing::warn!("failed to write metrics history: {e}");
            }
        }

        Ok(Tick {
            snapshot,
            alerts: evaluation.events,
            process_stats,
            records: evaluation.records,
        })
    }

    /// Re-read the config file and the template, and rebuild the alert
    /// thresholds, keeping every accumulated window and cooldown intact
    /// — call this after a settings panel writes.
    ///
    /// Only the `[alerts]` section takes effect: `[collector]` settings
    /// are baked into a running collector whose rate baselines would be
    /// lost, so apply those by building a new [`Monitor`]. Nothing is
    /// applied unless BOTH files load, so a broken one never leaves half
    /// the thresholds updated.
    pub fn reload_settings(&mut self) -> Result<(), ConfigError> {
        let settings = crate::settings::load(&self.config_dir)?;
        let template = crate::settings::load_template(&self.config_dir)?;
        self.thresholds = ActiveThresholds::from_config_with_template(&settings.alerts, &template);
        self.settings = settings;
        Ok(())
    }

    /// The settings this monitor is running with
    pub fn settings(&self) -> &FileConfig {
        &self.settings
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Whether any alert rule is active; a frontend can skip its
    /// notification plumbing entirely when this is false
    pub fn alerts_enabled(&self) -> bool {
        self.thresholds.any_enabled()
    }

    /// Read back the daily history, e.g. to chart the last week
    pub fn history(
        &self,
        from: jiff::civil::Date,
        to: jiff::civil::Date,
    ) -> std::io::Result<Vec<MetricRecord>> {
        crate::records::read_range(&self.config_dir, from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zstats-monitor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn ticks_produce_snapshots_and_smoothed_stats() {
        let dir = test_dir("tick");
        let mut monitor = Monitor::new(&dir).expect("new");

        let first = monitor.tick().expect("first tick");
        assert!(first.snapshot.memory.total_bytes > 0);
        // Rates need a baseline: the first tick has none
        for disk in first.snapshot.disks.as_deref().unwrap_or_default() {
            assert!(disk.read_bytes_per_sec.is_none());
        }

        std::thread::sleep(Duration::from_millis(200));
        let second = monitor.tick().expect("second tick");
        let processes = second.snapshot.processes.as_deref().expect("processes");
        // Every kept process has a rolling entry, with two samples now
        for p in processes {
            let stats = second.process_stats.get(&p.pid).expect("stats per pid");
            assert!(stats.samples >= 1);
        }
        // No alert can fire this early: the windows are not full
        assert!(second.alerts.is_empty());
        assert!(second.records.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_and_reload_keeps_engine_state() {
        let dir = test_dir("settings");
        let mut file = FileConfig::default();
        crate::settings::apply_add(&mut file, "alert-cpu", "40").expect("apply");
        crate::settings::save(&dir, &file).expect("save");

        let mut monitor = Monitor::new(&dir).expect("new");
        assert_eq!(monitor.settings().alerts.cpu, Some(40.0));
        assert!(monitor.alerts_enabled());
        assert_eq!(monitor.config_dir(), dir.as_path());

        // A settings panel writes, then asks for a reload
        let mut file = crate::settings::load(&dir).expect("load");
        crate::settings::apply_add(&mut file, "alert-cpu", "0").expect("apply");
        crate::settings::apply_add(&mut file, "alert-mem", "0").expect("apply");
        crate::settings::apply_add(&mut file, "alert-disk", "0").expect("apply");
        crate::settings::apply_add(&mut file, "alert-app-cpu", "0").expect("apply");
        crate::settings::apply_add(&mut file, "alert-app-mem", "0").expect("apply");
        crate::settings::apply_add(&mut file, "alert-pressure", "off").expect("apply");
        crate::settings::save(&dir, &file).expect("save");
        monitor.reload_settings().expect("reload");
        assert!(
            !monitor.alerts_enabled(),
            "every rule disabled means nothing to deliver"
        );

        // A malformed file is an error, not a silent revert to defaults
        std::fs::write(crate::settings::config_path(&dir), "not toml [").expect("write");
        assert!(monitor.reload_settings().is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_reads_back_what_was_written() {
        let dir = test_dir("history");
        let monitor = Monitor::new(&dir).expect("new");
        let today: jiff::civil::Date = "2026-08-12".parse().expect("date");
        crate::records::append(
            &dir,
            today,
            &[MetricRecord {
                timestamp: jiff::Timestamp::from_second(1).expect("ts"),
                pid: 1,
                name: "demo".into(),
                cpu_avg_percent: 50.0,
                memory_avg_bytes: 1024,
                memory_share_percent: 1.0,
                cpu_time_ms: 30_000,
            }],
        )
        .expect("append");

        let back = monitor.history(today, today).expect("history");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "demo");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

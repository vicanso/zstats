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

//! Daemon-side alerting: wires the library's rule engine
//! (`zstats::alerts`) to this frontend's delivery and persistence —
//! desktop notifications, config hot-reload, and the daily metrics files
//! (`zstats::records`). The rules themselves live in the lib so every
//! frontend agrees on them; what stays here is deliberately just "how
//! this frontend reacts".

use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use zstats::alerts::{ActiveThresholds, AlertEngine};
use zstats::records::MetricRecord;
use zstats::{MetricSink, SinkError, SystemSnapshot, async_trait};

use crate::settings;

/// Stat the config file's mtime once every this many collects (about a
/// minute at the default 2s interval) and hot-reload thresholds on change
const RELOAD_CHECK_EVERY: u32 = 30;

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

/// Sink that watches snapshots and fires desktop notifications, driven
/// by the library's [`AlertEngine`]
pub struct AlertSink {
    active: Mutex<ActiveThresholds>,
    engine: Mutex<AlertEngine>,
    reload: Mutex<ReloadState>,
}

impl AlertSink {
    /// Build from the `[alerts]` config section; the file is then
    /// re-checked every [`RELOAD_CHECK_EVERY`] collects and reloaded on
    /// mtime change
    pub fn from_config(file: &settings::AlertsConfig) -> Self {
        Self {
            active: Mutex::new(ActiveThresholds::from_config(file)),
            engine: Mutex::new(AlertEngine::new()),
            reload: Mutex::new(ReloadState {
                rounds_since_check: 0,
                last_mtime: config_mtime(),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .any_enabled()
    }

    /// Every [`RELOAD_CHECK_EVERY`] rounds, stat the config file and
    /// re-merge thresholds when its mtime changed. A file that fails to
    /// parse keeps the previous settings (unlike startup, a running
    /// daemon should not die over a config typo)
    fn maybe_reload(&self) {
        let mut reload = self.reload.lock().unwrap_or_else(|e| e.into_inner());
        reload.rounds_since_check += 1;
        if reload.rounds_since_check < RELOAD_CHECK_EVERY {
            return;
        }
        reload.rounds_since_check = 0;

        let mtime = config_mtime();
        tracing::debug!(?mtime, last = ?reload.last_mtime, "alert config reload check");
        if mtime == reload.last_mtime {
            return;
        }
        reload.last_mtime = mtime;

        match settings::load() {
            Ok(file) => {
                *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
                    ActiveThresholds::from_config(&file.alerts);
                tracing::info!("reloaded alert config from {}", settings::path().display());
            }
            Err(e) => {
                tracing::warn!("alert config reload failed, keeping previous settings: {e}");
            }
        }
    }

    /// Append records to `<config-dir>/data/<local-date>.jsonl`; the lib
    /// runs the daily retention sweep as part of the append
    fn persist_records(&self, records: &[MetricRecord]) {
        let today = jiff::Zoned::now().date();
        match zstats::records::append(&settings::dir(), today, records) {
            Ok(path) => tracing::debug!(
                "recorded {} metric line(s) to {}",
                records.len(),
                path.display()
            ),
            Err(e) => tracing::warn!("failed to write metrics history: {e}"),
        }
    }
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
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("notify-send")
            .args(["zstats", message])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(not(unix))]
    {
        let _ = message;
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
        let evaluation = {
            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            self.engine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .evaluate(Instant::now(), snapshot, &active)
        };
        for event in &evaluation.events {
            // Also log the alert: visible in the daemon log with a
            // timestamp, and it separates "rule fired" from "notification
            // displayed" when debugging delivery issues
            tracing::info!("alert: {}", event.message);
            send_notification(&event.message);
        }
        if !evaluation.records.is_empty() {
            self.persist_records(&evaluation.records);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "alerts"
    }
}

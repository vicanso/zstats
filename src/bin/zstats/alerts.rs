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

use zstats::alerts::{ActiveThresholds, AlertEngine, Template};
use zstats::records::MetricRecord;
use zstats::{MetricSink, SinkError, SystemSnapshot, async_trait};

use crate::settings;

/// Stat the config file's mtime once every this many collects (about a
/// minute at the default 2s interval) and hot-reload thresholds on change
const RELOAD_CHECK_EVERY: u32 = 30;
/// Give the notification helper this long before assuming it is stuck
const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn mtime(path: std::path::PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// mtimes of both files thresholds are built from. The template is
/// watched alongside the config precisely so refreshing it can be a
/// `curl -o` from cron — the daemon notices within about a minute and
/// never needs restarting
fn source_mtimes() -> (Option<SystemTime>, Option<SystemTime>) {
    (mtime(settings::path()), mtime(settings::template_path()))
}

/// Hot-reload bookkeeping
struct ReloadState {
    rounds_since_check: u32,
    last_mtime: (Option<SystemTime>, Option<SystemTime>),
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
    pub fn from_config(file: &settings::AlertsConfig, template: &Template) -> Self {
        Self {
            active: Mutex::new(ActiveThresholds::from_config_with_template(file, template)),
            engine: Mutex::new(AlertEngine::new()),
            reload: Mutex::new(ReloadState {
                rounds_since_check: 0,
                last_mtime: source_mtimes(),
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

        let mtimes = source_mtimes();
        tracing::debug!(?mtimes, last = ?reload.last_mtime, "alert config reload check");
        if mtimes == reload.last_mtime {
            return;
        }
        reload.last_mtime = mtimes;

        // Both halves must load before either is applied: half-new
        // thresholds are worse than the previous consistent set
        match (settings::load(), settings::load_template()) {
            (Ok(file), Ok(template)) => {
                *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
                    ActiveThresholds::from_config_with_template(&file.alerts, &template);
                tracing::info!(
                    config = %settings::path().display(),
                    template = %settings::template_path().display(),
                    "reloaded alert config"
                );
            }
            (Err(e), _) | (_, Err(e)) => {
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

/// Fire a desktop notification: macOS uses osascript (whose Script Editor
/// identity reliably displays banners — notification-center APIs
/// masquerading as Terminal.app get silently dropped on this setup),
/// other unixes try notify-send.
///
/// The exit status IS checked and failures are logged. Fire-and-forget
/// would make "the rule fired" and "the user was actually told"
/// indistinguishable after the fact — exactly the ambiguity that makes a
/// missing notification impossible to diagnose from the daemon log. Note
/// this only covers handing the message to the OS: whether the system
/// then displays it immediately, defers it into a notification summary,
/// or holds it behind a Focus mode is outside our reach.
async fn send_notification(message: &str) {
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    let (program, args) = (
        "osascript",
        vec![
            "-e".to_string(),
            format!(
                "display notification \"{}\" with title \"zstats\"",
                escape_applescript(message)
            ),
        ],
    );
    #[cfg(not(target_os = "macos"))]
    let (program, args) = (
        "notify-send",
        vec!["zstats".to_string(), message.to_string()],
    );

    // A wedged helper must not hold up the sink (the scheduler gives each
    // sink 5s); osascript normally returns in well under a second
    let result = tokio::time::timeout(
        NOTIFY_TIMEOUT,
        tokio::process::Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(out)) if out.status.success() => {
            tracing::debug!("{program} accepted the notification");
        }
        Ok(Ok(out)) => tracing::warn!(
            "{program} rejected the notification ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Ok(Err(e)) => tracing::warn!("failed to run {program}: {e}"),
        Err(_) => tracing::warn!("{program} timed out after {NOTIFY_TIMEOUT:?}"),
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
        let mut pending = Vec::with_capacity(evaluation.events.len());
        for event in &evaluation.events {
            // Also log the alert, with the structured fields alongside
            // the text: `ZSTATS_LOG` output stays greppable by rule and
            // severity, and it separates "rule fired" from "notification
            // displayed" when debugging delivery issues
            tracing::info!(
                kind = ?event.kind(),
                severity = ?event.severity(),
                repeat = event.repeat_after.is_some(),
                "alert: {}",
                event.summary()
            );
            pending.push(event.summary());
        }
        // Concurrently, not one after another: the engine has already
        // recorded every one of these as notified, so a sink that runs
        // out of its 5s budget partway through the list would drop the
        // rest permanently — the episode state never re-offers them.
        // Serially that needed only three alerts and a slow helper
        // (NOTIFY_TIMEOUT is 2s each); concurrently the whole batch costs
        // one timeout
        let mut deliveries = Vec::with_capacity(pending.len());
        for message in pending {
            deliveries.push(tokio::spawn(
                async move { send_notification(&message).await },
            ));
        }
        for delivery in deliveries {
            let _ = delivery.await;
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

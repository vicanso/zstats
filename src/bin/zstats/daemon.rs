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

//! Daemon mode: a background collector that keeps recent history and serves
//! it to attaching clients over a per-user Unix domain socket.
//!
//! Line-oriented protocol:
//! - the client sends one command line: `ATTACH` or `STOP`
//! - for `ATTACH` the server replies `HISTORY <n>` followed by n buffered
//!   snapshots as JSON lines (oldest first), then streams each new snapshot
//!   as another JSON line until the client disconnects
//! - for `STOP` the server replies `OK` and shuts down

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast};
use zstats::{
    CollectorConfig, LocalCollector, MetricSink, Scheduler, SinkError, SystemSnapshot, async_trait,
};

use crate::render::{TextSink, enter_live_screen, leave_live_screen};

/// Per-user socket path, e.g. `$TMPDIR/zstats-tree.sock`
pub fn socket_path() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    std::env::temp_dir().join(format!("zstats-{user}.sock"))
}

/// Whether a daemon is currently reachable on the socket
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

/// Per-user daemon log file, e.g. `$TMPDIR/zstats-tree.log` — the detached
/// daemon's stdout/stderr (alert lines, warnings) append here
pub fn log_path() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    std::env::temp_dir().join(format!("zstats-{user}.log"))
}

/// Ring buffer of recent snapshots, pruned by timestamp span
struct HistoryBuffer {
    retention_secs: i64,
    snapshots: Mutex<VecDeque<SystemSnapshot>>,
}

impl HistoryBuffer {
    fn new(retention: Duration) -> Self {
        Self {
            retention_secs: retention.as_secs() as i64,
            snapshots: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, snapshot: SystemSnapshot) {
        let mut queue = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(snapshot);
        while queue.len() > 1 {
            let span = queue.back().expect("non-empty").timestamp.as_second()
                - queue.front().expect("non-empty").timestamp.as_second();
            if span > self.retention_secs {
                queue.pop_front();
            } else {
                break;
            }
        }
    }

    fn to_json_lines(&self) -> Vec<String> {
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| serde_json::to_string(s).expect("snapshot is always serializable"))
            .collect()
    }
}

/// Sink feeding the daemon: records history and fans out to live clients
struct ServeSink {
    history: Arc<HistoryBuffer>,
    live: broadcast::Sender<SystemSnapshot>,
}

#[async_trait]
impl MetricSink for ServeSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        self.history.push(snapshot.clone());
        // No connected clients is not an error
        let _ = self.live.send(snapshot.clone());
        Ok(())
    }

    fn name(&self) -> &str {
        "serve"
    }
}

/// Run the collector daemon until Ctrl+C or a client sends STOP.
/// `extra_sinks` (e.g. alerting) run alongside the history/broadcast sink
pub async fn serve(
    config: CollectorConfig,
    interval: Duration,
    retention: Duration,
    extra_sinks: Vec<Arc<dyn MetricSink>>,
) -> ExitCode {
    let path = socket_path();
    if path.exists() {
        if UnixStream::connect(&path).await.is_ok() {
            eprintln!("zstats daemon is already running at {}", path.display());
            return ExitCode::FAILURE;
        }
        // Stale socket from a previous unclean exit
        let _ = std::fs::remove_file(&path);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("failed to bind {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let history = Arc::new(HistoryBuffer::new(retention));
    let (live_tx, _) = broadcast::channel(64);
    let shutdown = Arc::new(Notify::new());

    let mut sinks: Vec<Arc<dyn MetricSink>> = vec![Arc::new(ServeSink {
        history: Arc::clone(&history),
        live: live_tx.clone(),
    })];
    sinks.extend(extra_sinks);
    let collector = LocalCollector::new(config);
    let mut scheduler = Scheduler::new(Box::new(collector), sinks, interval);
    if scheduler.start().await.is_err() {
        eprintln!("failed to start scheduler");
        return ExitCode::FAILURE;
    }

    tracing::info!(
        "daemon started: socket {}, interval {}, history {}",
        path.display(),
        zstats::config::format_duration(interval),
        zstats::config::format_duration(retention),
    );

    let accept_task = {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let history = Arc::clone(&history);
                let live = live_tx.clone();
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if matches!(handle_client(stream, history, live).await, Ok(true)) {
                        shutdown.notify_one();
                    }
                });
            }
        })
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = shutdown.notified() => {}
    }
    scheduler.stop().await;
    accept_task.abort();
    let _ = std::fs::remove_file(&path);
    tracing::info!("daemon stopped");
    ExitCode::SUCCESS
}

/// Returns Ok(true) when the client asked the daemon to stop
async fn handle_client(
    stream: UnixStream,
    history: Arc<HistoryBuffer>,
    live: broadcast::Sender<SystemSnapshot>,
) -> std::io::Result<bool> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let command = lines.next_line().await?.unwrap_or_default();

    match command.trim() {
        "STOP" => {
            writer.write_all(b"OK\n").await?;
            Ok(true)
        }
        "ATTACH" => {
            // Subscribe before copying history so no snapshot can fall into
            // the gap (a duplicate frame is harmless, a hole is not)
            let mut rx = live.subscribe();
            let buffered = history.to_json_lines();
            writer
                .write_all(format!("HISTORY {}\n", buffered.len()).as_bytes())
                .await?;
            for line in buffered {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            loop {
                match rx.recv().await {
                    Ok(snapshot) => {
                        let line =
                            serde_json::to_string(&snapshot).expect("snapshot is serializable");
                        writer.write_all(line.as_bytes()).await?;
                        writer.write_all(b"\n").await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Attach to the daemon: replay buffered history, then follow live data.
/// `q` / `d` / Ctrl+C all leave the client; the daemon keeps running.
pub async fn attach() -> ExitCode {
    use tokio::io::AsyncReadExt as _;

    use crate::keys::{self, LiveExit, RawMode};

    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(_) => {
            eprintln!("zstats daemon is not running (start it with `zstats serve`)");
            return ExitCode::FAILURE;
        }
    };
    let (reader, mut writer) = stream.into_split();
    if writer.write_all(b"ATTACH\n").await.is_err() {
        eprintln!("failed to talk to the daemon");
        return ExitCode::FAILURE;
    }
    let mut lines = BufReader::new(reader).lines();

    let header = lines.next_line().await.ok().flatten().unwrap_or_default();
    let history_count: usize = header
        .strip_prefix("HISTORY ")
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);

    let interactive = enter_live_screen();
    let mut sink = TextSink::new();
    if interactive {
        sink = sink.with_footer("q/d detach · daemon keeps running");
    }

    // Replay history: absorb everything except the newest snapshot so the
    // rolling averages are warm, then render the newest immediately
    for i in 0..history_count {
        let Some(line) = lines.next_line().await.ok().flatten() else {
            break;
        };
        let Ok(snapshot) = serde_json::from_str::<SystemSnapshot>(&line) else {
            continue;
        };
        if i + 1 == history_count {
            let _ = sink.write(&snapshot).await;
        } else {
            sink.absorb(&snapshot);
        }
    }

    // Keys only work with raw stdin; when that fails, fall back to Ctrl+C
    let raw = if interactive { RawMode::enable() } else { None };
    let keys_enabled = raw.is_some();
    let _raw = raw;

    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1];
    let mut daemon_gone = false;
    let mut exit = LiveExit::Quit;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                exit = LiveExit::Quit;
                break;
            }
            result = stdin.read(&mut buf), if keys_enabled => {
                // stdin closed or error: keep following the daemon stream
                if let Ok(n) = result
                    && n > 0
                    && let Some(action) = keys::key_to_exit(buf[0])
                {
                    exit = action;
                    break;
                }
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Ok(snapshot) = serde_json::from_str::<SystemSnapshot>(&line) {
                            let _ = sink.write(&snapshot).await;
                        }
                    }
                    _ => {
                        daemon_gone = true;
                        break;
                    }
                }
            }
        }
    }

    leave_live_screen(interactive);
    if daemon_gone {
        eprintln!("zstats daemon closed the connection");
    } else if matches!(exit, LiveExit::Detach | LiveExit::Quit) {
        // Leaving the client is intentional; remind how to come back
        println!(
            "detached; daemon still running (log: {}). reattach with `zstats attach`",
            log_path().display()
        );
    }
    ExitCode::SUCCESS
}

/// Ask the daemon to shut down. Idempotent: a daemon that is not running
/// already satisfies "stopped", so that case succeeds silently
pub async fn stop() -> ExitCode {
    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(_) => return ExitCode::SUCCESS,
    };
    let (reader, mut writer) = stream.into_split();
    if writer.write_all(b"STOP\n").await.is_err() {
        eprintln!("failed to talk to the daemon");
        return ExitCode::FAILURE;
    }
    let mut lines = BufReader::new(reader).lines();
    let _ = lines.next_line().await;
    println!("zstats daemon stopped");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use zstats::{CpuSnapshot, HostInfo, LoadSnapshot, MemorySnapshot};

    fn snapshot_at(second: i64) -> SystemSnapshot {
        SystemSnapshot {
            timestamp: jiff::Timestamp::from_second(second).expect("valid timestamp"),
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
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: None,
            networks: None,
            processes: None,
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
    fn history_prunes_by_retention_span() {
        let buffer = HistoryBuffer::new(Duration::from_secs(300));
        // One snapshot per second for 400s: only the last ~300s must remain
        for second in 0..400 {
            buffer.push(snapshot_at(second));
        }

        let queue = buffer.snapshots.lock().unwrap();
        let newest = queue.back().unwrap().timestamp.as_second();
        let oldest = queue.front().unwrap().timestamp.as_second();
        assert_eq!(newest, 399);
        assert!(newest - oldest <= 300);
        assert!(queue.len() >= 300);
    }
}

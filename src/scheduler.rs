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

//! Scheduler: triggers collection at a fixed interval and dispatches the
//! result to every registered sink.
//!
//! Behavior highlights:
//! - When a collect overruns the interval, missed ticks are skipped
//!   (`MissedTickBehavior::Skip`).
//! - Collection runs inside `spawn_blocking`, bounded by `collect_timeout`.
//! - Each sink's `write` runs concurrently with its own timeout; a single
//!   slow/broken sink never affects the others.
//! - A failed collect is only logged; the loop continues with the next round.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::collector::Collector;
use crate::error::{CollectError, SchedulerError};
use crate::sink::MetricSink;
use crate::snapshot::SystemSnapshot;

const DEFAULT_SINK_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

type SharedCollector = Arc<Mutex<Box<dyn Collector>>>;

pub struct Scheduler {
    collector: SharedCollector,
    sinks: Vec<Arc<dyn MetricSink>>,
    interval: Duration,
    sink_write_timeout: Duration,
    // Run control
    shutdown_tx: Option<oneshot::Sender<()>>,
    loop_handle: Option<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(
        collector: Box<dyn Collector>,
        sinks: Vec<Arc<dyn MetricSink>>,
        interval: Duration,
    ) -> Self {
        Self {
            collector: Arc::new(Mutex::new(collector)),
            sinks,
            interval,
            sink_write_timeout: DEFAULT_SINK_WRITE_TIMEOUT,
            shutdown_tx: None,
            loop_handle: None,
        }
    }

    /// Override the per-sink write timeout
    pub fn with_sink_write_timeout(mut self, timeout: Duration) -> Self {
        self.sink_write_timeout = timeout;
        self
    }

    /// Start the background collection loop (async, returns immediately)
    pub async fn start(&mut self) -> Result<(), SchedulerError> {
        if self.shutdown_tx.is_some() {
            return Err(SchedulerError::AlreadyRunning);
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let collector = Arc::clone(&self.collector);
        let sinks = self.sinks.clone();
        let interval = self.interval;
        let sink_timeout = self.sink_write_timeout;

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = ticker.tick() => {
                        match collect_snapshot(&collector).await {
                            Ok(snapshot) => dispatch(&sinks, snapshot, sink_timeout).await,
                            Err(e) => tracing::warn!(error = %e, "collect failed, will retry next tick"),
                        }
                    }
                }
            }
            tracing::debug!("scheduler loop stopped");
        });

        self.shutdown_tx = Some(shutdown_tx);
        self.loop_handle = Some(handle);
        Ok(())
    }

    /// Graceful stop
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.loop_handle.take() {
            let _ = handle.await;
        }
    }

    /// Collect and dispatch immediately (for app-initiated refresh)
    pub async fn collect_once(&mut self) -> Result<SystemSnapshot, CollectError> {
        let snapshot = collect_snapshot(&self.collector).await?;
        dispatch(&self.sinks, snapshot.clone(), self.sink_write_timeout).await;
        Ok(snapshot)
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // Fallback: notify the background loop to exit even without an explicit stop
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Run collection on the blocking thread pool, bounded by collect_timeout
async fn collect_snapshot(collector: &SharedCollector) -> Result<SystemSnapshot, CollectError> {
    let timeout = lock_collector(collector).config().collect_timeout;

    let shared = Arc::clone(collector);
    let task = tokio::task::spawn_blocking(move || lock_collector(&shared).collect());

    match tokio::time::timeout(timeout, task).await {
        Err(_) => Err(CollectError::Timeout),
        Ok(Err(join_err)) => Err(CollectError::System {
            message: join_err.to_string(),
        }),
        Ok(Ok(result)) => result,
    }
}

fn lock_collector(collector: &SharedCollector) -> std::sync::MutexGuard<'_, Box<dyn Collector>> {
    // If a panic during collect poisons the lock, keep using the inner data
    // so the scheduling loop survives
    collector
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Dispatch to all sinks concurrently, each with its own timeout, isolated
/// from one another
async fn dispatch(sinks: &[Arc<dyn MetricSink>], snapshot: SystemSnapshot, timeout: Duration) {
    let mut handles = Vec::with_capacity(sinks.len());
    for sink in sinks {
        let sink = Arc::clone(sink);
        let snapshot = snapshot.clone();
        handles.push(tokio::spawn(async move {
            match tokio::time::timeout(timeout, sink.write(&snapshot)).await {
                Err(_) => tracing::warn!(sink = sink.name(), "sink write timed out"),
                Ok(Err(e)) => tracing::warn!(sink = sink.name(), error = %e, "sink write failed"),
                Ok(Ok(())) => {}
            }
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
}

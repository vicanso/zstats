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

// Integration tests depend on async scheduling; skipped entirely when the
// runtime feature is off
#![cfg(feature = "runtime")]

use std::sync::Arc;
use std::time::Duration;

use zstats::{
    Collector, CollectorConfig, LocalChannelSink, LocalCollector, MetricSink, Scheduler, SinkError,
    SystemSnapshot, async_trait,
};

#[test]
fn local_collector_produces_sane_snapshot() {
    let config = CollectorConfig::default();
    let max_processes = config.max_processes;
    let mut collector = LocalCollector::new(config);

    let first = collector.collect().expect("first collect");
    assert!(first.memory.total_bytes > 0);
    assert!(first.cpu.logical_cores > 0);
    assert!(!first.host.hostname.is_empty());
    // The first sample has no diff baseline; rates should be None
    for disk in &first.disks {
        assert!(disk.read_bytes_per_sec.is_none());
        assert!(disk.write_bytes_per_sec.is_none());
    }

    std::thread::sleep(Duration::from_millis(300));
    let second = collector.collect().expect("second collect");

    assert_eq!(
        second.cpu.per_core_usage.len() as u32,
        second.cpu.logical_cores
    );
    let processes = second.processes.as_ref().expect("processes enabled");
    assert!(!processes.is_empty());
    assert!(processes.len() <= max_processes);
    // From the second sample on, rate metrics should have values
    for disk in &second.disks {
        assert!(disk.read_bytes_per_sec.is_some());
        assert!(disk.write_bytes_per_sec.is_some());
    }
    assert!(second.timestamp >= first.timestamp);

    // The snapshot round-trips through serialization
    let json = serde_json::to_string(&second).expect("serialize");
    let parsed: SystemSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.memory.total_bytes, second.memory.total_bytes);
}

#[test]
fn config_toggles_are_respected() {
    let config = CollectorConfig {
        collect_processes: false,
        per_core_cpu: false,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);
    let snapshot = collector.collect().expect("collect");

    assert!(snapshot.processes.is_none());
    assert!(snapshot.cpu.per_core_usage.is_empty());
}

struct FailingSink;

#[async_trait]
impl MetricSink for FailingSink {
    async fn write(&self, _snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        Err(SinkError::Remote {
            message: "boom".into(),
        })
    }

    fn name(&self) -> &str {
        "failing"
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_delivers_snapshots_and_isolates_failing_sink() {
    let (sink, mut rx) = LocalChannelSink::channel();
    let sinks: Vec<Arc<dyn MetricSink>> = vec![Arc::new(FailingSink), Arc::new(sink)];

    let collector = LocalCollector::new(CollectorConfig {
        collect_processes: false,
        ..Default::default()
    });
    let mut scheduler = Scheduler::new(Box::new(collector), sinks, Duration::from_millis(200));

    scheduler.start().await.expect("start");
    // A second start should fail
    assert!(scheduler.start().await.is_err());

    // A broken sink must not prevent LocalChannelSink from receiving data
    tokio::time::timeout(Duration::from_secs(5), rx.changed())
        .await
        .expect("snapshot within 5s")
        .expect("channel alive");
    assert!(rx.borrow().is_some());

    scheduler.stop().await;
    // Can start again after stopping
    scheduler.start().await.expect("restart");
    scheduler.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn collect_once_returns_snapshot_and_dispatches() {
    let (sink, rx) = LocalChannelSink::channel();
    let collector = LocalCollector::new(CollectorConfig {
        collect_processes: false,
        ..Default::default()
    });
    let mut scheduler = Scheduler::new(
        Box::new(collector),
        vec![Arc::new(sink) as Arc<dyn MetricSink>],
        Duration::from_secs(60),
    );

    let snapshot = scheduler.collect_once().await.expect("collect once");
    assert!(snapshot.memory.total_bytes > 0);
    // Dispatch works even without the background loop running
    assert!(rx.borrow().is_some());
}

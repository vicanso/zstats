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
    for disk in first.disks.as_ref().expect("disks enabled") {
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
    // From the second sample on, rate metrics should have values. A disk or
    // interface that appeared between the two samples has no diff baseline
    // yet (None), so require at least one entry with rates — always true for
    // pre-existing ones — instead of all
    let disks = second.disks.as_ref().expect("disks enabled");
    assert!(
        disks
            .iter()
            .any(|d| d.read_bytes_per_sec.is_some() && d.write_bytes_per_sec.is_some())
    );
    assert!(second.timestamp >= first.timestamp);
    assert!(second.host.uptime_secs > 0);
    let networks = second.networks.as_ref().expect("networks enabled");
    assert!(networks.iter().any(|n| {
        // Error rates should be Some (possibly 0) from the second sample on
        n.received_errors_per_sec.is_some() && n.transmitted_errors_per_sec.is_some()
    }));
    for p in processes.iter() {
        // Disk IO off by default
        assert!(p.read_bytes_per_sec.is_none());
        assert!(p.write_bytes_per_sec.is_none());
    }

    // The snapshot round-trips through serialization
    let json = serde_json::to_string(&second).expect("serialize");
    let parsed: SystemSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.memory.total_bytes, second.memory.total_bytes);
}

#[test]
fn process_disk_io_opt_in_and_disk_dedupe() {
    let config = CollectorConfig {
        collect_process_disk_io: true,
        dedupe_disks: true,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);
    let _ = collector.collect().expect("first");
    std::thread::sleep(Duration::from_millis(300));
    let second = collector.collect().expect("second");

    let processes = second.processes.as_ref().expect("processes");
    // At least one process should have rate fields (Some after second sample)
    assert!(
        processes
            .iter()
            .any(|p| p.read_bytes_per_sec.is_some() && p.write_bytes_per_sec.is_some()),
        "process disk IO rates should be populated when enabled"
    );

    // Dedupe: no two disks should share the same device name
    if let Some(disks) = &second.disks {
        let mut names = std::collections::HashSet::new();
        for d in disks {
            assert!(
                names.insert(d.name.clone()),
                "duplicate disk name after dedupe: {}",
                d.name
            );
        }
    }
}

#[test]
fn process_refresh_interval_reuses_cached_list() {
    let config = CollectorConfig {
        // Effectively never due again within this test; the CPU boost must
        // be off too, or a busy test machine would force a refresh
        process_refresh_interval: Duration::from_secs(3600),
        process_boost_cpu_percent: None,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);

    let first = collector.collect().expect("first collect");
    std::thread::sleep(Duration::from_millis(200));
    let second = collector.collect().expect("second collect");

    let a = first.processes.expect("processes enabled");
    let b = second.processes.expect("processes enabled");
    // The second snapshot must carry the exact cached list: identical pids
    // AND bit-identical cpu values (a re-collection would recompute them)
    assert_eq!(
        a.iter().map(|p| p.pid).collect::<Vec<_>>(),
        b.iter().map(|p| p.pid).collect::<Vec<_>>()
    );
    assert_eq!(
        a.iter()
            .map(|p| p.cpu_usage_percent.to_bits())
            .collect::<Vec<_>>(),
        b.iter()
            .map(|p| p.cpu_usage_percent.to_bits())
            .collect::<Vec<_>>()
    );
}

#[test]
fn config_toggles_are_respected() {
    let config = CollectorConfig {
        collect_processes: false,
        per_core_cpu: false,
        collect_disks: false,
        collect_networks: false,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);
    let snapshot = collector.collect().expect("collect");

    assert!(snapshot.processes.is_none());
    assert!(snapshot.cpu.per_core_usage.is_empty());
    assert!(snapshot.disks.is_none());
    assert!(snapshot.networks.is_none());
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

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
    // The kept list is a truncation of the full table
    let total = second.total_processes.expect("total with processes on");
    assert!(total as usize >= processes.len());
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

    // Memory pressure (macOS): when reported, values must be sane
    if let Some(compressed) = second.memory.compressed_bytes {
        assert!(compressed < second.memory.total_bytes);
    }
    if let Some(level) = second.memory.pressure_level {
        assert!(matches!(level, 1 | 2 | 4), "unexpected level {level}");
    }
    #[cfg(target_os = "macos")]
    {
        assert!(second.memory.compressed_bytes.is_some());
        assert!(second.memory.pressure_level.is_some());
    }

    // Perf levels (Apple Silicon P/E clusters): when reported, the
    // topology must exactly cover the logical cores and usages stay sane
    if let Some(levels) = &second.cpu.perf_levels {
        assert!(levels.len() >= 2);
        let total: u32 = levels.iter().map(|l| l.logical_cores).sum();
        assert_eq!(total, second.cpu.logical_cores);
        for level in levels {
            assert!(!level.name.is_empty());
            assert!(level.usage_percent >= 0.0);
        }
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
fn disk_and_network_intervals_reuse_cached_lists() {
    let config = CollectorConfig {
        // Effectively never due again within this test
        disk_io_refresh_interval: Duration::from_secs(3600),
        network_refresh_interval: Duration::from_secs(3600),
        collect_processes: false,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);

    let first = collector.collect().expect("first collect");
    std::thread::sleep(Duration::from_millis(200));
    let second = collector.collect().expect("second collect");

    // Between refreshes the cached lists are returned verbatim
    let (d1, d2) = (
        first.disks.as_ref().expect("disks"),
        second.disks.as_ref().expect("disks"),
    );
    assert_eq!(d1.len(), d2.len());
    for (a, b) in d1.iter().zip(d2.iter()) {
        assert_eq!(a.mount_point, b.mount_point);
        assert_eq!(a.available_bytes, b.available_bytes);
        assert_eq!(a.read_bytes_per_sec, b.read_bytes_per_sec);
        assert_eq!(a.write_bytes_per_sec, b.write_bytes_per_sec);
    }
    let (n1, n2) = (
        first.networks.as_ref().expect("networks"),
        second.networks.as_ref().expect("networks"),
    );
    assert_eq!(n1.len(), n2.len());
    for (a, b) in n1.iter().zip(n2.iter()) {
        assert_eq!(a.interface, b.interface);
        assert_eq!(a.received_bytes_per_sec, b.received_bytes_per_sec);
        assert_eq!(a.transmitted_bytes_per_sec, b.transmitted_bytes_per_sec);
    }
}

#[test]
fn process_refresh_interval_reuses_cached_list() {
    let config = CollectorConfig {
        // Effectively never due again within this test; the CPU boost must
        // be off too, or a busy test machine would force a refresh
        process_refresh_interval: Duration::from_secs(3600),
        process_boost_cpu_cores: None,
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
        collect_temperatures: false,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);
    let snapshot = collector.collect().expect("collect");

    assert!(snapshot.processes.is_none());
    assert!(snapshot.cpu.per_core_usage.is_empty());
    assert!(snapshot.disks.is_none());
    assert!(snapshot.networks.is_none());
    assert!(snapshot.temperatures.is_none());
    // Groups and the total count require process collection even though
    // groups' own toggle defaults to on
    assert!(snapshot.process_groups.is_none());
    assert!(snapshot.total_processes.is_none());
}

#[test]
fn process_groups_aggregate_trees_and_respect_toggle_and_cache() {
    let max_processes = CollectorConfig::default().max_processes;
    let mut collector = LocalCollector::new(CollectorConfig {
        // Freeze the process cadence after the first collect so the
        // second one must serve the cached groups
        process_refresh_interval: Duration::from_secs(3600),
        process_boost_cpu_cores: None,
        ..Default::default()
    });

    let first = collector.collect().expect("first collect");
    let groups = first.process_groups.as_ref().expect("groups enabled");
    assert!(!groups.is_empty());
    assert!(groups.len() <= max_processes);
    for g in groups.iter() {
        assert!(g.process_count >= 1);
        assert!(!g.name.is_empty());
    }
    // Groups are ranked by CPU
    for pair in groups.windows(2) {
        assert!(pair[0].cpu_usage_percent >= pair[1].cpu_usage_percent);
    }
    // A real desktop always has at least one multi-process tree (this
    // test binary itself runs under cargo under a shell)
    assert!(groups.iter().any(|g| g.process_count > 1));
    // The whole-tree memory sum must be at least any single member's
    let processes = first.processes.as_ref().expect("processes enabled");
    let total_group_mem: u64 = groups.iter().map(|g| g.memory_bytes).sum();
    let max_proc_mem = processes.iter().map(|p| p.memory_bytes).max().unwrap_or(0);
    assert!(total_group_mem >= max_proc_mem);

    // Between process refreshes the cached Arc is served verbatim
    let second = collector.collect().expect("second collect");
    assert!(Arc::ptr_eq(
        first.process_groups.as_ref().expect("groups"),
        second.process_groups.as_ref().expect("groups")
    ));

    // Toggle off: everything else still on, groups absent
    let mut collector = LocalCollector::new(CollectorConfig {
        collect_process_groups: false,
        ..Default::default()
    });
    let snapshot = collector.collect().expect("collect");
    assert!(snapshot.processes.is_some());
    assert!(snapshot.process_groups.is_none());
}

#[test]
fn temperatures_refresh_interval_reuses_cache() {
    let config = CollectorConfig {
        collect_temperatures: true,
        // Effectively never re-refresh within this test
        temperature_refresh_interval: Duration::from_secs(3600),
        collect_processes: false,
        ..Default::default()
    };
    let mut collector = LocalCollector::new(config);
    let first = collector.collect().expect("first");
    let second = collector.collect().expect("second");

    let a = first.temperatures.expect("temps enabled");
    let b = second.temperatures.expect("temps enabled");
    // Same cached sample (labels + values bit-identical)
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.label, y.label);
        assert_eq!(x.celsius.to_bits(), y.celsius.to_bits());
    }
    // Plausible filter: no absurd firmware placeholders
    for t in &a {
        assert!(
            (-20.0..=150.0).contains(&t.celsius),
            "implausible temp {} for {}",
            t.celsius,
            t.label
        );
    }
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

/// Client side of the daemon protocol, exercised against a scripted fake
/// server (the real server needs a full daemon; the wire format is what
/// matters here)
#[cfg(all(feature = "client", unix))]
mod client_protocol {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use zstats::SystemSnapshot;

    fn snapshot_at(second: i64) -> SystemSnapshot {
        let mut collector = zstats::LocalCollector::new(zstats::CollectorConfig {
            collect_processes: false,
            collect_disks: false,
            collect_networks: false,
            collect_temperatures: false,
            ..Default::default()
        });
        let mut snapshot = zstats::Collector::collect(&mut collector).expect("collect");
        snapshot.timestamp = jiff::Timestamp::from_second(second).expect("valid timestamp");
        snapshot
    }

    fn test_socket(name: &str) -> std::path::PathBuf {
        // Keep it short: unix socket paths cap at ~104 bytes
        std::env::temp_dir().join(format!("zst-{}-{name}.sock", std::process::id()))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_replays_history_then_streams_live() {
        let path = test_socket("attach");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test socket");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let command = lines.next_line().await.expect("read").unwrap_or_default();
            assert_eq!(command, "ATTACH");

            writer.write_all(b"HISTORY 2\n").await.expect("header");
            for second in [1, 2] {
                let line = serde_json::to_string(&snapshot_at(second)).expect("serialize");
                writer
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .expect("history line");
            }
            // A garbled line must be skipped by the client, then live data
            writer.write_all(b"not json\n").await.expect("garbage");
            let live = serde_json::to_string(&snapshot_at(3)).expect("serialize");
            writer
                .write_all(format!("{live}\n").as_bytes())
                .await
                .expect("live line");
            // Dropping the writer closes the stream: client sees EOF
        });

        let mut stream = zstats::client::attach_at(&path).await.expect("attach");
        assert_eq!(stream.history_len(), 2);
        assert_eq!(stream.history_remaining(), 2);

        let first = stream.next().await.expect("next").expect("history 1");
        assert_eq!(first.timestamp.as_second(), 1);
        assert_eq!(stream.history_remaining(), 1);

        let second = stream.next().await.expect("next").expect("history 2");
        assert_eq!(second.timestamp.as_second(), 2);
        assert_eq!(stream.history_remaining(), 0);

        // The garbled line is skipped transparently
        let live = stream.next().await.expect("next").expect("live");
        assert_eq!(live.timestamp.as_second(), 3);

        assert!(stream.next().await.expect("eof").is_none());
        server.await.expect("server task");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_rejects_non_protocol_reply() {
        let path = test_socket("proto");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test socket");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            // Consume the command first so the client's ATTACH write can
            // never race against this connection closing
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await;
            writer.write_all(b"WHAT\n").await.expect("reply");
        });

        match zstats::client::attach_at(&path).await {
            Err(zstats::ClientError::Protocol { reply }) => assert_eq!(reply, "WHAT"),
            Err(other) => panic!("expected a protocol error, got: {other}"),
            Ok(_) => panic!("attach unexpectedly succeeded"),
        }
        server.await.expect("server task");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_reports_whether_a_daemon_was_reached() {
        let path = test_socket("stop");
        let _ = std::fs::remove_file(&path);

        // No socket: nothing to stop, and that is a success
        assert!(!zstats::client::stop_at(&path).await.expect("stop"));

        let listener = UnixListener::bind(&path).expect("bind test socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let command = lines.next_line().await.expect("read").unwrap_or_default();
            assert_eq!(command, "STOP");
            writer.write_all(b"OK\n").await.expect("reply");
        });

        assert!(zstats::client::stop_at(&path).await.expect("stop"));
        server.await.expect("server task");
        let _ = std::fs::remove_file(&path);
    }
}

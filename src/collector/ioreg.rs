// Copyright 2025 zstats authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Read-only IORegistry queries through the `ioreg` tool.
//!
//! GPU utilisation and block-storage driver statistics live in the macOS
//! IORegistry. The C API for that registry is IOKit — hand-written
//! unsafe FFI with no safe-wrapper crate, which `#![forbid(unsafe_code)]`
//! rules out. `ioreg` is the same registry read as a stock system tool:
//! it ships with every macOS, needs no root, and its `-a` output is an
//! XML plist the `plist` crate parses.
//!
//! **What a read costs depends on how often it runs.** Back to back, a
//! query takes 11-13 ms of CPU. At a real cadence, reads seconds apart,
//! the same query measured ~45 ms, child process included (M4 Pro,
//! 2026-09-25: GPU ~46 ms, drives ~41 ms, both in one collect ~66 ms). The
//! work lands on a core that has idled down, so it takes longer. The
//! back-to-back figure is the one first recorded here, and it made the
//! 10s defaults look like ~0.25% of a core; the real figure for both is
//! ~0.66%, about twice the process-table walk (~36 ms) under the same
//! conditions. Hence the separate cadences, and the runtime switches
//! (`LocalCollector::set_collect_gpu` / `set_collect_drives`) for a
//! frontend that shows these figures on one screen only.
//!
//! This is the library's only child process. A registry that stalls
//! (wedged USB storage is the realistic case) must not stall the collect
//! behind it, so every run is bounded by [`IOREG_TIMEOUT`] and killed at
//! the deadline; the round then simply reports nothing.
//!
//! The sample types are platform-neutral so the collector's diffing and
//! caching compile everywhere; only the queries are macOS. Elsewhere
//! [`gpus`] and [`drives`] report `None` — the "not on this platform"
//! answer `Capabilities` spells out.

#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use plist::{Dictionary, Value};

/// Hard cap on one `ioreg` run. Normal runs finish in 10-50 ms of wall
/// time (the upper end after the CPU has idled); anything near this bound
/// is a stuck registry, and the collect must not wait
#[cfg(target_os = "macos")]
const IOREG_TIMEOUT: Duration = Duration::from_secs(1);

/// One GPU's current sample, straight from the accelerator driver's
/// `PerformanceStatistics` dictionary
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct GpuSample {
    pub name: String,
    pub cores: Option<u32>,
    pub utilization_percent: f32,
    pub renderer_utilization_percent: Option<f32>,
    pub tiler_utilization_percent: Option<f32>,
    pub memory_in_use_bytes: Option<u64>,
    pub memory_allocated_bytes: Option<u64>,
}

/// One block-storage driver's cumulative counters plus the identity of
/// the whole-disk media beneath it. All counters are since boot; the
/// collector diffs two of these into rates
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct DriveCounters {
    /// BSD name of the whole disk (`disk0`)
    pub name: String,
    /// The media's registry name with its ` Media` suffix removed
    /// (`APPLE SSD AP0512Z`)
    pub model: Option<String>,
    pub size_bytes: Option<u64>,
    pub is_removable: bool,
    pub read_ops: u64,
    pub write_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Summed service time of completed operations, nanoseconds
    pub read_time_ns: u64,
    pub write_time_ns: u64,
    pub read_errors: u64,
    pub write_errors: u64,
}

/// Every accelerator the registry knows, or None when `ioreg` failed or
/// timed out (an empty list means it ran and found none)
#[cfg(target_os = "macos")]
pub(crate) fn gpus() -> Option<Vec<GpuSample>> {
    // Depth 1: the accelerator node carries everything itself, and its
    // subtree (surfaces, clients) is large
    let output = run_ioreg(&["-r", "-d", "1", "-c", "IOAccelerator", "-a"])?;
    Some(parse_gpus(&output))
}

/// Every block-storage driver with a whole-disk media child, or None
/// when `ioreg` failed or timed out
#[cfg(target_os = "macos")]
pub(crate) fn drives() -> Option<Vec<DriveCounters>> {
    // Depth 2 with -l: the statistics sit on the driver, the BSD name on
    // its IOMedia child, and children only carry properties under -l
    let output = run_ioreg(&["-r", "-d", "2", "-l", "-c", "IOBlockStorageDriver", "-a"])?;
    Some(parse_drives(&output))
}

/// No IORegistry off macOS
#[cfg(not(target_os = "macos"))]
pub(crate) fn gpus() -> Option<Vec<GpuSample>> {
    None
}

/// No IORegistry off macOS
#[cfg(not(target_os = "macos"))]
pub(crate) fn drives() -> Option<Vec<DriveCounters>> {
    None
}

#[cfg(target_os = "macos")]
fn run_ioreg(args: &[&str]) -> Option<Value> {
    let mut command = Command::new("ioreg");
    command.args(args);
    let bytes = run_with_timeout(command, IOREG_TIMEOUT)?;
    Value::from_reader(std::io::Cursor::new(bytes)).ok()
}

#[cfg(target_os = "macos")]
/// Run a child to completion, or kill it at the deadline. Stdout is
/// drained on a helper thread so a child that produces output slowly
/// cannot block the poll loop, and the kill closes the pipe so the
/// helper always terminates
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buffer = reader.join().ok()?;
                return status.success().then_some(buffer);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
}

#[cfg(target_os = "macos")]
/// The subtree roots `ioreg -r -a` prints: an array of dictionaries
fn roots(value: &Value) -> impl Iterator<Item = &Dictionary> {
    value
        .as_array()
        .map(|entries| entries.iter())
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
}

#[cfg(target_os = "macos")]
fn u64_of(dict: &Dictionary, key: &str) -> Option<u64> {
    let value = dict.get(key)?;
    value.as_unsigned_integer().or_else(|| {
        value
            .as_signed_integer()
            .and_then(|v| u64::try_from(v).ok())
    })
}

#[cfg(target_os = "macos")]
fn percent_of(dict: &Dictionary, key: &str) -> Option<f32> {
    let value = dict.get(key)?;
    value
        .as_unsigned_integer()
        .map(|v| v as f32)
        .or_else(|| value.as_signed_integer().map(|v| v as f32))
        .or_else(|| value.as_real().map(|v| v as f32))
}

#[cfg(target_os = "macos")]
fn string_of(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key)
        .and_then(Value::as_string)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_gpus(value: &Value) -> Vec<GpuSample> {
    roots(value)
        .filter_map(|node| {
            let stats = node.get("PerformanceStatistics")?.as_dictionary()?;
            // The one field every accelerator publishes; a node without it
            // is not a GPU we can say anything about
            let utilization_percent = percent_of(stats, "Device Utilization %")?;
            Some(GpuSample {
                name: string_of(node, "model")
                    .or_else(|| string_of(node, "IORegistryEntryName"))
                    .unwrap_or_else(|| "GPU".to_string()),
                cores: u64_of(node, "gpu-core-count").and_then(|c| u32::try_from(c).ok()),
                utilization_percent,
                renderer_utilization_percent: percent_of(stats, "Renderer Utilization %"),
                tiler_utilization_percent: percent_of(stats, "Tiler Utilization %"),
                memory_in_use_bytes: u64_of(stats, "In use system memory"),
                memory_allocated_bytes: u64_of(stats, "Alloc system memory"),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_drives(value: &Value) -> Vec<DriveCounters> {
    roots(value)
        .filter_map(|driver| {
            let stats = driver.get("Statistics")?.as_dictionary()?;
            // The whole-disk media is the child that carries the BSD name;
            // a driver without one (mid-eject, or a synthesized device
            // with no media yet) has nothing to key the counters on
            let media = driver
                .get("IORegistryEntryChildren")?
                .as_array()?
                .iter()
                .filter_map(Value::as_dictionary)
                .find(|child| child.get("BSD Name").is_some())?;
            let name = string_of(media, "BSD Name")?;
            let model = string_of(media, "IORegistryEntryName")
                .map(|n| n.strip_suffix(" Media").unwrap_or(&n).to_string())
                .filter(|n| !n.is_empty());
            Some(DriveCounters {
                name,
                model,
                size_bytes: u64_of(media, "Size"),
                is_removable: media
                    .get("Removable")
                    .and_then(Value::as_boolean)
                    .unwrap_or(false),
                read_ops: u64_of(stats, "Operations (Read)").unwrap_or(0),
                write_ops: u64_of(stats, "Operations (Write)").unwrap_or(0),
                read_bytes: u64_of(stats, "Bytes (Read)").unwrap_or(0),
                write_bytes: u64_of(stats, "Bytes (Write)").unwrap_or(0),
                read_time_ns: u64_of(stats, "Total Time (Read)").unwrap_or(0),
                write_time_ns: u64_of(stats, "Total Time (Write)").unwrap_or(0),
                read_errors: u64_of(stats, "Errors (Read)").unwrap_or(0),
                write_errors: u64_of(stats, "Errors (Write)").unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn plist(body: &str) -> Value {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array>{body}</array></plist>"#
        );
        Value::from_reader(std::io::Cursor::new(xml.into_bytes())).expect("valid plist")
    }

    /// Trimmed from a live `ioreg -r -d 1 -c IOAccelerator -a` on an M4 Pro
    const ACCELERATOR: &str = r#"<dict>
        <key>IOClass</key><string>AGXAcceleratorG16X</string>
        <key>IORegistryEntryName</key><string>AGXAcceleratorG16X</string>
        <key>model</key><string>Apple M4 Pro</string>
        <key>gpu-core-count</key><integer>16</integer>
        <key>PerformanceStatistics</key><dict>
            <key>Alloc system memory</key><integer>8970174464</integer>
            <key>Device Utilization %</key><integer>12</integer>
            <key>In use system memory</key><integer>1270726656</integer>
            <key>In use system memory (driver)</key><integer>0</integer>
            <key>Renderer Utilization %</key><integer>12</integer>
            <key>Tiler Utilization %</key><integer>7</integer>
        </dict>
    </dict>"#;

    /// Trimmed from `ioreg -r -d 2 -l -c IOBlockStorageDriver -a`: the
    /// internal SSD, a mounted disk image, and a driver with no media
    const DRIVERS: &str = r#"<dict>
        <key>IOClass</key><string>IOBlockStorageDriver</string>
        <key>Statistics</key><dict>
            <key>Bytes (Read)</key><integer>2261645545472</integer>
            <key>Bytes (Write)</key><integer>1543807442944</integer>
            <key>Errors (Read)</key><integer>0</integer>
            <key>Errors (Write)</key><integer>3</integer>
            <key>Latency Time (Read)</key><integer>0</integer>
            <key>Latency Time (Write)</key><integer>0</integer>
            <key>Operations (Read)</key><integer>116659174</integer>
            <key>Operations (Write)</key><integer>49028898</integer>
            <key>Total Time (Read)</key><integer>23710349454630</integer>
            <key>Total Time (Write)</key><integer>2295873010671</integer>
        </dict>
        <key>IORegistryEntryChildren</key><array><dict>
            <key>BSD Name</key><string>disk0</string>
            <key>Content</key><string>GUID_partition_scheme</string>
            <key>Ejectable</key><false/>
            <key>IORegistryEntryName</key><string>APPLE SSD AP0512Z Media</string>
            <key>Removable</key><false/>
            <key>Size</key><integer>500277792768</integer>
            <key>Whole</key><true/>
        </dict></array>
    </dict>
    <dict>
        <key>IOClass</key><string>IOBlockStorageDriver</string>
        <key>Statistics</key><dict>
            <key>Bytes (Read)</key><integer>0</integer>
            <key>Bytes (Write)</key><integer>0</integer>
            <key>Operations (Read)</key><integer>0</integer>
            <key>Operations (Write)</key><integer>0</integer>
            <key>Total Time (Read)</key><integer>0</integer>
            <key>Total Time (Write)</key><integer>0</integer>
        </dict>
        <key>IORegistryEntryChildren</key><array><dict>
            <key>BSD Name</key><string>disk4</string>
            <key>IORegistryEntryName</key><string>Apple Disk Image Media</string>
            <key>Removable</key><true/>
            <key>Size</key><integer>32545280</integer>
            <key>Whole</key><true/>
        </dict></array>
    </dict>
    <dict>
        <key>IOClass</key><string>IOBlockStorageDriver</string>
        <key>Statistics</key><dict>
            <key>Operations (Read)</key><integer>5</integer>
        </dict>
    </dict>"#;

    #[test]
    fn accelerator_node_becomes_a_gpu_sample() {
        let gpus = parse_gpus(&plist(ACCELERATOR));
        assert_eq!(
            gpus,
            vec![GpuSample {
                name: "Apple M4 Pro".into(),
                cores: Some(16),
                utilization_percent: 12.0,
                renderer_utilization_percent: Some(12.0),
                tiler_utilization_percent: Some(7.0),
                memory_in_use_bytes: Some(1_270_726_656),
                memory_allocated_bytes: Some(8_970_174_464),
            }]
        );
    }

    #[test]
    fn a_node_without_device_utilization_is_not_a_gpu() {
        let gpus = parse_gpus(&plist(
            r#"<dict><key>model</key><string>Something</string>
               <key>PerformanceStatistics</key><dict><key>Other</key><integer>1</integer></dict></dict>"#,
        ));
        assert!(gpus.is_empty());
    }

    #[test]
    fn drivers_key_on_the_media_bsd_name_and_skip_media_less_ones() {
        let drives = parse_drives(&plist(DRIVERS));
        assert_eq!(drives.len(), 2, "the driver with no media is skipped");
        let ssd = &drives[0];
        assert_eq!(ssd.name, "disk0");
        assert_eq!(ssd.model.as_deref(), Some("APPLE SSD AP0512Z"));
        assert_eq!(ssd.size_bytes, Some(500_277_792_768));
        assert!(!ssd.is_removable);
        assert_eq!(ssd.read_ops, 116_659_174);
        assert_eq!(ssd.write_ops, 49_028_898);
        assert_eq!(ssd.read_bytes, 2_261_645_545_472);
        assert_eq!(ssd.read_time_ns, 23_710_349_454_630);
        assert_eq!(ssd.write_time_ns, 2_295_873_010_671);
        assert_eq!(ssd.write_errors, 3);

        let image = &drives[1];
        assert_eq!(image.name, "disk4");
        assert_eq!(image.model.as_deref(), Some("Apple Disk Image"));
        assert!(image.is_removable);
        assert_eq!(image.read_ops, 0);
    }

    #[test]
    fn garbage_is_not_a_registry() {
        assert!(parse_gpus(&Value::String("nope".into())).is_empty());
        assert!(parse_drives(&Value::Integer(1.into())).is_empty());
    }

    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();
        assert!(run_with_timeout(command, Duration::from_millis(50)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_finished_child_returns_its_output() {
        let mut command = Command::new("echo");
        command.arg("hello");
        assert_eq!(
            run_with_timeout(command, Duration::from_secs(5)),
            Some(b"hello\n".to_vec())
        );
    }

    #[test]
    fn a_failing_child_returns_nothing() {
        let command = Command::new("false");
        assert!(run_with_timeout(command, Duration::from_secs(5)).is_none());
    }

    /// The real registry on the reference platform: whatever it holds
    /// must parse, and the internal drive must be there
    #[test]
    fn live_registry_parses() {
        let gpus = gpus().expect("ioreg runs");
        for gpu in &gpus {
            assert!((0.0..=100.0).contains(&gpu.utilization_percent));
        }
        let drives = drives().expect("ioreg runs");
        assert!(drives.iter().any(|d| d.name == "disk0"));
    }
}

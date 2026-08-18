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

//! Collector configuration.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Parse a human-friendly duration: `"500ms"`, `"2s"`, `"5m"`, `"1h"`,
/// or a bare integer meaning milliseconds
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("empty duration".into());
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        let ms: u64 = v
            .parse()
            .map_err(|_| format!("invalid duration: {value}"))?;
        return Ok(Duration::from_millis(ms));
    }
    let unit_start = v
        .find(|c: char| !c.is_ascii_digit())
        .expect("checked: not all digits");
    let (number, unit) = v.split_at(unit_start);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration: {value}"))?;
    let ms = match unit.trim() {
        "ms" => n,
        "s" => n * 1_000,
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        other => {
            return Err(format!(
                "invalid duration unit: {other} (use ms, s, m or h)"
            ));
        }
    };
    Ok(Duration::from_millis(ms))
}

/// The most compact exact representation: `1h`, `5m`, `90s`, `1500ms`
pub fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis() as u64;
    if ms == 0 {
        return "0s".to_string();
    }
    if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// Serde representation for human-friendly durations: serializes as a
/// string like `"60s"`; deserializes from such strings or from a bare
/// integer meaning milliseconds
pub mod duration_serde {
    use std::time::Duration;

    use serde::{Deserializer, Serializer, de};

    use super::{format_duration, parse_duration};

    pub fn serialize<S: Serializer>(duration: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(*duration))
    }

    struct DurationVisitor;

    impl de::Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str(
                "a duration like \"500ms\", \"2s\", \"5m\", \"1h\", or milliseconds as an integer",
            )
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Duration, E> {
            Ok(Duration::from_millis(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Duration, E> {
            u64::try_from(v)
                .map(Duration::from_millis)
                .map_err(|_| E::custom("duration must not be negative"))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Duration, E> {
            parse_duration(v).map_err(E::custom)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        d.deserialize_any(DurationVisitor)
    }
}

/// Parse a human-friendly byte size: `"4GiB"`, `"512MiB"`, `"2GB"`, or
/// a bare integer meaning bytes.
///
/// Binary and decimal units are both accepted and mean what they say
/// (`GiB` = 1024³, `GB` = 1000³); a bare `G`/`M`/`K` is read as the
/// binary one, since that is what every memory reading on this platform
/// is quoted in
pub fn parse_size(value: &str) -> Result<u64, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("empty size".into());
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        return v.parse().map_err(|_| format!("invalid size: {value}"));
    }
    let unit_start = v
        .find(|c: char| !c.is_ascii_digit())
        .expect("checked: not all digits");
    let (number, unit) = v.split_at(unit_start);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid size: {value}"))?;
    let factor: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1u64 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "invalid size unit: {other} (use B, KiB, MiB, GiB, TiB or their KB/MB/GB forms)"
            ));
        }
    };
    n.checked_mul(factor)
        .ok_or_else(|| format!("size out of range: {value}"))
}

/// The most compact exact binary representation: `4GiB`, `512MiB`,
/// `1536B`
pub fn format_size(bytes: u64) -> String {
    for (factor, unit) in [
        (1u64 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ] {
        if bytes >= factor && bytes.is_multiple_of(factor) {
            return format!("{}{unit}", bytes / factor);
        }
    }
    format!("{bytes}B")
}

/// Serde representation for human-friendly byte sizes on `Option<u64>`
/// fields: serializes as `"4GiB"`, deserializes from such strings or
/// from a bare integer meaning bytes
pub mod option_size_serde {
    use serde::{Deserializer, Serializer, de};

    use super::{format_size, parse_size};

    pub fn serialize<S: Serializer>(bytes: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => s.serialize_str(&format_size(*b)),
            None => s.serialize_none(),
        }
    }

    struct SizeVisitor;

    impl de::Visitor<'_> for SizeVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a size like \"4GiB\", \"512MiB\", or bytes as an integer")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::custom("size must not be negative"))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            parse_size(v).map_err(E::custom)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        d.deserialize_any(SizeVisitor).map(Some)
    }
}

/// [`duration_serde`] for `Option<Duration>` fields (combine with
/// `#[serde(default, skip_serializing_if = "Option::is_none")]`)
pub mod option_duration_serde {
    use std::time::Duration;

    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(duration: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match duration {
            Some(d) => super::duration_serde::serialize(d, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        super::duration_serde::deserialize(d).map(Some)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    /// Whether to collect process info (relatively expensive)
    pub collect_processes: bool,

    /// How often to refresh the process list. Zero (the default) refreshes
    /// on every collect. A tray-style embedder that only needs a coarse
    /// "top processes" view can set e.g. 10s: between refreshes the
    /// snapshot carries the last collected list, and per-process CPU% is
    /// averaged over the longer window (smoother rankings)
    #[serde(with = "duration_serde")]
    pub process_refresh_interval: Duration,

    /// While overall CPU load is at or above this many *logical cores* of
    /// work, the process list refreshes on every collect regardless of
    /// `process_refresh_interval` — busy periods get precise per-process
    /// attribution, idle periods stay cheap. None or a value <= 0
    /// disables the boost (0 is what the config file round-trips, since
    /// an absent field would revert to the default on reload).
    ///
    /// Units match per-process CPU% (single-core units): `1.0` means "at
    /// least one full core busy". The effective overall-usage threshold is
    /// therefore `cores / logical_cores * 100`, so the same setting scales
    /// across machine sizes.
    ///
    /// Default `Some(2.0)`: a modern desktop's ambient load (browser +
    /// language servers + background services) is already around one full
    /// core, so 1.0 would keep the boost pinned on during any working
    /// session — 2.0 means "ambient plus a whole extra core of anomaly".
    /// Alert correctness does not depend on the boost (window averages
    /// are time-weighted over whatever cadence is active); it only buys
    /// finer per-process attribution while things are busy. Raise it
    /// (e.g. 4.0) when baseline load is high and you want fewer forced
    /// process refreshes.
    pub process_boost_cpu_cores: Option<f32>,

    /// Max number of processes to keep. The budget is split between the
    /// top-by-CPU and top-by-memory rankings so that both views stay
    /// meaningful; the returned list is sorted by CPU desc
    pub max_processes: usize,

    /// Also collect per-process disk read/write byte rates. Off by default
    /// because it requires an extra refresh kind on every process pass.
    /// When disabled, `ProcessSnapshot::{read,write}_bytes_per_sec` stay
    /// `None`.
    pub collect_process_disk_io: bool,

    /// Also aggregate whole process trees into per-application groups
    /// (`SystemSnapshot::process_groups`): root = a direct child of
    /// init/launchd, summed over every descendant. Computed from the full
    /// process table during the process refresh (one extra in-memory pass,
    /// no additional system calls) and capped at `max_processes` groups
    /// ranked by CPU. Requires `collect_processes`.
    pub collect_process_groups: bool,

    /// Whether to collect per-core CPU usage
    pub per_core_cpu: bool,

    /// How often to refresh CPU frequency. Frequency changes slowly relative
    /// to usage; refreshing it every collect is wasted work. Usage is still
    /// refreshed on every collect. Zero refreshes frequency every collect.
    #[serde(with = "duration_serde")]
    pub cpu_frequency_refresh_interval: Duration,

    /// Whether to collect disk metrics
    pub collect_disks: bool,

    /// How often to refresh disk capacity (total/available bytes). The
    /// capacity query is by far the most expensive part of disk collection
    /// (~18ms per round on macOS vs ~0.7ms for IO counters) and the data
    /// barely changes, so it runs on its own slower cadence; IO counters
    /// still refresh on every collect. Capacity values between refreshes
    /// are the last known ones
    #[serde(with = "duration_serde")]
    pub disk_storage_refresh_interval: Duration,

    /// How often to refresh disk IO counters and rebuild disk snapshots.
    /// Zero (the default) refreshes on every collect. Between refreshes the
    /// last snapshot list (including rates) is reused; rate diffs span the
    /// time since the previous disk refresh
    #[serde(with = "duration_serde")]
    pub disk_io_refresh_interval: Duration,

    /// When true, keep a single entry per disk device name (preferring the
    /// shortest mount point). Collapses APFS synthetic mounts such as `/`
    /// and `/System/Volumes/Data` that report the same underlying volume.
    pub dedupe_disks: bool,

    /// Whether to collect network interface metrics
    pub collect_networks: bool,

    /// How often to refresh network counters and rebuild network snapshots.
    /// Zero (the default) refreshes on every collect; same reuse semantics
    /// as `disk_io_refresh_interval`
    #[serde(with = "duration_serde")]
    pub network_refresh_interval: Duration,

    /// Whether to collect battery state. Machines without a battery
    /// simply report `None`.
    ///
    /// The field that earns this its keep is `power_watts`: it changes
    /// second to second with load and macOS surfaces it nowhere, so it
    /// is what pairs with the CPU numbers ("150% CPU while drawing
    /// 22 W"). Charge percentage duplicates the menu bar, and health and
    /// cycle count move over months — they ride along free in the same
    /// read as a reference value, not as a time series. Deliberately no
    /// alert rule: low-battery warnings are the OS's job (see CLAUDE.md).
    pub collect_battery: bool,

    /// How often to refresh the battery (~0.6ms per read). Zero
    /// refreshes every collect.
    #[serde(with = "duration_serde")]
    pub battery_refresh_interval: Duration,

    /// Whether to collect hardware temperature sensors (sysinfo Components).
    /// Platform-dependent: macOS returns many named sensors with occasional
    /// garbage values (filtered); some environments report none.
    pub collect_temperatures: bool,

    /// How often to refresh temperatures. Sensors change slowly compared to
    /// CPU counters, so a multi-second cadence is enough and avoids extra
    /// IOKit/hwmon work every collect. Between refreshes the last reading
    /// is reused. Zero refreshes every collect.
    #[serde(with = "duration_serde")]
    pub temperature_refresh_interval: Duration,

    /// Custom host labels
    pub labels: HashMap<String, String>,

    /// Collect timeout (guards against platform calls that hang)
    #[serde(with = "duration_serde")]
    pub collect_timeout: Duration,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            collect_processes: true,
            process_refresh_interval: Duration::ZERO,
            process_boost_cpu_cores: Some(2.0),
            max_processes: 50,
            collect_process_disk_io: false,
            collect_process_groups: true,
            per_core_cpu: true,
            cpu_frequency_refresh_interval: Duration::from_secs(30),
            collect_disks: true,
            disk_storage_refresh_interval: Duration::from_secs(60),
            disk_io_refresh_interval: Duration::ZERO,
            dedupe_disks: true,
            collect_networks: true,
            network_refresh_interval: Duration::ZERO,
            collect_battery: true,
            battery_refresh_interval: Duration::from_secs(30),
            // Off by default on Windows only. `sysinfo` reads
            // temperatures there through WMI
            // (`MSAcpi_ThermalZoneTemperature`), which calls
            // `CoInitializeEx` on the calling thread and sets a
            // PROCESS-GLOBAL `CoInitializeSecurity` — on the collector
            // thread, every cadence. For an embedded frontend that is a
            // real integration hazard, since the host may have its own
            // COM apartment or security requirements. The payload it
            // buys is at most one component hard-labelled "Computer",
            // and on most consumer hardware none at all. Set it to true
            // explicitly if you want it
            collect_temperatures: !cfg!(target_os = "windows"),
            // Temps drift over seconds/minutes, not milliseconds
            temperature_refresh_interval: Duration::from_secs(15),
            labels: HashMap::new(),
            collect_timeout: Duration::from_secs(2),
        }
    }
}

#[cfg(feature = "config")]
impl CollectorConfig {
    /// Load collector settings from `<dir>/config.toml`'s `[collector]`
    /// section (durations are integer milliseconds). A missing file yields
    /// the defaults; other sections and unknown keys are ignored, so the
    /// same file can carry application-level settings alongside.
    ///
    /// ```toml
    /// [collector]
    /// process_refresh_interval = 10000
    /// disk_io_refresh_interval = 10000
    /// collect_temperatures = false
    /// ```
    pub fn load_from_dir(dir: impl AsRef<std::path::Path>) -> Result<Self, crate::ConfigError> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct FileRoot {
            collector: CollectorConfig,
        }

        let path = dir.as_ref().join("config.toml");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(crate::ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let root: FileRoot = toml::from_str(&content).map_err(|e| crate::ConfigError::Parse {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Ok(root.collector)
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn parse_duration_accepts_units_and_bare_millis() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1500").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration(" 10s ").unwrap(), Duration::from_secs(10));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1.5s").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("s").is_err());
    }

    #[test]
    fn format_duration_picks_compact_unit() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
    }
}

#[cfg(all(test, feature = "config"))]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_from_both_unit_families_and_round_trip() {
        assert_eq!(parse_size("4GiB"), Ok(4 << 30));
        assert_eq!(parse_size("4G"), Ok(4 << 30), "bare units are binary");
        assert_eq!(parse_size("512MiB"), Ok(512 << 20));
        assert_eq!(
            parse_size("2GB"),
            Ok(2_000_000_000),
            "GB means what it says"
        );
        assert_eq!(parse_size("1024"), Ok(1024), "a bare number is bytes");
        assert!(parse_size("4 gigs").is_err());
        assert!(parse_size("").is_err());
        assert!(
            parse_size("99999999999999999999GiB").is_err(),
            "no wraparound"
        );

        assert_eq!(format_size(4 << 30), "4GiB");
        assert_eq!(format_size(1536), "1536B", "not a lying 1.5KiB");
        assert_eq!(format_size(0), "0B");
        for text in ["4GiB", "512MiB", "1536B"] {
            assert_eq!(format_size(parse_size(text).expect("parse")), text);
        }
    }

    #[test]
    fn load_from_dir_reads_collector_section() {
        let dir = std::env::temp_dir().join(format!("zstats-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        std::fs::write(
            dir.join("config.toml"),
            r#"
[collector]
collect_disks = false
process_refresh_interval = 5000
network_refresh_interval = "10s"

[alerts]
cpu = 40.0
"#,
        )
        .expect("write config");

        let config = CollectorConfig::load_from_dir(&dir).expect("load");
        assert!(!config.collect_disks);
        // Bare integers stay milliseconds; unit strings also work
        assert_eq!(config.process_refresh_interval, Duration::from_secs(5));
        assert_eq!(config.network_refresh_interval, Duration::from_secs(10));
        // Untouched fields keep their defaults
        assert!(config.collect_networks);
        assert_eq!(config.collect_timeout, Duration::from_secs(2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_dir_yields_defaults() {
        let dir = std::env::temp_dir().join("zstats-config-test-does-not-exist");
        let config = CollectorConfig::load_from_dir(&dir).expect("defaults");
        assert!(config.collect_disks);
    }

    #[test]
    fn durations_roundtrip_as_human_strings() {
        let config = CollectorConfig::default();
        let toml = toml::to_string(&config).expect("serialize");
        assert!(toml.contains(r#"disk_storage_refresh_interval = "1m""#));
        assert!(toml.contains(r#"collect_timeout = "2s""#));
        let back: CollectorConfig = toml::from_str(&toml).expect("reparse");
        assert_eq!(back.disk_storage_refresh_interval, Duration::from_secs(60));
    }
}

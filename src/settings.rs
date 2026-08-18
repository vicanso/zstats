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

//! The full `<config-dir>/config.toml` model shared by every frontend:
//! `[collector]` (toggles and cadences), `[daemon]` (how the daemon
//! runs), and `[alerts]`. The CLI edits it through `-add` / `-remove`
//! (via [`apply_add`] / [`apply_remove`], so a GUI settings panel gets
//! the identical key set and validation); the daemon loads it at startup
//! and hot-reloads the alerts section.
//!
//! Unlike [`crate::config::CollectorConfig::load_from_dir`] — which reads
//! only the `[collector]` section for embedders — this module owns the
//! whole file, including reading AND writing it back.
//!
//! Every function takes the config directory explicitly; the conventional
//! default is [`default_dir`] (`~/.zstats`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CollectorConfig;
use crate::error::{ConfigError, ParseConfigSnafu, ReadConfigSnafu, WriteConfigSnafu};
use snafu::ResultExt as _;

/// The conventional config directory: `~/.zstats` (`HOME` on unix,
/// `USERPROFILE` on Windows).
///
/// Every frontend — CLI and GUI alike — uses this same directory by
/// design: shared config and metrics history between frontends depend on
/// there being exactly one location. Deliberately NOT the platform-native
/// app dirs (Application Support / XDG / APPDATA); a GUI must not invent
/// its own location
pub fn default_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zstats")
}

/// The config file inside a config directory
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

/// The optional alert-template override inside a config directory.
///
/// Absent on a normal install — the compiled-in `templates/alerts.toml`
/// is used. Dropping a file here REPLACES it wholesale (rather than
/// layering, which would make a removed entry impossible to remove and
/// turn three precedence levels into five). To ADD a single entry, use a
/// user override — `zstats -add alert-cpu 'name=pct'` — which outranks
/// either template anyway. Keeping it a plain file is what makes
/// "refresh the table on a schedule" a one-line cron job (`curl -o`)
/// instead of an HTTP client inside a local metrics collector
pub fn template_path(dir: &Path) -> PathBuf {
    dir.join("template.toml")
}

/// Which kernel memory-pressure verdict is worth a notification.
///
/// A memory-heavy machine can sit at `warning` as its normal operating
/// state, in which case only `critical` carries information — hence the
/// choice. Deserializes from `"off"`/`"warning"`/`"critical"` and also
/// from the booleans this key originally accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PressureAlert {
    Off,
    /// The kernel's "warning" verdict (level 2) and worse
    Warning,
    /// Only the kernel's "critical" verdict (level 4)
    Critical,
}

impl PressureAlert {
    /// Kernel level at which this setting starts alerting; None = off
    pub fn level(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Warning => Some(2),
            Self::Critical => Some(4),
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "none" => Ok(Self::Off),
            "warning" | "warn" | "true" | "1" => Ok(Self::Warning),
            "critical" | "crit" => Ok(Self::Critical),
            other => Err(format!("{other} (use off|warning|critical)")),
        }
    }
}

impl<'de> Deserialize<'de> for PressureAlert {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `pressure = true` predates the three-way setting; keep it working
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bool(bool),
            Name(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Bool(true) => Ok(Self::Warning),
            Repr::Bool(false) => Ok(Self::Off),
            Repr::Name(name) => Self::parse(&name).map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// Collector settings (`[collector]`). Kept as an Option so a file
    /// without the section round-trips without freezing the builtin
    /// defaults into it
    pub collector: Option<CollectorConfig>,
    pub daemon: DaemonConfig,
    pub alerts: AlertsConfig,
}

/// The `[daemon]` section: how the daemon runs. Durations accept
/// `"500ms"`, `"2s"`, `"5m"`, `"1h"`, or a bare integer meaning
/// milliseconds
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Collection interval; absent = builtin default (2s)
    #[serde(
        default,
        with = "crate::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval: Option<std::time::Duration>,
    /// History retention for attach replay; absent = builtin default (5m)
    #[serde(
        default,
        with = "crate::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub history: Option<std::time::Duration>,
    /// Start the daemon detached (the default); --foreground overrides
    pub detach: Option<bool>,
}

/// The `[alerts]` section. CPU thresholds are in single-core percent,
/// memory in percent of total. A value of 0 disables the rule (or, in an
/// override, disables it for that process only).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    /// Default CPU threshold; absent = builtin default (30)
    pub cpu: Option<f32>,
    /// Default memory threshold as a percentage of total; absent =
    /// builtin default (25). Combined with [`Self::mem_bytes`] — the
    /// process only has to reach whichever of the two is lower
    pub mem: Option<f64>,
    /// Absolute ceiling for the same rule, in bytes; absent = builtin
    /// default (4 GiB), 0 = no absolute ceiling.
    ///
    /// A percentage alone scales the wrong way — 25% of 8 GiB is a
    /// browser tab, 25% of 64 GiB is never reached before the machine is
    /// already swapping — so the effective bar is the LOWER of the two:
    /// the percentage protects small machines, this protects large ones
    #[serde(
        default,
        with = "crate::config::option_size_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub mem_bytes: Option<u64>,
    /// Re-alert cooldown; absent = builtin default (10m)
    #[serde(
        default,
        with = "crate::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub cooldown: Option<std::time::Duration>,
    /// Volume used-capacity threshold in percent; absent = builtin
    /// default (90). Fires once per upward crossing, re-arms 5 points
    /// below. 0 disables
    pub disk: Option<f32>,
    /// Which kernel memory-pressure verdict starts alerting (macOS);
    /// absent = warning. The levels are the kernel's own — the only
    /// choice is which one is worth interrupting for
    pub pressure: Option<PressureAlert>,
    /// Whole-application CPU threshold in single-core percent; absent =
    /// builtin default (200, i.e. two cores). Catches the multi-process
    /// app whose members each stay under the per-process bar. 0 disables
    pub app_cpu: Option<f32>,
    /// Whole-application memory threshold as a percentage of total;
    /// absent = builtin default (40). Combined with
    /// [`Self::app_mem_bytes`] exactly like [`Self::mem`] is with
    /// [`Self::mem_bytes`] — the group only has to reach the lower of
    /// the two. 0 disables
    pub app_mem: Option<f64>,
    /// Absolute ceiling for the whole-app memory rule, in bytes; absent
    /// = builtin default (8 GiB), 0 = no absolute ceiling.
    ///
    /// Without it the rule is unreachable on the machines it was written
    /// for: 40% is 9.6 GiB on a 24 GiB laptop and 25.6 GiB on a 64 GiB
    /// desktop, so the browser holding gigabytes across dozens of
    /// helpers never qualified
    #[serde(
        default,
        with = "crate::config::option_size_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub app_mem_bytes: Option<u64>,
    /// Per-process CPU overrides, keyed by process name
    pub cpu_overrides: BTreeMap<String, f32>,
    /// Per-process memory overrides, keyed by process name
    pub mem_overrides: BTreeMap<String, f64>,
    /// Per-volume disk overrides, keyed by mount point (backup volumes
    /// run full by design — disable them with 0)
    pub disk_overrides: BTreeMap<String, f32>,
    /// Per-application overrides for the whole-app rules, keyed by the
    /// group's root process name
    pub app_cpu_overrides: BTreeMap<String, f32>,
    pub app_mem_overrides: BTreeMap<String, f64>,
    /// Apply the builtin per-app override template
    /// (`zstats::alerts::TEMPLATE_CPU_OVERRIDES`) beneath the user's own
    /// overrides; absent = true. Set false for a pure user config
    pub template: Option<bool>,
}

/// Keys accepted by [`apply_add`] / [`apply_remove`], for error messages
/// and interactive key listings
pub const KNOWN_KEYS: &str = "interval, history, detach, process-interval, \
disk-interval, disk-storage-interval, network-interval, temp-interval, \
cpu-freq-interval, battery-interval, process-boost, max-processes, \
collect-processes, collect-disks, collect-networks, collect-temperatures, \
collect-battery, process-disk-io, \
process-groups, dedupe-disks, per-core-cpu, alert-cpu, alert-mem, \
alert-mem-bytes, alert-app-cpu, alert-app-mem, alert-app-mem-bytes, \
alert-disk, alert-pressure, \
alert-cooldown, alert-template";

fn parse<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("invalid value for {key}: {value}"))
}

/// Reject an override key the matcher cannot honour, so a key that
/// looks like a pattern is never silently stored as an exact name.
/// A leading and/or trailing `*` is fine; anything else is not
fn check_name_pattern(key: &str, name: &str) -> Result<(), String> {
    crate::alerts::Matcher::parse(name)
        .map(|_| ())
        .map_err(|e| format!("invalid value for {key}: {e}"))
}

fn collector_mut(config: &mut FileConfig) -> &mut CollectorConfig {
    config
        .collector
        .get_or_insert_with(CollectorConfig::default)
}

/// Apply an `<key> <value>` setting to the config (the CLI's `-add`).
/// `alert-cpu` / `alert-mem` accept either a bare percentage (rule
/// default) or `name=pct` (per-process override). Returns a
/// human-readable description of the change; errors are human-readable
/// validation messages
pub fn apply_add(config: &mut FileConfig, key: &str, value: &str) -> Result<String, String> {
    use std::time::Duration;

    let millis = |key: &str, value: &str| -> Result<Duration, String> {
        crate::config::parse_duration(value).map_err(|e| format!("{key}: {e}"))
    };

    match key {
        // [daemon]
        "interval" => {
            let interval = millis(key, value)?;
            if interval.is_zero() {
                return Err("interval must be greater than 0".into());
            }
            config.daemon.interval = Some(interval);
        }
        "history" => config.daemon.history = Some(millis(key, value)?),
        "detach" => config.daemon.detach = Some(parse(key, value)?),
        // [collector] cadences (milliseconds; 0 = every collect)
        "process-interval" => collector_mut(config).process_refresh_interval = millis(key, value)?,
        "disk-interval" => collector_mut(config).disk_io_refresh_interval = millis(key, value)?,
        "disk-storage-interval" => {
            collector_mut(config).disk_storage_refresh_interval = millis(key, value)?
        }
        "network-interval" => collector_mut(config).network_refresh_interval = millis(key, value)?,
        "temp-interval" => collector_mut(config).temperature_refresh_interval = millis(key, value)?,
        "battery-interval" => collector_mut(config).battery_refresh_interval = millis(key, value)?,
        "cpu-freq-interval" => {
            collector_mut(config).cpu_frequency_refresh_interval = millis(key, value)?
        }
        // [collector] numbers and toggles
        "process-boost" => {
            let cores: f32 = parse(key, value)?;
            // Store 0 as Some(0.0), NOT None: a None field is omitted from
            // the file and would silently revert to the default on reload.
            // The collector treats any threshold <= 0 as "boost disabled"
            if cores.is_nan() || cores < 0.0 {
                return Err(format!("invalid value for {key}: {value}"));
            }
            collector_mut(config).process_boost_cpu_cores = Some(cores);
        }
        "max-processes" => collector_mut(config).max_processes = parse(key, value)?,
        "collect-processes" => collector_mut(config).collect_processes = parse(key, value)?,
        "collect-disks" => collector_mut(config).collect_disks = parse(key, value)?,
        "collect-networks" => collector_mut(config).collect_networks = parse(key, value)?,
        "collect-temperatures" => collector_mut(config).collect_temperatures = parse(key, value)?,
        "collect-battery" => collector_mut(config).collect_battery = parse(key, value)?,
        "process-disk-io" => collector_mut(config).collect_process_disk_io = parse(key, value)?,
        "process-groups" => collector_mut(config).collect_process_groups = parse(key, value)?,
        "dedupe-disks" => collector_mut(config).dedupe_disks = parse(key, value)?,
        "per-core-cpu" => collector_mut(config).per_core_cpu = parse(key, value)?,
        // [alerts]
        "alert-app-cpu" | "alert-app-mem" => {
            if let Some((name, pct)) = value.split_once('=') {
                let name = name.trim();
                if name.is_empty() {
                    return Err(format!("invalid value for {key}: {value} (empty name)"));
                }
                check_name_pattern(key, name)?;
                let pct: f64 = parse(key, pct)?;
                if key == "alert-app-cpu" {
                    config
                        .alerts
                        .app_cpu_overrides
                        .insert(name.to_string(), pct as f32);
                } else {
                    config
                        .alerts
                        .app_mem_overrides
                        .insert(name.to_string(), pct);
                }
                return Ok(format!("{key} override: {name} = {pct}% (0 disables)"));
            }
            let pct: f64 = parse(key, value)?;
            if key == "alert-app-cpu" {
                config.alerts.app_cpu = Some(pct as f32);
            } else {
                config.alerts.app_mem = Some(pct);
            }
        }
        "alert-cpu" | "alert-mem" => {
            if let Some((name, pct)) = value.split_once('=') {
                let name = name.trim();
                if name.is_empty() {
                    return Err(format!("invalid value for {key}: {value} (empty name)"));
                }
                check_name_pattern(key, name)?;
                let pct: f64 = parse(key, pct)?;
                if key == "alert-cpu" {
                    config
                        .alerts
                        .cpu_overrides
                        .insert(name.to_string(), pct as f32);
                } else {
                    config.alerts.mem_overrides.insert(name.to_string(), pct);
                }
                return Ok(format!("{key} override: {name} = {pct}% (0 disables)"));
            }
            let pct: f64 = parse(key, value)?;
            if key == "alert-cpu" {
                config.alerts.cpu = Some(pct as f32);
            } else {
                config.alerts.mem = Some(pct);
            }
        }
        "alert-mem-bytes" | "alert-app-mem-bytes" => {
            let bytes = crate::config::parse_size(value)
                .map_err(|e| format!("invalid value for {key}: {e}"))?;
            let pct_key = if key == "alert-mem-bytes" {
                config.alerts.mem_bytes = Some(bytes);
                "alert-mem"
            } else {
                config.alerts.app_mem_bytes = Some(bytes);
                "alert-app-mem"
            };
            return Ok(format!(
                "{key} = {} (0 removes the absolute ceiling; the effective bar is \
                 the lower of this and {pct_key})",
                crate::config::format_size(bytes)
            ));
        }
        "alert-disk" => {
            if let Some((mount, pct)) = value.split_once('=') {
                let mount = mount.trim();
                if mount.is_empty() {
                    return Err(format!("invalid value for {key}: {value} (empty mount)"));
                }
                check_name_pattern(key, mount)?;
                let pct: f32 = parse(key, pct)?;
                config.alerts.disk_overrides.insert(mount.to_string(), pct);
                return Ok(format!("{key} override: {mount} = {pct}% (0 disables)"));
            }
            config.alerts.disk = Some(parse(key, value)?);
        }
        "alert-cooldown" => config.alerts.cooldown = Some(millis(key, value)?),
        "alert-pressure" => {
            config.alerts.pressure = Some(
                PressureAlert::parse(value).map_err(|e| format!("invalid value for {key}: {e}"))?,
            )
        }
        "alert-template" => config.alerts.template = Some(parse(key, value)?),
        other => return Err(format!("unknown key: {other} (known keys: {KNOWN_KEYS})")),
    }
    Ok(format!("{key} = {}", value.trim()))
}

/// Apply a `<key> [name]` removal (the CLI's `-remove`): reset a setting
/// to its builtin default, or drop a per-process alert override when
/// `name` is given
pub fn apply_remove(
    config: &mut FileConfig,
    key: &str,
    name: Option<&str>,
) -> Result<String, String> {
    if let Some(name) = name {
        let removed = match key {
            "alert-cpu" => config.alerts.cpu_overrides.remove(name).is_some(),
            "alert-mem" => config.alerts.mem_overrides.remove(name).is_some(),
            "alert-disk" => config.alerts.disk_overrides.remove(name).is_some(),
            "alert-app-cpu" => config.alerts.app_cpu_overrides.remove(name).is_some(),
            "alert-app-mem" => config.alerts.app_mem_overrides.remove(name).is_some(),
            other => return Err(format!("{other} has no per-name overrides")),
        };
        return if removed {
            Ok(format!("removed {key} override for {name}"))
        } else {
            Err(format!("no {key} override found for {name}"))
        };
    }

    let defaults = CollectorConfig::default();
    match key {
        "interval" => config.daemon.interval = None,
        "history" => config.daemon.history = None,
        "detach" => config.daemon.detach = None,
        "process-interval" => {
            collector_mut(config).process_refresh_interval = defaults.process_refresh_interval
        }
        "disk-interval" => {
            collector_mut(config).disk_io_refresh_interval = defaults.disk_io_refresh_interval
        }
        "disk-storage-interval" => {
            collector_mut(config).disk_storage_refresh_interval =
                defaults.disk_storage_refresh_interval
        }
        "network-interval" => {
            collector_mut(config).network_refresh_interval = defaults.network_refresh_interval
        }
        "temp-interval" => {
            collector_mut(config).temperature_refresh_interval =
                defaults.temperature_refresh_interval
        }
        "cpu-freq-interval" => {
            collector_mut(config).cpu_frequency_refresh_interval =
                defaults.cpu_frequency_refresh_interval
        }
        "process-boost" => {
            collector_mut(config).process_boost_cpu_cores = defaults.process_boost_cpu_cores
        }
        "max-processes" => collector_mut(config).max_processes = defaults.max_processes,
        "collect-processes" => collector_mut(config).collect_processes = defaults.collect_processes,
        "collect-disks" => collector_mut(config).collect_disks = defaults.collect_disks,
        "collect-networks" => collector_mut(config).collect_networks = defaults.collect_networks,
        "collect-temperatures" => {
            collector_mut(config).collect_temperatures = defaults.collect_temperatures
        }
        "collect-battery" => collector_mut(config).collect_battery = defaults.collect_battery,
        "battery-interval" => {
            collector_mut(config).battery_refresh_interval = defaults.battery_refresh_interval
        }
        "process-disk-io" => {
            collector_mut(config).collect_process_disk_io = defaults.collect_process_disk_io
        }
        "process-groups" => {
            collector_mut(config).collect_process_groups = defaults.collect_process_groups
        }
        "dedupe-disks" => collector_mut(config).dedupe_disks = defaults.dedupe_disks,
        "per-core-cpu" => collector_mut(config).per_core_cpu = defaults.per_core_cpu,
        "alert-cpu" => config.alerts.cpu = None,
        "alert-mem" => config.alerts.mem = None,
        "alert-mem-bytes" => config.alerts.mem_bytes = None,
        "alert-app-mem-bytes" => config.alerts.app_mem_bytes = None,
        "alert-app-cpu" => config.alerts.app_cpu = None,
        "alert-app-mem" => config.alerts.app_mem = None,
        "alert-disk" => config.alerts.disk = None,
        "alert-cooldown" => config.alerts.cooldown = None,
        "alert-pressure" => config.alerts.pressure = None,
        "alert-template" => config.alerts.template = None,
        other => return Err(format!("unknown key: {other} (known keys: {KNOWN_KEYS})")),
    }
    Ok(format!("{key} reset to default"))
}

/// Load `<dir>/config.toml`; a missing file is an empty config, a
/// malformed one is an error (so a typo doesn't silently drop the user's
/// settings)
pub fn load(dir: &Path) -> Result<FileConfig, ConfigError> {
    let path = config_path(dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(e) => {
            return Err(e).context(ReadConfigSnafu {
                path: path.display().to_string(),
            });
        }
    };
    toml::from_str(&content).map_err(|e| {
        ParseConfigSnafu {
            path: path.display().to_string(),
            message: e.to_string(),
        }
        .build()
    })
}

/// Load `<dir>/template.toml` if it exists, else the compiled-in
/// template. A malformed or wrong-version file is an error rather than a
/// silent fallback: an alert template that quietly did not apply is
/// indistinguishable from a quiet machine
pub fn load_template(dir: &Path) -> Result<crate::alerts::Template, ConfigError> {
    let path = template_path(dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::alerts::Template::builtin().clone());
        }
        Err(e) => {
            return Err(e).context(ReadConfigSnafu {
                path: path.display().to_string(),
            });
        }
    };
    crate::alerts::Template::parse(&content).map_err(|message| {
        ParseConfigSnafu {
            path: path.display().to_string(),
            message,
        }
        .build()
    })
}

/// Write the config back to `<dir>/config.toml`, creating the directory
/// as needed
pub fn save(dir: &Path, config: &FileConfig) -> Result<(), ConfigError> {
    let path = config_path(dir);
    let write_context = || WriteConfigSnafu {
        path: path.display().to_string(),
    };
    std::fs::create_dir_all(dir).with_context(|_| write_context())?;
    let content = toml::to_string_pretty(config).map_err(|e| {
        ParseConfigSnafu {
            path: path.display().to_string(),
            message: format!("serialize failed: {e}"),
        }
        .build()
    })?;
    std::fs::write(&path, content).with_context(|_| write_context())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alerts_section() {
        let config: FileConfig = toml::from_str(
            r#"
[alerts]
cpu = 40.0
cooldown = "5m"

[alerts.cpu_overrides]
ghostty = 100.0

[alerts.mem_overrides]
"Google Chrome" = 50.0
"#,
        )
        .expect("parse");
        assert_eq!(config.alerts.cpu, Some(40.0));
        assert_eq!(config.alerts.mem, None);
        assert_eq!(
            config.alerts.cooldown,
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(config.alerts.cpu_overrides["ghostty"], 100.0);
        assert_eq!(config.alerts.mem_overrides["Google Chrome"], 50.0);
    }

    #[test]
    fn empty_input_is_default() {
        let config: FileConfig = toml::from_str("").expect("parse empty");
        assert!(config.alerts.cpu.is_none());
        assert!(config.alerts.cpu_overrides.is_empty());
        assert!(config.collector.is_none());
        assert!(config.daemon.detach.is_none());
    }

    #[test]
    fn parses_collector_and_daemon_sections() {
        let config: FileConfig = toml::from_str(
            r#"
[collector]
collect_temperatures = false
network_refresh_interval = "10s"

[daemon]
interval = "5s"
detach = true
"#,
        )
        .expect("parse");
        let collector = config.collector.expect("collector section");
        assert!(!collector.collect_temperatures);
        assert_eq!(
            collector.network_refresh_interval,
            std::time::Duration::from_secs(10)
        );
        // Untouched collector fields keep builtin defaults
        assert!(collector.collect_disks);
        assert_eq!(
            config.daemon.interval,
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(config.daemon.detach, Some(true));
        assert_eq!(config.daemon.history, None);
    }

    fn settings_apply(config: &mut FileConfig, key: &str, value: &str) {
        apply_add(config, key, value).unwrap_or_else(|e| panic!("apply {key}={value}: {e}"));
    }

    #[test]
    fn apply_add_maps_keys_to_sections() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "interval", "5s");
        settings_apply(&mut config, "detach", "false");
        settings_apply(&mut config, "process-interval", "10000");
        settings_apply(&mut config, "collect-temperatures", "false");
        settings_apply(&mut config, "alert-cpu", "40");
        settings_apply(&mut config, "alert-cpu", "ghostty=100");
        settings_apply(&mut config, "alert-disk", "95");
        settings_apply(&mut config, "alert-disk", "/Volumes/Backup=0");
        settings_apply(&mut config, "alert-pressure", "critical");
        settings_apply(&mut config, "alert-template", "false");
        settings_apply(&mut config, "process-boost", "0");

        assert_eq!(
            config.daemon.interval,
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(config.daemon.detach, Some(false));
        let collector = config.collector.as_ref().expect("collector section");
        assert_eq!(
            collector.process_refresh_interval,
            std::time::Duration::from_secs(10)
        );
        assert!(!collector.collect_temperatures);
        // 0 persists as Some(0.0) — "disabled" must survive a file
        // round-trip, and an absent (None) field would not
        assert_eq!(collector.process_boost_cpu_cores, Some(0.0));
        assert_eq!(config.alerts.cpu, Some(40.0));
        assert_eq!(config.alerts.cpu_overrides["ghostty"], 100.0);
        assert_eq!(config.alerts.disk, Some(95.0));
        assert_eq!(config.alerts.disk_overrides["/Volumes/Backup"], 0.0);
        assert_eq!(config.alerts.pressure, Some(PressureAlert::Critical));
        assert_eq!(config.alerts.template, Some(false));

        apply_remove(&mut config, "alert-pressure", None).expect("reset pressure");
        assert_eq!(config.alerts.pressure, None);
        apply_remove(&mut config, "alert-template", None).expect("reset template");
        assert_eq!(config.alerts.template, None);
        apply_remove(&mut config, "alert-disk", Some("/Volumes/Backup")).expect("drop override");
        assert!(config.alerts.disk_overrides.is_empty());
        apply_remove(&mut config, "alert-disk", None).expect("reset disk");
        assert_eq!(config.alerts.disk, None);
    }

    #[test]
    fn apply_add_rejects_unknown_keys_and_bad_values() {
        let mut config = FileConfig::default();
        assert!(apply_add(&mut config, "no-such-key", "1").is_err());
        assert!(apply_add(&mut config, "interval", "abc").is_err());
        assert!(apply_add(&mut config, "interval", "0").is_err());
        assert!(apply_add(&mut config, "collect-disks", "yes").is_err());
        assert!(apply_add(&mut config, "alert-cpu", "=100").is_err());
        assert!(apply_add(&mut config, "alert-disk", "=90").is_err());
        assert!(apply_add(&mut config, "alert-pressure", "loud").is_err());
        assert!(apply_add(&mut config, "process-boost", "-1").is_err());
        // A `*` only means anything at the ends; an interior one is
        // refused rather than stored as a literal name that can never
        // match (see alerts::Matcher)
        assert!(apply_add(&mut config, "alert-cpu", "rust*analyzer=100").is_err());
        assert!(apply_add(&mut config, "alert-mem", "a*b=10").is_err());
        assert!(apply_add(&mut config, "alert-app-cpu", "a*b=10").is_err());
        assert!(apply_add(&mut config, "alert-disk", "/Vol*umes=90").is_err());
    }

    #[test]
    fn alert_mem_bytes_takes_human_sizes_and_round_trips() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "alert-mem-bytes", "4GiB");
        assert_eq!(config.alerts.mem_bytes, Some(4 << 30));
        settings_apply(&mut config, "alert-mem-bytes", "1500MB");
        assert_eq!(config.alerts.mem_bytes, Some(1_500_000_000));
        settings_apply(&mut config, "alert-mem-bytes", "0");
        assert_eq!(
            config.alerts.mem_bytes,
            Some(0),
            "0 = no ceiling, not unset"
        );
        assert!(apply_add(&mut config, "alert-mem-bytes", "4 gigs").is_err());

        // The file keeps the human form, and reading it back gives the
        // same bytes — a config you cannot read is a config you cannot
        // check
        settings_apply(&mut config, "alert-mem-bytes", "6GiB");
        let toml = toml::to_string_pretty(&config).expect("serialize");
        assert!(toml.contains(r#"mem_bytes = "6GiB""#), "got: {toml}");
        let back: FileConfig = toml::from_str(&toml).expect("reparse");
        assert_eq!(back.alerts.mem_bytes, Some(6 << 30));

        apply_remove(&mut config, "alert-mem-bytes", None).expect("remove");
        assert_eq!(config.alerts.mem_bytes, None);
    }

    #[test]
    fn apply_add_accepts_name_patterns() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "alert-cpu", "rust-analyzer*=200");
        settings_apply(&mut config, "alert-cpu", "*Helper (Renderer)=100");
        assert_eq!(config.alerts.cpu_overrides["rust-analyzer*"], 200.0);
        assert_eq!(config.alerts.cpu_overrides["*Helper (Renderer)"], 100.0);

        // -remove takes the key verbatim, patterns included
        apply_remove(&mut config, "alert-cpu", Some("rust-analyzer*")).expect("remove pattern");
        assert!(!config.alerts.cpu_overrides.contains_key("rust-analyzer*"));
    }

    #[test]
    fn disabled_process_boost_survives_a_file_roundtrip() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "process-boost", "0");

        let text = toml::to_string_pretty(&config).expect("serialize");
        let back: FileConfig = toml::from_str(&text).expect("reparse");
        assert_eq!(
            back.collector.expect("collector").process_boost_cpu_cores,
            Some(0.0),
            "boost disabled must not revert to the default after reload"
        );
    }

    #[test]
    fn apply_remove_resets_and_drops_overrides() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "interval", "5000");
        settings_apply(&mut config, "alert-cooldown", "10m");
        settings_apply(&mut config, "alert-cpu", "40");
        settings_apply(&mut config, "alert-cpu", "ghostty=100");
        settings_apply(&mut config, "disk-interval", "9000");

        apply_remove(&mut config, "interval", None).expect("remove interval");
        assert_eq!(config.daemon.interval, None);

        apply_remove(&mut config, "alert-cpu", Some("ghostty")).expect("remove override");
        assert!(config.alerts.cpu_overrides.is_empty());
        // The rule default is untouched by an override removal
        assert_eq!(config.alerts.cpu, Some(40.0));

        apply_remove(&mut config, "disk-interval", None).expect("reset cadence");
        assert_eq!(
            config.collector.as_ref().unwrap().disk_io_refresh_interval,
            std::time::Duration::ZERO
        );

        assert!(apply_remove(&mut config, "alert-cpu", Some("missing")).is_err());
        assert!(apply_remove(&mut config, "interval", Some("name")).is_err());
    }

    #[test]
    fn saving_without_collector_section_keeps_it_absent() {
        let mut config = FileConfig::default();
        config.alerts.cpu_overrides.insert("ghostty".into(), 100.0);
        let text = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !text.contains("[collector]"),
            "an absent [collector] must not be frozen into the file: {text}"
        );
    }

    #[test]
    fn roundtrips_through_toml() {
        let mut config = FileConfig::default();
        config.alerts.cpu_overrides.insert("ghostty".into(), 100.0);
        config.alerts.mem_overrides.insert("chrome".into(), 40.0);

        let text = toml::to_string_pretty(&config).expect("serialize");
        assert!(text.contains("[alerts.cpu_overrides]"));
        let back: FileConfig = toml::from_str(&text).expect("reparse");
        assert_eq!(back.alerts.cpu_overrides["ghostty"], 100.0);
        assert_eq!(back.alerts.mem_overrides["chrome"], 40.0);
    }

    #[test]
    fn load_and_save_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("zstats-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Missing file = defaults
        let empty = load(&dir).expect("load missing");
        assert!(empty.alerts.cpu.is_none());

        let mut config = FileConfig::default();
        settings_apply(&mut config, "alert-cpu", "40");
        save(&dir, &config).expect("save");
        let back = load(&dir).expect("load");
        assert_eq!(back.alerts.cpu, Some(40.0));

        // Malformed file = error, not silent defaults
        std::fs::write(config_path(&dir), "not toml [").expect("write garbage");
        assert!(load(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_template_falls_back_to_builtin_but_never_to_silence() {
        let dir = std::env::temp_dir().join(format!("zstats-template-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // No file at all is the normal install: the compiled-in table.
        // Probed by SIZE, not by a key — the builtin is per platform
        // (`rustc` is `rustc.exe` on Windows), and what this test is
        // about is which file was loaded, not what is in it
        let builtin = load_template(&dir).expect("load missing");
        let compiled_in = crate::alerts::Template::builtin();
        assert!(!builtin.cpu.is_empty());
        assert_eq!(builtin.cpu.len(), compiled_in.cpu.len());

        std::fs::write(template_path(&dir), "version = 1\n[cpu]\ngopls = 42.0")
            .expect("write template");
        let loaded = load_template(&dir).expect("load override");
        assert_eq!(loaded.cpu.get("gopls"), Some(&42.0));
        assert_eq!(loaded.cpu.len(), 1, "replaces, not layers");

        // A template that fails to parse is an ERROR, never a quiet
        // fallback — one that did not apply is indistinguishable from a
        // quiet machine, which is the failure a monitor must not have
        std::fs::write(template_path(&dir), "version = 99").expect("write bad version");
        assert!(load_template(&dir).is_err());
        std::fs::write(template_path(&dir), "not toml [").expect("write garbage");
        assert!(load_template(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

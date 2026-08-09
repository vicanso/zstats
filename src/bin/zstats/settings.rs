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

//! Persistent CLI configuration at `<config-dir>/config.toml` (default
//! `~/.zstats`): `[collector]` (toggles and cadences), `[daemon]` (how
//! `serve` runs), and `[alerts]`. Managed with `-add` / `-remove` /
//! `-list`; `serve` loads it at startup and hot-reloads the alerts
//! section.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use zstats::CollectorConfig;

/// Config directory chosen at startup (`--config-dir`); ~/.zstats when unset
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the config directory once at startup; later calls are ignored
pub fn set_dir(dir: PathBuf) {
    let _ = CONFIG_DIR.set(dir);
}

pub fn dir() -> PathBuf {
    CONFIG_DIR.get().cloned().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zstats")
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// Collector settings (`[collector]`, durations in milliseconds —
    /// deserialized by the lib's own serde representation). Kept as an
    /// Option so a file without the section round-trips without freezing
    /// the builtin defaults into it
    pub collector: Option<CollectorConfig>,
    pub daemon: DaemonConfig,
    pub alerts: AlertsConfig,
}

/// The `[daemon]` section: how `serve` runs. Durations accept `"500ms"`,
/// `"2s"`, `"5m"`, `"1h"`, or a bare integer meaning milliseconds
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Collection interval; absent = builtin default (2s)
    #[serde(
        default,
        with = "zstats::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval: Option<std::time::Duration>,
    /// History retention for serve; absent = builtin default (5m)
    #[serde(
        default,
        with = "zstats::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub history: Option<std::time::Duration>,
    /// Start `serve` detached (the default); --foreground overrides
    pub detach: Option<bool>,
}

/// The `[alerts]` section. Percentages use the same units as the CLI
/// flags: CPU in single-core percent, memory in percent of total. A value
/// of 0 disables the rule (or, in an override, disables it for that
/// process only).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    /// Default CPU threshold; absent = builtin default (30)
    pub cpu: Option<f32>,
    /// Default memory threshold; absent = builtin default (25)
    pub mem: Option<f64>,
    /// Re-alert cooldown; absent = builtin default (10m)
    #[serde(
        default,
        with = "zstats::config::option_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub cooldown: Option<std::time::Duration>,
    /// Per-process CPU overrides, keyed by process name
    pub cpu_overrides: BTreeMap<String, f32>,
    /// Per-process memory overrides, keyed by process name
    pub mem_overrides: BTreeMap<String, f64>,
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// Keys accepted by `-add` / `-remove`, shown in error messages
pub const KNOWN_KEYS: &str = "interval, history, detach, process-interval, \
disk-interval, disk-storage-interval, network-interval, temp-interval, \
cpu-freq-interval, process-boost, max-processes, collect-processes, \
collect-disks, collect-networks, collect-temperatures, process-disk-io, \
dedupe-disks, per-core-cpu, alert-cpu, alert-mem, alert-cooldown";

fn parse<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("invalid value for {key}: {value}"))
}

fn collector_mut(config: &mut FileConfig) -> &mut CollectorConfig {
    config
        .collector
        .get_or_insert_with(CollectorConfig::default)
}

/// Apply a `-add <key> <value>` entry to the config. `alert-cpu` /
/// `alert-mem` accept either a bare percentage (rule default) or
/// `name=pct` (per-process override). Returns a description of the change.
pub fn apply_add(config: &mut FileConfig, key: &str, value: &str) -> Result<String, String> {
    use std::time::Duration;

    let millis = |key: &str, value: &str| -> Result<Duration, String> {
        zstats::config::parse_duration(value).map_err(|e| format!("{key}: {e}"))
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
        "cpu-freq-interval" => {
            collector_mut(config).cpu_frequency_refresh_interval = millis(key, value)?
        }
        // [collector] numbers and toggles
        "process-boost" => {
            let cores: f32 = parse(key, value)?;
            collector_mut(config).process_boost_cpu_cores = (cores > 0.0).then_some(cores);
        }
        "max-processes" => collector_mut(config).max_processes = parse(key, value)?,
        "collect-processes" => collector_mut(config).collect_processes = parse(key, value)?,
        "collect-disks" => collector_mut(config).collect_disks = parse(key, value)?,
        "collect-networks" => collector_mut(config).collect_networks = parse(key, value)?,
        "collect-temperatures" => collector_mut(config).collect_temperatures = parse(key, value)?,
        "process-disk-io" => collector_mut(config).collect_process_disk_io = parse(key, value)?,
        "dedupe-disks" => collector_mut(config).dedupe_disks = parse(key, value)?,
        "per-core-cpu" => collector_mut(config).per_core_cpu = parse(key, value)?,
        // [alerts]
        "alert-cpu" | "alert-mem" => {
            if let Some((name, pct)) = value.split_once('=') {
                let name = name.trim();
                if name.is_empty() {
                    return Err(format!("invalid value for {key}: {value} (empty name)"));
                }
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
        "alert-cooldown" => config.alerts.cooldown = Some(millis(key, value)?),
        other => return Err(format!("unknown key: {other} (known keys: {KNOWN_KEYS})")),
    }
    Ok(format!("{key} = {}", value.trim()))
}

/// Apply `-remove <key> [name]`: reset a setting to its builtin default,
/// or drop a per-process alert override when `name` is given
pub fn apply_remove(
    config: &mut FileConfig,
    key: &str,
    name: Option<&str>,
) -> Result<String, String> {
    if let Some(name) = name {
        let removed = match key {
            "alert-cpu" => config.alerts.cpu_overrides.remove(name).is_some(),
            "alert-mem" => config.alerts.mem_overrides.remove(name).is_some(),
            other => return Err(format!("{other} has no per-process overrides")),
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
        "process-disk-io" => {
            collector_mut(config).collect_process_disk_io = defaults.collect_process_disk_io
        }
        "dedupe-disks" => collector_mut(config).dedupe_disks = defaults.dedupe_disks,
        "per-core-cpu" => collector_mut(config).per_core_cpu = defaults.per_core_cpu,
        "alert-cpu" => config.alerts.cpu = None,
        "alert-mem" => config.alerts.mem = None,
        "alert-cooldown" => config.alerts.cooldown = None,
        other => return Err(format!("unknown key: {other} (known keys: {KNOWN_KEYS})")),
    }
    Ok(format!("{key} reset to default"))
}

/// Load the config file; a missing file is an empty config, a malformed
/// one is an error (so a typo doesn't silently drop the user's settings)
pub fn load() -> Result<FileConfig, String> {
    let path = path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

pub fn save(config: &FileConfig) -> Result<(), String> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    }
    let content =
        toml::to_string_pretty(config).map_err(|e| format!("failed to serialize config: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
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

    #[test]
    fn apply_add_maps_keys_to_sections() {
        let mut config = FileConfig::default();
        settings_apply(&mut config, "interval", "5s");
        settings_apply(&mut config, "detach", "false");
        settings_apply(&mut config, "process-interval", "10000");
        settings_apply(&mut config, "collect-temperatures", "false");
        settings_apply(&mut config, "alert-cpu", "40");
        settings_apply(&mut config, "alert-cpu", "ghostty=100");
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
        assert_eq!(collector.process_boost_cpu_cores, None);
        assert_eq!(config.alerts.cpu, Some(40.0));
        assert_eq!(config.alerts.cpu_overrides["ghostty"], 100.0);
    }

    fn settings_apply(config: &mut FileConfig, key: &str, value: &str) {
        apply_add(config, key, value).unwrap_or_else(|e| panic!("apply {key}={value}: {e}"));
    }

    #[test]
    fn apply_add_rejects_unknown_keys_and_bad_values() {
        let mut config = FileConfig::default();
        assert!(apply_add(&mut config, "no-such-key", "1").is_err());
        assert!(apply_add(&mut config, "interval", "abc").is_err());
        assert!(apply_add(&mut config, "interval", "0").is_err());
        assert!(apply_add(&mut config, "collect-disks", "yes").is_err());
        assert!(apply_add(&mut config, "alert-cpu", "=100").is_err());
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
}

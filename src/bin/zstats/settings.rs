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

//! Persistent CLI configuration at `~/.zstats/config.toml`, currently
//! holding the `[alerts]` section. Managed with `--add-alert` /
//! `--remove-alert` / `--list-alerts`; `serve` loads it at startup, with
//! explicit command-line flags taking precedence.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub alerts: AlertsConfig,
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
    /// Re-alert cooldown; absent = builtin default (600)
    pub cooldown_secs: Option<u64>,
    /// Per-process CPU overrides, keyed by process name
    pub cpu_overrides: BTreeMap<String, f32>,
    /// Per-process memory overrides, keyed by process name
    pub mem_overrides: BTreeMap<String, f64>,
}

pub fn path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".zstats").join("config.toml")
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
cooldown_secs = 300

[alerts.cpu_overrides]
ghostty = 100.0

[alerts.mem_overrides]
"Google Chrome" = 50.0
"#,
        )
        .expect("parse");
        assert_eq!(config.alerts.cpu, Some(40.0));
        assert_eq!(config.alerts.mem, None);
        assert_eq!(config.alerts.cooldown_secs, Some(300));
        assert_eq!(config.alerts.cpu_overrides["ghostty"], 100.0);
        assert_eq!(config.alerts.mem_overrides["Google Chrome"], 50.0);
    }

    #[test]
    fn empty_input_is_default() {
        let config: FileConfig = toml::from_str("").expect("parse empty");
        assert!(config.alerts.cpu.is_none());
        assert!(config.alerts.cpu_overrides.is_empty());
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

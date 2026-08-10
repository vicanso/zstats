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

//! CLI-side view of the config file: holds the `--config-dir` choice as
//! process-wide state and wraps `zstats::settings` (the shared file
//! model, key table, and load/save) with it. Errors are flattened to
//! strings here because every caller prints them to the terminal.

use std::path::PathBuf;
use std::sync::OnceLock;

// AlertsConfig's only consumer is the alerting sink, which is unix-only
#[cfg(unix)]
pub use zstats::settings::AlertsConfig;
pub use zstats::settings::{FileConfig, apply_add, apply_remove};

/// Config directory chosen at startup (`--config-dir`); ~/.zstats when unset
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the config directory once at startup; later calls are ignored
pub fn set_dir(dir: PathBuf) {
    let _ = CONFIG_DIR.set(dir);
}

pub fn dir() -> PathBuf {
    CONFIG_DIR
        .get()
        .cloned()
        .unwrap_or_else(zstats::settings::default_dir)
}

pub fn path() -> PathBuf {
    zstats::settings::config_path(&dir())
}

/// Load the config file; a missing file is an empty config, a malformed
/// one is an error (so a typo doesn't silently drop the user's settings)
pub fn load() -> Result<FileConfig, String> {
    zstats::settings::load(&dir()).map_err(|e| e.to_string())
}

pub fn save(config: &FileConfig) -> Result<(), String> {
    zstats::settings::save(&dir(), config).map_err(|e| e.to_string())
}

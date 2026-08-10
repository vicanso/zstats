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

//! Error type definitions.
//!
//! Principles:
//! - A failed collect must never crash the process; the Scheduler logs it
//!   and continues with the next round.
//! - A single sink failure must not affect other sinks.

use snafu::Snafu;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum CollectError {
    #[snafu(display("underlying system call failed: {message}"))]
    System { message: String },

    // Custom selector suffix to avoid clashing with `SinkError::Timeout`,
    // whose selector would otherwise also be named `TimeoutSnafu`
    #[snafu(display("timeout while collecting"))]
    #[snafu(context(suffix(CollectSnafu)))]
    Timeout,

    #[snafu(display("partial failure: {message}"))]
    Partial { message: String },
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum SinkError {
    // context(false) generates `From<std::io::Error>` so `?` still works
    #[snafu(display("io error: {source}"))]
    #[snafu(context(false))]
    Io { source: std::io::Error },

    #[snafu(display("serialization error: {message}"))]
    Serde { message: String },

    #[snafu(display("remote write failed: {message}"))]
    Remote { message: String },

    #[snafu(display("timeout"))]
    #[snafu(context(suffix(SinkSnafu)))]
    Timeout,
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum SchedulerError {
    #[snafu(display("scheduler is already running"))]
    AlreadyRunning,
}

/// Errors from the daemon client (the `client` feature, unix only)
#[cfg(all(feature = "client", unix))]
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ClientError {
    /// Connecting to the socket failed — in practice "no daemon is
    /// running there"
    #[snafu(display("no daemon reachable at {}: {source}", path.display()))]
    #[snafu(context(suffix(ClientSnafu)))]
    Connect {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("daemon connection failed: {source}"))]
    #[snafu(context(suffix(ClientSnafu)))]
    Io { source: std::io::Error },

    /// The server's reply does not follow the attach protocol
    #[snafu(display("unexpected reply from daemon: {reply:?}"))]
    #[snafu(context(suffix(ClientSnafu)))]
    Protocol { reply: String },
}

/// Errors from loading configuration files (the `config` feature)
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ConfigError {
    // Custom selector suffix: `Read` alone is a likely future clash and the
    // module already establishes this convention for `Timeout`
    #[snafu(display("failed to read {path}: {source}"))]
    #[snafu(context(suffix(ConfigSnafu)))]
    Read {
        path: String,
        source: std::io::Error,
    },

    #[snafu(display("failed to parse {path}: {message}"))]
    #[snafu(context(suffix(ConfigSnafu)))]
    Parse { path: String, message: String },

    #[snafu(display("failed to write {path}: {source}"))]
    #[snafu(context(suffix(ConfigSnafu)))]
    Write {
        path: String,
        source: std::io::Error,
    },
}

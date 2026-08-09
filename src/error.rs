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

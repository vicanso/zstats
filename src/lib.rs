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

//! zstats — a unified, extensible, and reusable core for system
//! performance data collection.
//!
//! - The unified data model [`SystemSnapshot`] is the single contract;
//! - Collection ([`Collector`]) and output ([`MetricSink`]) are fully
//!   decoupled, supporting multiple backends;
//! - Can be embedded into an app as a library (low-latency delivery via
//!   [`LocalChannelSink`]) or run as a standalone agent process.
//!
//! # Quick Start
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use zstats::{
//!     CollectorConfig, LocalChannelSink, LocalCollector, MetricSink, Scheduler,
//! };
//!
//! # async fn run() {
//! let collector = LocalCollector::new(CollectorConfig::default());
//! let (sink, mut rx) = LocalChannelSink::channel();
//!
//! let mut scheduler = Scheduler::new(
//!     Box::new(collector),
//!     vec![Arc::new(sink) as Arc<dyn MetricSink>],
//!     Duration::from_secs(1),
//! );
//! scheduler.start().await.unwrap();
//!
//! // The subscriber waits for and reads the latest data
//! rx.changed().await.unwrap();
//! let snapshot = rx.borrow().clone();
//! # }
//! ```

pub mod collector;
pub mod config;
pub mod error;
#[cfg(feature = "runtime")]
pub mod scheduler;
#[cfg(feature = "runtime")]
pub mod sink;
pub mod snapshot;
pub mod utils;

/// Attribute macro required to implement a custom [`MetricSink`];
/// re-exported for downstream convenience
#[cfg(feature = "runtime")]
pub use async_trait::async_trait;

pub use collector::{Collector, LocalCollector};
pub use config::CollectorConfig;
pub use error::{CollectError, ConfigError, SchedulerError, SinkError};
#[cfg(feature = "runtime")]
pub use scheduler::Scheduler;
#[cfg(feature = "runtime")]
pub use sink::{LocalChannelSink, MetricSink, StdoutSink};
pub use snapshot::{
    CpuSnapshot, DiskSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, NetworkSnapshot,
    ProcessSnapshot, SystemSnapshot, TemperatureSnapshot,
};

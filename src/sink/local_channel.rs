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

//! Pushes the latest snapshot to the embedder (e.g. a GPUI app) via a
//! `watch` channel. The app side only needs to hold a `watch::Receiver`
//! to read the latest data at any time.

use async_trait::async_trait;
use tokio::sync::watch;

use crate::error::SinkError;
use crate::sink::MetricSink;
use crate::snapshot::SystemSnapshot;

pub struct LocalChannelSink {
    tx: watch::Sender<Option<SystemSnapshot>>,
}

impl LocalChannelSink {
    pub fn new(tx: watch::Sender<Option<SystemSnapshot>>) -> Self {
        Self { tx }
    }

    /// Convenience constructor: create the sink and a subscriber together
    pub fn channel() -> (Self, watch::Receiver<Option<SystemSnapshot>>) {
        let (tx, rx) = watch::channel(None);
        (Self::new(tx), rx)
    }

    /// Subscribe an additional receiver
    pub fn subscribe(&self) -> watch::Receiver<Option<SystemSnapshot>> {
        self.tx.subscribe()
    }
}

#[async_trait]
impl MetricSink for LocalChannelSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        // Having no receiver is not an error: the app may not have
        // subscribed yet, or may have already exited
        let _ = self.tx.send(Some(snapshot.clone()));
        Ok(())
    }

    fn name(&self) -> &str {
        "local_channel"
    }
}

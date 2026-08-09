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

//! Debug sink: prints snapshots to stdout as JSON.

use async_trait::async_trait;

use crate::error::SinkError;
use crate::sink::MetricSink;
use crate::snapshot::SystemSnapshot;

#[derive(Debug, Default)]
pub struct StdoutSink {
    pretty: bool,
}

impl StdoutSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pretty() -> Self {
        Self { pretty: true }
    }
}

#[async_trait]
impl MetricSink for StdoutSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        let json = if self.pretty {
            serde_json::to_string_pretty(snapshot)
        } else {
            serde_json::to_string(snapshot)
        }
        .map_err(|e| SinkError::Serde {
            message: e.to_string(),
        })?;
        println!("{json}");
        Ok(())
    }

    fn name(&self) -> &str {
        "stdout"
    }
}

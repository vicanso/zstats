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

//! Typed client for the daemon's Unix-socket protocol (unix only).
//!
//! `zstats serve` publishes snapshots over a per-user Unix domain socket
//! with a line protocol: the client sends `ATTACH` and receives
//! `HISTORY <n>` followed by n buffered snapshots as JSON lines (oldest
//! first), then a live JSON line per collect; or sends `STOP` and the
//! daemon shuts down. This module is the client side of that exchange,
//! shared by every frontend (CLI, GUI, scripts) so the wire format is
//! defined in exactly one place — presentation stays with the caller.
//!
//! ```no_run
//! # async fn run() -> Result<(), zstats::ClientError> {
//! let mut stream = zstats::client::attach().await?;
//! while let Some(snapshot) = stream.next().await? {
//!     let live = stream.history_remaining() == 0;
//!     println!("{} (live: {live})", snapshot.timestamp);
//! }
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use snafu::ResultExt as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::SystemSnapshot;
use crate::error::{ClientError, ConnectClientSnafu, IoClientSnafu, ProtocolClientSnafu};

/// Per-user socket path, e.g. `$TMPDIR/zstats-tree.sock` — the address
/// both `zstats serve` and every client agree on
pub fn socket_path() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    std::env::temp_dir().join(format!("zstats-{user}.sock"))
}

/// Whether a daemon is currently reachable on the default socket
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

/// An attached daemon session: yields the buffered history snapshots
/// first (oldest first), then live snapshots as the daemon collects them
pub struct DaemonStream {
    lines: Lines<BufReader<OwnedReadHalf>>,
    /// Held so the connection stays fully open for the session's lifetime
    _writer: OwnedWriteHalf,
    history_len: usize,
    history_remaining: usize,
}

impl DaemonStream {
    /// How many buffered snapshots the daemon announced at attach time
    pub fn history_len(&self) -> usize {
        self.history_len
    }

    /// How many announced history snapshots are still to be read; once
    /// this reaches 0 every further snapshot is live data
    pub fn history_remaining(&self) -> usize {
        self.history_remaining
    }

    /// The next snapshot, or `Ok(None)` once the daemon closed the
    /// connection. Lines that fail to parse as a snapshot are skipped.
    /// Cancel-safe: dropping the future between lines loses nothing, so
    /// this can sit directly in a `select!` arm
    pub async fn next(&mut self) -> Result<Option<SystemSnapshot>, ClientError> {
        loop {
            let Some(line) = self.lines.next_line().await.context(IoClientSnafu)? else {
                return Ok(None);
            };
            // A garbled line still consumes its announced history slot
            self.history_remaining = self.history_remaining.saturating_sub(1);
            if let Ok(snapshot) = serde_json::from_str::<SystemSnapshot>(&line) {
                return Ok(Some(snapshot));
            }
        }
    }
}

/// Attach to the daemon listening at `path`
pub async fn attach_at(path: &Path) -> Result<DaemonStream, ClientError> {
    let stream = UnixStream::connect(path)
        .await
        .context(ConnectClientSnafu { path })?;
    let (reader, mut writer) = stream.into_split();
    writer.write_all(b"ATTACH\n").await.context(IoClientSnafu)?;

    let mut lines = BufReader::new(reader).lines();
    let header = lines
        .next_line()
        .await
        .context(IoClientSnafu)?
        .unwrap_or_default();
    let Some(count) = header
        .strip_prefix("HISTORY ")
        .and_then(|n| n.trim().parse().ok())
    else {
        return ProtocolClientSnafu { reply: header }.fail();
    };

    Ok(DaemonStream {
        lines,
        _writer: writer,
        history_len: count,
        history_remaining: count,
    })
}

/// Attach to the daemon at the default per-user socket
pub async fn attach() -> Result<DaemonStream, ClientError> {
    attach_at(&socket_path()).await
}

/// Ask the daemon listening at `path` to shut down. `Ok(false)` means no
/// daemon was reachable — which already satisfies "stopped", so callers
/// wanting idempotent stop treat both `Ok` values as success
pub async fn stop_at(path: &Path) -> Result<bool, ClientError> {
    let Ok(stream) = UnixStream::connect(path).await else {
        return Ok(false);
    };
    let (reader, mut writer) = stream.into_split();
    writer.write_all(b"STOP\n").await.context(IoClientSnafu)?;
    // Wait for the OK (or EOF) so the daemon has seen the command before
    // this returns
    let _ = BufReader::new(reader).lines().next_line().await;
    Ok(true)
}

/// Ask the daemon at the default per-user socket to shut down
pub async fn stop() -> Result<bool, ClientError> {
    stop_at(&socket_path()).await
}

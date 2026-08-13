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

//! Daily metrics history: `<config-dir>/data/YYYY-MM-DD.jsonl`.
//!
//! Once a minute the daemon records every process whose 1-minute averages
//! exceed the base alert thresholds (see [`crate::alerts`]) as one
//! [`MetricRecord`] JSON line in the current local date's file. This
//! module owns that file format — writing, reading back by date range,
//! and retention cleanup — so every frontend sees the same history.
//!
//! Two time bases meet here, on purpose. The FILE is named by the LOCAL
//! date, because "what happened on my Monday" should be one file; each
//! record's `timestamp` is UTC, because it is copied verbatim from the
//! snapshot and has to mean the same thing on every machine. So a file
//! legitimately contains two UTC dates — east of UTC, everything before
//! local 08:00 carries yesterday's UTC date. Group by day with
//! [`read_range`] (it selects by file, i.e. by local day) rather than by
//! slicing the timestamp string, and convert to local only for display.
//!
//! Retention is built into writing: [`append`] runs a sweep deleting
//! files older than [`RETENTION_DAYS`] whenever the date differs from
//! the last sweep's (tracked in process memory per config dir), so every
//! writer gets cleanup without scheduling anything. The corollary: a
//! quiet stretch with no appends leaves expired files in place until the
//! next append.
//!
//! All functions take the date explicitly instead of reading the clock;
//! callers pass `jiff::Zoned::now().date()` (the local date).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

/// Daily files older than this many days are deleted by the sweep that
/// [`append`] triggers on day change
pub const RETENTION_DAYS: i64 = 30;

/// Local date of the last retention sweep, per config dir (process-wide:
/// each writer process sweeps independently; the sweep is idempotent and
/// cheap, so overlap between processes is harmless)
static LAST_CLEANUP: LazyLock<Mutex<HashMap<PathBuf, jiff::civil::Date>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One recorded data point: a process whose 1-minute averages exceeded
/// the base alert thresholds at recording time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRecord {
    /// Sampling time in UTC (the snapshot's own timestamp) — note the
    /// containing file is named by LOCAL date, so the two can disagree
    pub timestamp: jiff::Timestamp,
    pub pid: u32,
    pub name: String,
    /// 1-minute average CPU, single-core units
    pub cpu_avg_percent: f32,
    /// 1-minute average resident memory
    pub memory_avg_bytes: u64,
    /// `memory_avg_bytes` as a percentage of total memory
    pub memory_share_percent: f32,
    /// The process's LIFETIME CPU counter at this sample, in single-core
    /// milliseconds — an absolute value, not this window's share.
    ///
    /// Answers "what actually burned the CPU today", which neither an
    /// average nor a peak can: a steady 8% outspends a ten-minute 100%
    /// spike several times over while never looking alarming in any one
    /// sample. Subtract one pid's earliest record of the day from its
    /// latest to get exactly what it consumed in between.
    ///
    /// Storing the counter rather than a per-window delta is deliberate:
    /// a process is only recorded on the minutes it qualifies, so
    /// summing deltas would silently undercount everything that drifts
    /// in and out of the selection — whereas differencing a cumulative
    /// counter stays exact across any gap. A DECREASE for a given pid
    /// means the pid was reused by a new process; treat the two sides as
    /// unrelated. Absent from files written before this field existed,
    /// where it reads 0
    #[serde(default)]
    pub cpu_time_ms: u64,
}

/// Where the daily files live: `<config_dir>/data`
pub fn data_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("data")
}

fn file_for(config_dir: &Path, date: jiff::civil::Date) -> PathBuf {
    data_dir(config_dir).join(format!("{date}.jsonl"))
}

/// Append records to `date`'s file, creating the data directory as
/// needed, and — on the first append of each `date` per config dir —
/// sweep files older than [`RETENTION_DAYS`]. Returns the file written to
pub fn append(
    config_dir: &Path,
    date: jiff::civil::Date,
    records: &[MetricRecord],
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(data_dir(config_dir))?;
    let path = file_for(config_dir, date);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    for record in records {
        let line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
    }
    maybe_cleanup(config_dir, date);
    Ok(path)
}

/// Run the retention sweep when `date` differs from the last sweep's
/// date for this config dir (i.e. once per day per writer, and on the
/// writer's first append after startup)
fn maybe_cleanup(config_dir: &Path, date: jiff::civil::Date) {
    {
        let mut last = LAST_CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
        if last.get(config_dir) == Some(&date) {
            return;
        }
        last.insert(config_dir.to_path_buf(), date);
    }
    for path in cleanup(config_dir, date, RETENTION_DAYS) {
        tracing::info!("removed old metrics file {}", path.display());
    }
}

/// Read every record from the files dated `from..=to` (inclusive),
/// oldest file first. Missing files are skipped (days without qualifying
/// processes have none), and so are lines that fail to parse
pub fn read_range(
    config_dir: &Path,
    from: jiff::civil::Date,
    to: jiff::civil::Date,
) -> std::io::Result<Vec<MetricRecord>> {
    let mut records = Vec::new();
    let mut date = from;
    while date <= to {
        match std::fs::File::open(file_for(config_dir, date)) {
            Ok(file) => {
                for line in std::io::BufReader::new(file).lines() {
                    if let Ok(record) = serde_json::from_str::<MetricRecord>(&line?) {
                        records.push(record);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let Ok(next) = date.tomorrow() else { break };
        date = next;
    }
    Ok(records)
}

/// Delete `YYYY-MM-DD.jsonl` files dated more than `retention_days`
/// before `today`, returning the paths removed. Anything unparseable is
/// left alone; per-file removal errors are skipped (best-effort — the
/// next daily sweep retries)
pub fn cleanup(config_dir: &Path, today: jiff::civil::Date, retention_days: i64) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(cutoff) = today.checked_sub(jiff::Span::new().days(retention_days)) else {
        return removed;
    };
    let Ok(entries) = std::fs::read_dir(data_dir(config_dir)) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(date) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|stem| stem.parse::<jiff::civil::Date>().ok())
        else {
            continue;
        };
        if date < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            removed.push(entry.path());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_at(second: i64, pid: u32) -> MetricRecord {
        MetricRecord {
            timestamp: jiff::Timestamp::from_second(second).expect("valid timestamp"),
            pid,
            name: format!("p{pid}"),
            cpu_avg_percent: 50.0,
            memory_avg_bytes: 1024,
            memory_share_percent: 10.0,
            cpu_time_ms: 30_000,
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zstats-records-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn append_and_read_range_roundtrip() {
        let dir = test_dir("roundtrip");
        let day1: jiff::civil::Date = "2026-08-01".parse().expect("date");
        let day3: jiff::civil::Date = "2026-08-03".parse().expect("date");

        append(&dir, day1, &[record_at(1, 10), record_at(2, 11)]).expect("append day1");
        append(&dir, day3, &[record_at(3, 12)]).expect("append day3");
        // A garbled line must not break reading
        std::fs::write(data_dir(&dir).join("2026-08-02.jsonl"), "not json\n")
            .expect("write garbage");

        // day2 has no valid records, day boundaries are inclusive
        let all = read_range(&dir, day1, day3).expect("read");
        assert_eq!(
            all.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert_eq!(all[0].timestamp.as_second(), 1);
        assert_eq!(all[0].name, "p10");

        // A narrower range excludes the other days
        let only_day3 = read_range(&dir, day3, day3).expect("read");
        assert_eq!(only_day3.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_sweeps_expired_files_on_day_change() {
        let dir = test_dir("append-sweep");
        std::fs::create_dir_all(data_dir(&dir)).expect("create data dir");
        // 31 days before the append date: must be swept by the append
        std::fs::write(data_dir(&dir).join("2026-07-10.jsonl"), "x\n").expect("write");

        let today: jiff::civil::Date = "2026-08-10".parse().expect("date");
        append(&dir, today, &[record_at(1, 10)]).expect("append");
        assert!(!data_dir(&dir).join("2026-07-10.jsonl").exists());
        assert!(data_dir(&dir).join("2026-08-10.jsonl").exists());

        // Same date again: the sweep is skipped (seed a new expired file
        // and observe it survive this append)
        std::fs::write(data_dir(&dir).join("2026-07-01.jsonl"), "x\n").expect("write");
        append(&dir, today, &[record_at(2, 11)]).expect("append");
        assert!(data_dir(&dir).join("2026-07-01.jsonl").exists());

        // Day rolls over: the sweep runs again
        let tomorrow = today.tomorrow().expect("tomorrow");
        append(&dir, tomorrow, &[record_at(3, 12)]).expect("append");
        assert!(!data_dir(&dir).join("2026-07-01.jsonl").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_only_expired_jsonl() {
        let dir = test_dir("cleanup");
        let today: jiff::civil::Date = "2026-08-10".parse().expect("date");
        std::fs::create_dir_all(data_dir(&dir)).expect("create data dir");
        for name in [
            "2026-07-10.jsonl", // 31 days old: expired
            "2026-07-12.jsonl", // 29 days old: kept
            "2026-08-10.jsonl", // today: kept
            "notes.txt",        // not a metrics file: kept
            "garbage.jsonl",    // unparseable date: kept
        ] {
            std::fs::write(data_dir(&dir).join(name), "x\n").expect("write test file");
        }

        let removed = cleanup(&dir, today, 30);
        assert_eq!(removed, vec![data_dir(&dir).join("2026-07-10.jsonl")]);

        let mut remaining: Vec<String> = std::fs::read_dir(data_dir(&dir))
            .expect("read data dir")
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            [
                "2026-07-12.jsonl",
                "2026-08-10.jsonl",
                "garbage.jsonl",
                "notes.txt"
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

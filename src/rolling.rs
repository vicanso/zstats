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

//! Rolling per-process averages over a fixed time window.
//!
//! Snapshots deliver raw per-interval facts; frontends usually want
//! smoothed values — the PROC table ranks by 1-minute averages, the alert
//! rules only react to sustained behavior. [`ProcessWindows`] is that
//! shared smoothing state: feed it every snapshot's process list and it
//! returns per-pid averages over the window, pruning dead pids and
//! expired samples as it goes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::snapshot::ProcessSnapshot;

/// One recorded sample
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    cpu: f32,
    memory_bytes: u64,
    /// The process's lifetime CPU counter at this instant
    cpu_time_ms: u64,
}

/// Averages for one process over the window, as of the latest
/// [`ProcessWindows::record`] call
#[derive(Debug, Clone, Copy)]
pub struct ProcessStats {
    /// Average CPU in single-core percent
    pub cpu_avg: f64,
    /// CPU time consumed between the oldest and newest retained sample,
    /// in single-core milliseconds.
    ///
    /// Unlike `cpu_avg` this is an amount, not a rate: it answers "what
    /// did this process cost over `span`" rather than "how busy is it".
    /// That is the only framing in which a process sitting at a steady
    /// 8% shows up at all — it never approaches any threshold, yet over
    /// hours it outspends everything that does. 0 for a single sample
    pub cpu_time_delta_ms: u64,
    /// Average resident memory in bytes
    pub memory_avg_bytes: f64,
    /// Time between the oldest and newest retained sample — how "full"
    /// the window is (0 for a single sample)
    pub span: Duration,
    /// Number of retained samples
    pub samples: usize,
}

/// Per-process rolling sample windows over a fixed duration
pub struct ProcessWindows {
    window: Duration,
    history: HashMap<u32, VecDeque<Sample>>,
}

impl ProcessWindows {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            history: HashMap::new(),
        }
    }

    /// Record `processes` as observed at `now` and return each pid's
    /// averages over the window. Pids absent from this snapshot are
    /// forgotten entirely (dead, or fell out of the collector's
    /// selection), so the state never grows beyond one snapshot's worth
    /// of processes
    pub fn record(
        &mut self,
        now: Instant,
        processes: &[ProcessSnapshot],
    ) -> HashMap<u32, ProcessStats> {
        let current: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        self.history.retain(|pid, _| current.contains(pid));

        let mut stats = HashMap::with_capacity(processes.len());
        for p in processes {
            let samples = self.history.entry(p.pid).or_default();
            samples.push_back(Sample {
                at: now,
                cpu: p.cpu_usage_percent,
                memory_bytes: p.memory_bytes,
                cpu_time_ms: p.cpu_time_ms,
            });
            while let Some(oldest) = samples.front() {
                if now.duration_since(oldest.at) > self.window {
                    samples.pop_front();
                } else {
                    break;
                }
            }

            let n = samples.len() as f64;
            let span = samples
                .front()
                .map(|oldest| now.duration_since(oldest.at))
                .unwrap_or(Duration::ZERO);
            // saturating: a counter can only go backwards if the pid was
            // reused between samples, and 0 is the honest answer there
            let cpu_time_delta_ms = match (samples.front(), samples.back()) {
                (Some(first), Some(last)) => last.cpu_time_ms.saturating_sub(first.cpu_time_ms),
                _ => 0,
            };
            stats.insert(
                p.pid,
                ProcessStats {
                    cpu_avg: samples.iter().map(|s| f64::from(s.cpu)).sum::<f64>() / n,
                    cpu_time_delta_ms,
                    memory_avg_bytes: samples.iter().map(|s| s.memory_bytes as f64).sum::<f64>()
                        / n,
                    span,
                    samples: samples.len(),
                },
            );
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu: f32, mem: u64) -> ProcessSnapshot {
        proc_at(pid, cpu, mem, 0)
    }

    fn proc_at(pid: u32, cpu: f32, mem: u64, cpu_time_ms: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            name: format!("p{pid}"),
            cmd: String::new(),
            cpu_usage_percent: cpu,
            cpu_time_ms,
            memory_bytes: mem,
            phys_footprint_bytes: None,
            virtual_memory_bytes: mem,
            run_time_secs: 0,
            parent_pid: None,
            user_id: None,
            status: "Runnable".into(),
            read_bytes_per_sec: None,
            write_bytes_per_sec: None,
        }
    }

    #[test]
    fn averages_across_records_and_prunes_dead_pids() {
        let mut windows = ProcessWindows::new(Duration::from_secs(60));
        let base = Instant::now();

        let first = windows.record(base, &[proc(1, 10.0, 100)]);
        assert_eq!(first[&1].cpu_avg, 10.0);
        assert_eq!(first[&1].span, Duration::ZERO);
        assert_eq!(first[&1].samples, 1);

        let second = windows.record(base + Duration::from_secs(2), &[proc(1, 30.0, 300)]);
        assert_eq!(second[&1].cpu_avg, 20.0);
        assert_eq!(second[&1].memory_avg_bytes, 200.0);
        assert_eq!(second[&1].span, Duration::from_secs(2));

        // pid 1 disappears: its window is dropped, a fresh pid starts clean
        let third = windows.record(base + Duration::from_secs(4), &[proc(2, 50.0, 0)]);
        assert!(!third.contains_key(&1));
        assert_eq!(third[&2].samples, 1);

        // pid 1 comes back with an empty window (no stale average)
        let fourth = windows.record(base + Duration::from_secs(6), &[proc(1, 90.0, 0)]);
        assert_eq!(fourth[&1].cpu_avg, 90.0);
        assert_eq!(fourth[&1].samples, 1);
    }

    #[test]
    fn cpu_time_delta_measures_the_amount_a_low_percentage_process_costs() {
        let mut windows = ProcessWindows::new(Duration::from_secs(60));
        let base = Instant::now();

        // A steady 10% process: 100 core-ms per second of wall clock
        windows.record(base, &[proc_at(1, 10.0, 0, 5_000)]);
        let stats = windows.record(
            base + Duration::from_secs(30),
            &[proc_at(1, 10.0, 0, 8_000)],
        );
        assert_eq!(stats[&1].cpu_time_delta_ms, 3_000);
        // ...which no average-vs-threshold test would ever surface
        assert_eq!(stats[&1].cpu_avg, 10.0);

        // A single sample has nothing to diff against
        let fresh = windows.record(base + Duration::from_secs(31), &[proc_at(2, 90.0, 0, 999)]);
        assert_eq!(fresh[&2].cpu_time_delta_ms, 0);

        // A counter that went backwards (pid reuse) reports 0, not a wrap
        let reused = windows.record(base + Duration::from_secs(32), &[proc_at(2, 90.0, 0, 10)]);
        assert_eq!(reused[&2].cpu_time_delta_ms, 0);
    }

    #[test]
    fn samples_older_than_the_window_fall_off() {
        let mut windows = ProcessWindows::new(Duration::from_secs(60));
        let base = Instant::now();

        windows.record(base, &[proc(1, 100.0, 0)]);
        // 70s later the first sample is outside the 60s window
        let stats = windows.record(base + Duration::from_secs(70), &[proc(1, 10.0, 0)]);
        assert_eq!(stats[&1].cpu_avg, 10.0);
        assert_eq!(stats[&1].samples, 1);
    }
}

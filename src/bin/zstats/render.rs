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

//! Human-readable text rendering (CLI only; the library itself has no
//! notion of presentation).

use std::fmt::Write as _;

use jiff::tz::TimeZone;
use zstats::{MetricSink, SinkError, SystemSnapshot, async_trait};

/// Max number of processes to display
const TOP_PROCESSES: usize = 5;

pub fn render(s: &SystemSnapshot) -> String {
    let mut out = String::new();

    let _ = writeln!(
        out,
        "HOST  {}  {} {} ({})  {}",
        s.host.hostname,
        s.host.os_name,
        s.host.os_version,
        s.host.arch,
        s.timestamp
            .to_zoned(TimeZone::system())
            .strftime("%Y-%m-%d %H:%M:%S"),
    );

    let freq = s
        .cpu
        .frequency_mhz
        .map(|f| format!(" @ {f} MHz"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "CPU   {:.1}%  {} cores{}  load {:.2} / {:.2} / {:.2}",
        s.cpu.usage_percent, s.cpu.logical_cores, freq, s.load.load1, s.load.load5, s.load.load15,
    );

    let mem_percent = if s.memory.total_bytes > 0 {
        s.memory.used_bytes as f64 / s.memory.total_bytes as f64 * 100.0
    } else {
        0.0
    };
    let _ = writeln!(
        out,
        "MEM   {} / {} ({:.1}%)  swap {} / {}",
        human_bytes(s.memory.used_bytes),
        human_bytes(s.memory.total_bytes),
        mem_percent,
        human_bytes(s.memory.swap_used_bytes),
        human_bytes(s.memory.swap_total_bytes),
    );

    for disk in &s.disks {
        let _ = writeln!(
            out,
            "DISK  {}  {}  free {} / {}  R {}  W {}",
            disk.name,
            disk.mount_point,
            human_bytes(disk.available_bytes),
            human_bytes(disk.total_bytes),
            human_rate(disk.read_bytes_per_sec),
            human_rate(disk.write_bytes_per_sec),
        );
    }

    // Only show interfaces with traffic to avoid a wall of idle utun/lo entries
    let active: Vec<_> = s
        .networks
        .iter()
        .filter(|n| n.received_bytes_per_sec > 0 || n.transmitted_bytes_per_sec > 0)
        .collect();
    if active.is_empty() {
        let _ = writeln!(out, "NET   (no active interfaces)");
    } else {
        for net in active {
            let _ = writeln!(
                out,
                "NET   {}  rx {}  tx {}",
                net.interface,
                human_rate(Some(net.received_bytes_per_sec)),
                human_rate(Some(net.transmitted_bytes_per_sec)),
            );
        }
    }

    if let Some(processes) = &s.processes {
        let _ = writeln!(out, "TOP   {:>7}  {:>6}  {:>9}  NAME", "PID", "CPU%", "MEM");
        for p in processes.iter().take(TOP_PROCESSES) {
            let _ = writeln!(
                out,
                "      {:>7}  {:>6.1}  {:>9}  {}",
                p.pid,
                p.cpu_usage_percent,
                human_bytes(p.memory_bytes),
                p.name,
            );
        }
    }

    out
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a rate; shows "-" when the first sample has no diff baseline
fn human_rate(rate: Option<u64>) -> String {
    match rate {
        Some(v) => format!("{}/s", human_bytes(v)),
        None => "-".to_string(),
    }
}

/// Sink that prints text output in watch mode
pub struct TextSink;

#[async_trait]
impl MetricSink for TextSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        println!("{}", render(snapshot));
        Ok(())
    }

    fn name(&self) -> &str {
        "text"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3_221_225_472), "3.0 GiB");
    }

    #[test]
    fn human_rate_missing_baseline() {
        assert_eq!(human_rate(None), "-");
        assert_eq!(human_rate(Some(1024)), "1.0 KiB/s");
    }
}

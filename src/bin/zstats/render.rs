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

/// Max display width (in chars) of the process command line
const CMD_MAX_WIDTH: usize = 50;

/// Width of the section label column ("HOST  ", "DISK  ", ...)
const LABEL_WIDTH: usize = 6;

enum Align {
    Left,
    Right,
}

/// Write a section as an aligned table: a header row prefixed by the section
/// label, then one indented row per entry. Column widths are computed from
/// the actual data; numeric columns should be right-aligned.
fn write_table(
    out: &mut String,
    label: &str,
    headers: &[&str],
    aligns: &[Align],
    rows: &[Vec<String>],
) {
    if rows.is_empty() {
        return;
    }

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let render_row = |cells: &[&str]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let width = widths[i];
            match aligns[i] {
                Align::Right => {
                    let _ = write!(line, "{cell:>width$}");
                }
                // Left-aligned last column: no trailing padding
                Align::Left if i == cells.len() - 1 => line.push_str(cell),
                Align::Left => {
                    let _ = write!(line, "{cell:<width$}");
                }
            }
        }
        line
    };

    let _ = writeln!(out, "{label:<LABEL_WIDTH$}{}", render_row(headers));
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = writeln!(out, "{:<LABEL_WIDTH$}{}", "", render_row(&cells));
    }
}

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

    if let Some(disks) = &s.disks {
        let rows: Vec<Vec<String>> = disks
            .iter()
            .map(|d| {
                vec![
                    d.name.clone(),
                    d.mount_point.clone(),
                    human_bytes(d.available_bytes),
                    human_bytes(d.total_bytes),
                    human_rate(d.read_bytes_per_sec),
                    human_rate(d.write_bytes_per_sec),
                ]
            })
            .collect();
        write_table(
            &mut out,
            "DISK",
            &["NAME", "MOUNT", "FREE", "TOTAL", "READ", "WRITE"],
            &[
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            &rows,
        );
    }

    if let Some(networks) = &s.networks {
        // Only show interfaces with traffic to avoid a wall of idle utun/lo entries
        let rows: Vec<Vec<String>> = networks
            .iter()
            .filter(|n| n.received_bytes_per_sec > 0 || n.transmitted_bytes_per_sec > 0)
            .map(|n| {
                vec![
                    n.interface.clone(),
                    human_rate(Some(n.received_bytes_per_sec)),
                    human_rate(Some(n.transmitted_bytes_per_sec)),
                ]
            })
            .collect();
        if rows.is_empty() {
            let _ = writeln!(out, "NET   (no active interfaces)");
        } else {
            write_table(
                &mut out,
                "NET",
                &["IFACE", "RX", "TX"],
                &[Align::Left, Align::Right, Align::Right],
                &rows,
            );
        }
    }

    if let Some(processes) = &s.processes {
        let rows: Vec<Vec<String>> = processes
            .iter()
            .take(TOP_PROCESSES)
            .map(|p| {
                // cmd can be empty (e.g. no permission to read other users'
                // processes); fall back to "-" so the column stays readable
                let cmd = if p.cmd.is_empty() {
                    "-".to_string()
                } else {
                    truncate_chars(&p.cmd, CMD_MAX_WIDTH)
                };
                vec![
                    p.pid.to_string(),
                    format!("{:.1}", p.cpu_usage_percent),
                    human_bytes(p.memory_bytes),
                    p.name.clone(),
                    cmd,
                ]
            })
            .collect();
        write_table(
            &mut out,
            "TOP",
            &["PID", "CPU%", "MEM", "NAME", "CMD"],
            &[
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
                Align::Left,
            ],
            &rows,
        );
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

/// Truncate to at most `max` chars, appending an ellipsis when cut
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
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

    #[test]
    fn table_columns_align() {
        let mut out = String::new();
        write_table(
            &mut out,
            "NET",
            &["IFACE", "RX", "TX"],
            &[Align::Left, Align::Right, Align::Right],
            &[
                vec!["en0".into(), "13.1 KiB/s".into(), "14.8 KiB/s".into()],
                vec!["bridge100".into(), "3.4 KiB/s".into(), "358 B/s".into()],
            ],
        );
        let expected = "\
NET   IFACE              RX          TX
      en0        13.1 KiB/s  14.8 KiB/s
      bridge100   3.4 KiB/s     358 B/s
";
        assert_eq!(out, expected);
    }

    #[test]
    fn truncate_chars_respects_char_boundaries() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        // Multi-byte chars must not be cut mid-codepoint
        assert_eq!(truncate_chars("ééééé", 3), "éé…");
    }

    #[test]
    fn empty_table_writes_nothing() {
        let mut out = String::new();
        write_table(&mut out, "DISK", &["NAME"], &[Align::Left], &[]);
        assert!(out.is_empty());
    }
}

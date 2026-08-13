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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zstats::rolling::ProcessWindows;
use zstats::{MetricSink, ProcessSnapshot, SinkError, SystemSnapshot, async_trait};

/// PROC table: rows ranked by CPU, then rows ranked by memory
const PROC_CPU_ROWS: usize = 5;
const PROC_MEM_ROWS: usize = 4;

/// Max display width (in columns) of the process command line
const CMD_MAX_WIDTH: usize = 50;

/// Gauge widths: the header gauges (CPU/MEM/SWP/LOAD), the per-disk usage
/// bar, and the per-process memory bar
const GAUGE_WIDTH: usize = 20;
const DISK_GAUGE_WIDTH: usize = 14;

/// NET table height: always the top-N interfaces by current traffic, so
/// the section (and everything under it) never jumps as traffic starts
/// and stops
const NET_ROWS: usize = 6;

/// Temperature verdict cutoffs, as a fraction of 100°C
const TEMP_WARM: f64 = 0.65;
const TEMP_HOT: f64 = 0.85;

/// Rolling window for per-process averages in watch mode
const AVG_WINDOW: Duration = Duration::from_secs(60);

/// Per-process rolling averages over [`AVG_WINDOW`]: (cpu %, memory bytes)
type ProcAverages = HashMap<u32, (f32, u64)>;

/// Width of the section label column ("HOST  ", "DISK  ", ...)
const LABEL_WIDTH: usize = 6;

enum Align {
    Left,
    Right,
}

/// ANSI styling, disabled when the output is not a terminal so piped
/// output stays plain text
#[derive(Clone, Copy)]
struct Theme {
    enabled: bool,
}

impl Theme {
    const fn plain() -> Self {
        Self { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// SGR color codes
const RED_BOLD: &str = "1;31";
const YELLOW: &str = "33";
const GREEN: &str = "32";
const DIM: &str = "2";

/// Color for a utilization level: green under `warn`, yellow between,
/// bold red above `crit`
fn level_color(fraction: f64, warn: f64, crit: f64) -> &'static str {
    if fraction >= crit {
        RED_BOLD
    } else if fraction >= warn {
        YELLOW
    } else {
        GREEN
    }
}

/// Display width of a string, ignoring ANSI escape sequences
fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the escape sequence up to its final letter
            for follow in chars.by_ref() {
                if follow.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            width += c.width().unwrap_or(0);
        }
    }
    width
}

/// Render a block gauge like `██████░░░░░░` for a 0.0–1.0 fraction
fn gauge(fraction: f64, width: usize) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut bar = "█".repeat(filled);
    bar.push_str(&"░".repeat(width - filled));
    bar
}

/// Gauge with the filled part colored by utilization level and the empty
/// part dimmed
fn styled_gauge(fraction: f64, width: usize, theme: Theme) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!(
        "{}{}",
        theme.paint(level_color(fraction, 0.6, 0.85), &"█".repeat(filled)),
        theme.paint(DIM, &"░".repeat(width - filled)),
    )
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
    theme: Theme,
) {
    if rows.is_empty() {
        return;
    }

    // Measure by visible display width: cells may contain ANSI escapes
    // (colors) and wide CJK chars
    let mut widths: Vec<usize> = headers.iter().map(|h| visible_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(visible_width(cell));
        }
    }

    let render_row = |cells: &[&str]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let pad = widths[i].saturating_sub(visible_width(cell));
            match aligns[i] {
                Align::Right => {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(cell);
                }
                // Left-aligned last column: no trailing padding
                Align::Left if i == cells.len() - 1 => line.push_str(cell),
                Align::Left => {
                    line.push_str(cell);
                    line.push_str(&" ".repeat(pad));
                }
            }
        }
        line
    };

    let _ = writeln!(
        out,
        "{label:<LABEL_WIDTH$}{}",
        theme.paint(DIM, &render_row(headers))
    );
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = writeln!(out, "{:<LABEL_WIDTH$}{}", "", render_row(&cells));
    }
}

/// Enter the live-view screen (alternate screen + hidden cursor) when
/// stdout is a TTY; returns whether it did
pub fn enter_live_screen() -> bool {
    let interactive = std::io::stdout().is_terminal();
    if interactive {
        print!("\x1b[?1049h\x1b[?25l\x1b[H");
        let _ = std::io::stdout().flush();
    }
    interactive
}

/// Restore the terminal after [`enter_live_screen`]
pub fn leave_live_screen(entered: bool) {
    if entered {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    }
}

pub fn render(s: &SystemSnapshot) -> String {
    render_with(s, None, Theme::plain())
}

fn render_with(s: &SystemSnapshot, proc_averages: Option<&ProcAverages>, theme: Theme) -> String {
    let mut out = String::new();

    let _ = writeln!(
        out,
        "HOST  {}  {} {} ({})  up {}  {}",
        s.host.hostname,
        s.host.os_name,
        s.host.os_version,
        s.host.arch,
        human_uptime(s.host.uptime_secs),
        s.timestamp
            .to_zoned(TimeZone::system())
            .strftime("%Y-%m-%d %H:%M:%S"),
    );

    let freq = s
        .cpu
        .frequency_mhz
        .map(|f| format!(" @ {f} MHz"))
        .unwrap_or_default();
    let brand = s
        .cpu
        .brand
        .as_deref()
        .filter(|b| !b.is_empty())
        .map(|b| format!("  {b}"))
        .unwrap_or_default();
    // P/E cluster split on heterogeneous CPUs, e.g. "P8 12% · E4 31%"
    let levels = s
        .cpu
        .perf_levels
        .as_ref()
        .map(|levels| {
            let parts: Vec<String> = levels
                .iter()
                .map(|l| {
                    format!(
                        "{}{} {:.0}%",
                        l.name.chars().next().unwrap_or('?'),
                        l.logical_cores,
                        l.usage_percent
                    )
                })
                .collect();
            format!("  {}", parts.join(" · "))
        })
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "CPU   {}  {:>5.1}%  {} cores{}{}{}",
        styled_gauge(f64::from(s.cpu.usage_percent) / 100.0, GAUGE_WIDTH, theme),
        s.cpu.usage_percent,
        s.cpu.logical_cores,
        freq,
        brand,
        levels,
    );

    let mem_fraction = f64::from(s.memory.used_percent) / 100.0;
    // Compressor footprint + the kernel's pressure verdict: growth there
    // is the real "memory is tight" signal on macOS, not used%
    let mut mem_extra = String::new();
    if let Some(compressed) = s.memory.compressed_bytes {
        let _ = write!(mem_extra, "  ·  zip {}", human_bytes(compressed));
    }
    // Always show the verdict, not just the bad ones: a high used% is
    // normal on macOS (it caches aggressively), so "80% used" without
    // "pressure normal" next to it reads as a problem when it is not
    match s.memory.pressure_level {
        Some(1) => mem_extra.push_str("  ·  pressure normal"),
        Some(2) => mem_extra.push_str(&theme.paint(YELLOW, "  ·  pressure warning")),
        Some(level) if level > 2 => {
            mem_extra.push_str(&theme.paint(RED_BOLD, "  ·  pressure critical"));
        }
        _ => {}
    }
    let _ = writeln!(
        out,
        "MEM   {}  {:>5.1}%  {} / {}{mem_extra}",
        styled_gauge(mem_fraction, GAUGE_WIDTH, theme),
        s.memory.used_percent,
        human_bytes(s.memory.used_bytes),
        human_bytes(s.memory.total_bytes),
    );

    if s.memory.swap_total_bytes > 0 {
        let swap_fraction = f64::from(s.memory.swap_used_percent) / 100.0;
        let _ = writeln!(
            out,
            "SWP   {}  {:>5.1}%  {} / {}",
            styled_gauge(swap_fraction, GAUGE_WIDTH, theme),
            s.memory.swap_used_percent,
            human_bytes(s.memory.swap_used_bytes),
            human_bytes(s.memory.swap_total_bytes),
        );
    }

    // Load gauge is scaled against the core count: 1.0 per core = full
    let load_fraction = if s.cpu.logical_cores > 0 {
        s.load.load1 / f64::from(s.cpu.logical_cores)
    } else {
        0.0
    };
    let procs = s
        .total_processes
        .map(|n| format!("  ·  {n} procs"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "LOAD  {}  {:>6.2}  {:.2} · {:.2}{}",
        styled_gauge(load_fraction, GAUGE_WIDTH, theme),
        s.load.load1,
        s.load.load5,
        s.load.load15,
        procs,
    );

    if let Some(b) = &s.battery {
        let fraction = f64::from(b.charge_percent) / 100.0;
        // Low battery is the alarming direction, so the gauge colors are
        // inverted relative to a utilization bar
        let color = level_color(1.0 - fraction, 0.8, 0.9);
        // "discharging" is library vocabulary; what the user wants to
        // know is whether the machine is on wall power, and how long the
        // current state has left
        let (state, remaining) = match b.state.to_ascii_lowercase().as_str() {
            "charging" => ("charging", b.time_to_full_secs.map(|s| (s, "to full"))),
            "discharging" => ("on battery", b.time_to_empty_secs.map(|s| (s, "left"))),
            "full" => ("full, on AC", None),
            "empty" => ("empty", None),
            _ => ("unknown state", None),
        };
        let mut extra = String::new();
        if let Some((secs, label)) = remaining {
            let _ = write!(extra, "  ·  {} {label}", human_uptime(secs));
        }
        if let Some(w) = b.power_watts.filter(|w| *w > 0.1) {
            let _ = write!(extra, "  ·  {w:.1} W");
        }
        if let Some(h) = b.health_percent {
            let _ = write!(extra, "  ·  health {h:.0}%");
        }
        if let Some(c) = b.cycle_count {
            let _ = write!(extra, "  ·  {c} cycles");
        }
        let _ = writeln!(
            out,
            "BATT  {}  {:>5.1}%  {}{}",
            theme.paint(color, &gauge(fraction, GAUGE_WIDTH)),
            b.charge_percent,
            state,
            extra,
        );
    }

    if let Some(temps) = &s.temperatures {
        // The question a temperature line answers is "is this machine
        // hot?", and raw firmware sensor names ("PMU tdie8") answer it
        // for nobody. Lead with the hottest reading and a verdict; the
        // sensor that produced it is demoted to a parenthetical, and the
        // full list stays in the JSON output
        match temps.first() {
            None => {
                let _ = writeln!(out, "TEMP  (no sensors)");
            }
            Some(hottest) => {
                let fraction = f64::from(hottest.celsius) / 100.0;
                let color = level_color(fraction, TEMP_WARM, TEMP_HOT);
                let verdict = if fraction >= TEMP_HOT {
                    "hot"
                } else if fraction >= TEMP_WARM {
                    "warm"
                } else {
                    "normal"
                };
                let _ = writeln!(
                    out,
                    "TEMP  {}  {:>5.1}°C  {}  ·  {} sensors (max {})",
                    theme.paint(color, &gauge(fraction, GAUGE_WIDTH)),
                    hottest.celsius,
                    theme.paint(color, verdict),
                    temps.len(),
                    hottest.label,
                );
            }
        }
    }

    // Machine-wide disk/net rates (pure sums of the per-device lists)
    let io = &s.io_totals;
    if io.disk_read_bytes_per_sec.is_some()
        || io.disk_write_bytes_per_sec.is_some()
        || io.network_received_bytes_per_sec.is_some()
        || io.network_transmitted_bytes_per_sec.is_some()
    {
        let _ = writeln!(
            out,
            "IO    disk {}↓ {}↑  ·  net {}↓ {}↑",
            human_rate(io.disk_read_bytes_per_sec),
            human_rate(io.disk_write_bytes_per_sec),
            human_rate(io.network_received_bytes_per_sec),
            human_rate(io.network_transmitted_bytes_per_sec),
        );
    }

    if let Some(disks) = &s.disks {
        let rows: Vec<Vec<String>> = disks
            .iter()
            .map(|d| {
                let fraction = f64::from(d.used_percent) / 100.0;
                // Highlight disks over 90% usage in bold red
                let color = level_color(fraction, 0.7, 0.9);
                let kind = if d.kind.is_empty() {
                    "-".to_string()
                } else {
                    d.kind.clone()
                };
                vec![
                    d.mount_point.clone(),
                    kind,
                    theme.paint(color, &gauge(fraction, DISK_GAUGE_WIDTH)),
                    theme.paint(color, &format!("{:.1}%", d.used_percent)),
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
            &[
                "MOUNT", "KIND", "USED", "USE%", "FREE", "TOTAL", "READ", "WRITE",
            ],
            &[
                Align::Left,
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            &rows,
            theme,
        );
    }

    if let Some(networks) = &s.networks {
        // Fixed-height section: filtering to "interfaces with traffic"
        // made the row count change every tick, so everything below
        // jumped around in watch mode. Instead always show the top
        // NET_ROWS interfaces ranked by current traffic (idle ones fill
        // the rest, sorted by name for a stable order) — the height only
        // changes when the OS adds or removes an interface
        let total_rate = |n: &&zstats::NetworkSnapshot| {
            n.received_bytes_per_sec
                + n.transmitted_bytes_per_sec
                + n.received_errors_per_sec.unwrap_or(0)
                + n.transmitted_errors_per_sec.unwrap_or(0)
        };
        // Idle fill prefers meaningful interfaces: physical NICs (en* on
        // macOS and modern Linux) and loopback before the anpi/awdl/ap
        // house-keeping ones
        let class = |name: &str| {
            if name.starts_with("en") {
                0
            } else if name.starts_with("lo") {
                1
            } else {
                2
            }
        };
        let mut ranked: Vec<&zstats::NetworkSnapshot> = networks.iter().collect();
        ranked.sort_by(|a, b| {
            total_rate(b)
                .cmp(&total_rate(a))
                .then_with(|| class(&a.interface).cmp(&class(&b.interface)))
                .then_with(|| a.interface.cmp(&b.interface))
        });
        let rows: Vec<Vec<String>> = ranked
            .iter()
            .take(NET_ROWS)
            .map(|n| {
                let err_rx = n.received_errors_per_sec.unwrap_or(0);
                let err_tx = n.transmitted_errors_per_sec.unwrap_or(0);
                let errs = if err_rx > 0 || err_tx > 0 {
                    format!("{err_rx}/{err_tx}")
                } else {
                    "-".to_string()
                };
                vec![
                    n.interface.clone(),
                    human_rate(Some(n.received_bytes_per_sec)),
                    human_rate(Some(n.transmitted_bytes_per_sec)),
                    errs,
                ]
            })
            .collect();
        if rows.is_empty() {
            let _ = writeln!(out, "NET   (no interfaces)");
        } else {
            write_table(
                &mut out,
                "NET",
                &["IFACE", "RX↓", "TX↑", "ERR↓/↑"],
                &[Align::Left, Align::Right, Align::Right, Align::Right],
                &rows,
                theme,
            );
        }
    }

    if let Some(processes) = &s.processes {
        // cmd can be empty (e.g. no permission to read other users'
        // processes); fall back to "-" so the column stays readable
        let cmd_cell = |p: &ProcessSnapshot| {
            if p.cmd.is_empty() {
                "-".to_string()
            } else {
                truncate_width(&p.cmd, CMD_MAX_WIDTH)
            }
        };

        let avg_of = |p: &ProcessSnapshot| proc_averages.and_then(|m| m.get(&p.pid)).copied();
        let cpu_key = |p: &ProcessSnapshot| avg_of(p).map_or(p.cpu_usage_percent, |(c, _)| c);
        let mem_key = |p: &ProcessSnapshot| avg_of(p).map_or(p.memory_bytes, |(_, m)| m);

        // One PROC table: the top rows ranked by CPU, then the top memory
        // processes appended, so heavy-but-idle processes stay visible.
        // With rolling averages available (watch mode), rank by the average
        // so the ordering doesn't reshuffle on every tick
        let mut by_cpu: Vec<&ProcessSnapshot> = processes.iter().collect();
        by_cpu.sort_by(|a, b| cpu_key(b).total_cmp(&cpu_key(a)));
        let mut selected: Vec<&ProcessSnapshot> =
            by_cpu.iter().copied().take(PROC_CPU_ROWS).collect();

        let mut by_mem: Vec<&ProcessSnapshot> = processes.iter().collect();
        by_mem.sort_by_key(|p| std::cmp::Reverse(mem_key(p)));
        for p in by_mem {
            if selected.len() >= PROC_CPU_ROWS + PROC_MEM_ROWS {
                break;
            }
            if selected.iter().all(|q| q.pid != p.pid) {
                selected.push(p);
            }
        }

        let cpu_header = if proc_averages.is_some() {
            "CPU%(AVG)"
        } else {
            "CPU%"
        };
        let mem_header = if proc_averages.is_some() {
            "MEM(AVG)"
        } else {
            "MEM"
        };
        let rows: Vec<Vec<String>> = selected
            .iter()
            .map(|p| {
                let cpu_cell = match avg_of(p) {
                    Some((avg, _)) => format!("{:.1}({avg:.1})", p.cpu_usage_percent),
                    None => format!("{:.1}", p.cpu_usage_percent),
                };
                let mem_cell = match avg_of(p) {
                    Some((_, avg)) => {
                        format!("{}({})", human_bytes(p.memory_bytes), human_bytes(avg))
                    }
                    None => human_bytes(p.memory_bytes),
                };
                vec![
                    p.pid.to_string(),
                    p.name.clone(),
                    cpu_cell,
                    mem_cell,
                    cmd_cell(p),
                ]
            })
            .collect();
        write_table(
            &mut out,
            "PROC",
            &["PID", "NAME", cpu_header, mem_header, "CMD"],
            &[
                Align::Right,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Left,
            ],
            &rows,
            theme,
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

/// Compact uptime: `3d4h`, `5h12m`, `42m`, or `17s`
fn human_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem = secs % 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{rem}s")
    }
}

/// Format a rate; shows "-" when the first sample has no diff baseline
fn human_rate(rate: Option<u64>) -> String {
    match rate {
        Some(v) => format!("{}/s", human_bytes(v)),
        None => "-".to_string(),
    }
}

/// Truncate to at most `max` display columns, appending an ellipsis when cut
fn truncate_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut truncated = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        truncated.push(c);
        used += w;
    }
    truncated.push('…');
    truncated
}

/// Sink that prints text output in watch mode.
///
/// Keeps a rolling window ([`AVG_WINDOW`], via the lib's
/// [`ProcessWindows`]) of per-process CPU/memory samples so the PROC
/// table can rank by average instead of the latest value.
pub struct TextSink {
    windows: Mutex<ProcessWindows>,
    /// On a TTY: enable colors and repaint in place instead of scrolling
    interactive: bool,
    /// Optional dim footer line (e.g. key hints in watch mode)
    footer: Option<String>,
}

impl Default for TextSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TextSink {
    pub fn new() -> Self {
        Self {
            windows: Mutex::new(ProcessWindows::new(AVG_WINDOW)),
            interactive: std::io::stdout().is_terminal(),
            footer: None,
        }
    }

    pub fn with_footer(mut self, footer: &str) -> Self {
        self.footer = Some(footer.to_string());
        self
    }

    /// Record a snapshot into the rolling averages without rendering it —
    /// used to warm up from daemon history on attach. Samples are stamped
    /// with the absorb time, so a replayed backlog initially counts toward
    /// the window as if it had just been observed
    #[cfg(unix)] // its only caller, attach, is unix-only
    pub fn absorb(&self, snapshot: &SystemSnapshot) {
        if let Some(processes) = snapshot.processes.as_deref() {
            let _ = self.averages(processes);
        }
    }

    /// Record the current samples and return per-PID rolling averages
    fn averages(&self, processes: &[ProcessSnapshot]) -> ProcAverages {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        windows
            .record(Instant::now(), processes)
            .into_iter()
            .map(|(pid, stat)| (pid, (stat.cpu_avg as f32, stat.memory_avg_bytes as u64)))
            .collect()
    }
}

#[async_trait]
impl MetricSink for TextSink {
    async fn write(&self, snapshot: &SystemSnapshot) -> Result<(), SinkError> {
        let averages = snapshot.processes.as_deref().map(|ps| self.averages(ps));
        let theme = Theme {
            enabled: self.interactive,
        };
        let mut frame = render_with(snapshot, averages.as_ref(), theme);
        if let Some(footer) = &self.footer {
            frame.push_str(&theme.paint(DIM, footer));
            frame.push('\n');
        }

        if self.interactive {
            // Repaint in place: cursor home, clear each line's tail as we
            // overwrite it, then clear everything below the frame. This
            // avoids the flicker a full-screen clear would cause
            let frame = frame.replace('\n', "\x1b[K\n");
            let mut stdout = std::io::stdout();
            write!(stdout, "\x1b[H{frame}\x1b[J")?;
            stdout.flush()?;
        } else {
            println!("{frame}");
        }
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
    fn human_uptime_picks_two_leading_units() {
        assert_eq!(human_uptime(0), "0s");
        assert_eq!(human_uptime(17), "17s");
        assert_eq!(human_uptime(59), "59s");
        assert_eq!(human_uptime(60), "1m");
        assert_eq!(human_uptime(42 * 60), "42m");
        assert_eq!(human_uptime(3600), "1h0m");
        assert_eq!(human_uptime(5 * 3600 + 12 * 60 + 40), "5h12m");
        assert_eq!(human_uptime(86_400), "1d0h");
        assert_eq!(human_uptime(3 * 86_400 + 4 * 3600 + 59 * 60), "3d4h");
    }

    #[test]
    fn gauge_renders_fill_levels() {
        assert_eq!(gauge(0.0, 4), "░░░░");
        assert_eq!(gauge(0.5, 4), "██░░");
        assert_eq!(gauge(1.0, 4), "████");
        // Out-of-range fractions are clamped
        assert_eq!(gauge(1.5, 4), "████");
        assert_eq!(gauge(-0.5, 4), "░░░░");
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
            Theme::plain(),
        );
        let expected = "\
NET   IFACE              RX          TX
      en0        13.1 KiB/s  14.8 KiB/s
      bridge100   3.4 KiB/s     358 B/s
";
        assert_eq!(out, expected);
    }

    #[test]
    fn truncate_by_display_width() {
        assert_eq!(truncate_width("short", 10), "short");
        assert_eq!(truncate_width("abcdef", 4), "abc…");
        // Multi-byte chars must not be cut mid-codepoint
        assert_eq!(truncate_width("ééééé", 3), "éé…");
        // CJK chars occupy two display columns each
        assert_eq!(truncate_width("企业微信", 5), "企业…");
    }

    #[test]
    fn table_aligns_cjk_by_display_width() {
        let mut out = String::new();
        write_table(
            &mut out,
            "TOP",
            &["NAME", "MEM"],
            &[Align::Left, Align::Right],
            &[
                vec!["chrome".into(), "1.0 GiB".into()],
                vec!["企业微信".into(), "449.6 MiB".into()],
            ],
            Theme::plain(),
        );
        // "企业微信" is 8 display columns wide, so it sets the NAME width
        let expected = "\
TOP   NAME            MEM
      chrome      1.0 GiB
      企业微信  449.6 MiB
";
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_table_writes_nothing() {
        let mut out = String::new();
        write_table(
            &mut out,
            "DISK",
            &["NAME"],
            &[Align::Left],
            &[],
            Theme::plain(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn visible_width_ignores_ansi_escapes() {
        let theme = Theme { enabled: true };
        let painted = theme.paint(RED_BOLD, "abc");
        assert!(painted.contains("\x1b["));
        assert_eq!(visible_width(&painted), 3);
        // CJK still counts as two columns each
        assert_eq!(visible_width(&theme.paint(GREEN, "企业")), 4);
    }

    #[test]
    fn colored_cells_stay_aligned() {
        let theme = Theme { enabled: true };
        let mut out = String::new();
        write_table(
            &mut out,
            "DISK",
            &["MOUNT", "USE%"],
            &[Align::Left, Align::Right],
            &[
                vec!["/".into(), theme.paint(RED_BOLD, "98.8%")],
                vec!["/System".into(), "72.6%".into()],
            ],
            Theme::plain(),
        );
        // Both data lines must end at the same visible column
        let lines: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(visible_width(lines[0]), visible_width(lines[1]));
    }

    #[test]
    fn plain_theme_paints_nothing() {
        assert_eq!(Theme::plain().paint(RED_BOLD, "text"), "text");
    }

    fn proc(pid: u32, cpu: f32, mem: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            name: format!("p{pid}"),
            cmd: String::new(),
            cpu_usage_percent: cpu,
            cpu_time_ms: 0,
            memory_bytes: mem,
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
    fn text_sink_averages_across_writes_and_prunes_dead_pids() {
        let sink = TextSink::new();

        let first = sink.averages(&[proc(1, 10.0, 100)]);
        assert_eq!(first[&1], (10.0, 100));

        // Second sample: averages over both
        let second = sink.averages(&[proc(1, 20.0, 300)]);
        assert!((second[&1].0 - 15.0).abs() < 0.01);
        assert_eq!(second[&1].1, 200);

        // PID 1 gone: its history must be pruned
        let third = sink.averages(&[proc(2, 1.0, 1)]);
        assert!(!third.contains_key(&1));
        assert_eq!(third[&2], (1.0, 1));
    }
}

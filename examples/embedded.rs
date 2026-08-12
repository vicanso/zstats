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

//! Skeleton of a single-process frontend (e.g. a tray GUI) that embeds
//! collection instead of talking to a `zstats serve` daemon: quitting the
//! process stops everything — collection, alerting, and recording.
//!
//! Run with: `cargo run --example embedded`
//!
//! The dependency shape this models is
//! `zstats = { default-features = false, features = ["frontend", "client"] }`:
//! no CLI, no `runtime`, and the consumer never touches tokio.
//! [`zstats::Monitor`] wires collection, the alert rules, rolling
//! averages and history persistence together; the caller owns the loop,
//! so a GUI decides which thread ticks and which thread paints.
//!
//! One embedded collector must be the ONLY collector: running this next
//! to `zstats serve` would duplicate notifications and metrics records. A
//! real frontend checks `zstats::client::is_running()` at startup and
//! takes over (`zstats::client::stop()`, async — bring a small tokio
//! runtime or spawn `zstats stop`) before collecting.

use std::sync::mpsc;
use std::time::Duration;

use zstats::Monitor;

fn main() {
    // Isolated config dir so the example never touches ~/.zstats or a
    // running daemon's data; a real frontend uses
    // `zstats::settings::default_dir()`
    let config_dir = std::env::temp_dir().join(format!("zstats-embedded-{}", std::process::id()));

    // -------- settings: the same file model + validation as the CLI -----
    let mut file = zstats::settings::load(&config_dir).expect("load settings");
    zstats::settings::apply_add(&mut file, "alert-cpu", "40").expect("set threshold");
    zstats::settings::apply_add(&mut file, "alert-cpu", "ghostty=100").expect("set override");
    zstats::settings::save(&config_dir, &file).expect("save settings");

    // -------- collection thread: a plain thread is the "callback" -------
    // tick() blocks (collection is a syscall), so it runs off the UI
    // thread and hands finished work over a channel
    let (tx, rx) = mpsc::channel();
    let collect_thread = std::thread::spawn({
        let config_dir = config_dir.clone();
        move || {
            let mut monitor = Monitor::new(&config_dir).expect("monitor");
            println!(
                "settings: alert-cpu={:?} overrides={:?} alerts_enabled={}",
                monitor.settings().alerts.cpu,
                monitor.settings().alerts.cpu_overrides,
                monitor.alerts_enabled(),
            );
            for _ in 0..4 {
                match monitor.tick() {
                    // Everything the UI needs, already evaluated and
                    // persisted: raw sample, alerts to deliver, smoothed
                    // per-process averages to rank a table by
                    Ok(tick) => {
                        if tx.send(tick).is_err() {
                            break; // receiver dropped = tray quit
                        }
                    }
                    Err(e) => eprintln!("collect failed: {e}"),
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });

    // -------- the "UI thread": consume ticks ----------------------------
    let mut rounds = 0;
    while let Ok(tick) = rx.recv() {
        rounds += 1;
        println!(
            "round {rounds}: cpu {:5.1}%  {} procs averaged  {} alerts  {} records",
            tick.snapshot.cpu.usage_percent,
            tick.process_stats.len(),
            tick.alerts.len(),
            tick.records.len(),
        );
        // A real frontend delivers these its own way — in-app banner,
        // system notification, a log line. Persisting them already
        // happened inside tick()
        for alert in &tick.alerts {
            println!("  ALERT: {}", alert.summary());
        }
    }
    collect_thread.join().expect("collector thread");

    // -------- history: what a chart view reads --------------------------
    let today = jiff::Zoned::now().date();
    let monitor = Monitor::new(&config_dir).expect("monitor");
    let history = monitor.history(today, today).expect("history");
    println!("history for {today}: {} record(s)", history.len());

    // -------- daemon takeover check (sync — no tokio runtime) -----------
    #[cfg(unix)]
    println!("daemon running elsewhere: {}", zstats::client::is_running());

    let _ = std::fs::remove_dir_all(&config_dir);
    println!("embedded frontend loop complete");
}

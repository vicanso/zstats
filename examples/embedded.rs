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
//! no CLI, no `runtime`, and the consumer never touches tokio — collection
//! is a plain thread driving the synchronous `LocalCollector`, and the
//! channel receiver plays the role of the UI thread's callback.
//!
//! One embedded collector must be the ONLY collector: running this next to
//! `zstats serve` would duplicate notifications and metrics records. A
//! real frontend checks `zstats::client::is_running()` at startup and
//! takes over (`zstats::client::stop()`, async — bring a small tokio
//! runtime or spawn `zstats stop`) before collecting.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use zstats::alerts::{ActiveThresholds, AlertEngine};
use zstats::rolling::ProcessWindows;
use zstats::{Collector, CollectorConfig, LocalCollector};

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
    let file = zstats::settings::load(&config_dir).expect("reload settings");
    println!(
        "settings: alert-cpu={:?} overrides={:?}",
        file.alerts.cpu, file.alerts.cpu_overrides
    );

    // -------- alert engine driven by those settings ---------------------
    // Thresholds are passed per evaluate() call: after a settings-panel
    // save, rebuild them — no engine state is lost
    let thresholds = ActiveThresholds::from_config(&file.alerts);
    let mut engine = AlertEngine::new();

    // -------- rolling 1-minute averages for a process table -------------
    let mut windows = ProcessWindows::new(Duration::from_secs(60));

    // -------- collection: a plain thread + channel is the "callback" ----
    let (tx, rx) = mpsc::channel();
    let collect_thread = std::thread::spawn(move || {
        let mut collector = LocalCollector::new(CollectorConfig::default());
        for _ in 0..4 {
            match collector.collect() {
                Ok(snapshot) => {
                    if tx.send(snapshot).is_err() {
                        break; // receiver dropped = tray quit: stop collecting
                    }
                }
                Err(e) => eprintln!("collect failed: {e}"),
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });

    // -------- the "UI thread": consume snapshots ------------------------
    let mut rounds = 0;
    while let Ok(snapshot) = rx.recv() {
        rounds += 1;
        let evaluation = engine.evaluate(Instant::now(), &snapshot, &thresholds);
        let averaged = snapshot
            .processes
            .as_deref()
            .map(|ps| windows.record(Instant::now(), ps).len())
            .unwrap_or(0);
        println!(
            "round {rounds}: cpu {:5.1}%  {} procs averaged  {} alerts  {} records",
            snapshot.cpu.usage_percent,
            averaged,
            evaluation.events.len(),
            evaluation.records.len(),
        );
        // A real frontend delivers evaluation.events its own way (in-app
        // banner / system notification) and persists the records:
        if !evaluation.records.is_empty() {
            let today = jiff::Zoned::now().date();
            zstats::records::append(&config_dir, today, &evaluation.records)
                .expect("append records");
        }
    }
    collect_thread.join().expect("collector thread");

    // -------- history: write + read back through the shared format ------
    // (a fabricated record, since the rules need a ~50s-full window and
    // this demo only runs a few seconds)
    let today = jiff::Zoned::now().date();
    let sample = zstats::records::MetricRecord {
        timestamp: jiff::Timestamp::now(),
        pid: 1,
        name: "demo".into(),
        cpu_avg_percent: 42.0,
        memory_avg_bytes: 1 << 20,
        memory_share_percent: 4.2,
    };
    zstats::records::append(&config_dir, today, &[sample]).expect("append history");
    let history = zstats::records::read_range(&config_dir, today, today).expect("read history");
    println!(
        "history: {} record(s), first = {}",
        history.len(),
        history[0].name
    );
    let removed = zstats::records::cleanup(&config_dir, today, 30);
    println!("cleanup: {} expired file(s)", removed.len());

    // -------- daemon takeover check (sync — no tokio runtime) -----------
    #[cfg(unix)]
    println!("daemon running elsewhere: {}", zstats::client::is_running());

    let _ = std::fs::remove_dir_all(&config_dir);
    println!("embedded frontend loop complete");
}

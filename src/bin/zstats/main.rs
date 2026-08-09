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

mod render;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use render::TextSink;
use zstats::{
    Collector, CollectorConfig, LocalCollector, MetricSink, Scheduler, StdoutSink, SystemSnapshot,
};

const USAGE: &str = "\
zstats - system performance metrics collector

Usage:
  zstats [options]

By default, collects once and prints human-readable text.

Options:
  --watch             Collect and print continuously; Ctrl+C to exit
  --interval <ms>     Collection interval in watch mode, in milliseconds (default 2000)
  --json              Output as JSON (single line, machine-friendly)
  --pretty            Output as pretty-printed JSON (implies --json)
  --no-processes      Skip process collection
  --no-disks          Skip disk collection
  --no-networks       Skip network collection
  --max-processes <n> Max number of processes to collect (default 50)
  -h, --help          Show this help
";

#[derive(Clone, Copy, PartialEq)]
enum OutputFormat {
    Text,
    Json,
    JsonPretty,
}

struct CliArgs {
    watch: bool,
    interval: Duration,
    format: OutputFormat,
    config: CollectorConfig,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = CliArgs {
        watch: false,
        interval: Duration::from_millis(2000),
        format: OutputFormat::Text,
        config: CollectorConfig::default(),
    };

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--watch" => args.watch = true,
            "--json" => {
                if args.format == OutputFormat::Text {
                    args.format = OutputFormat::Json;
                }
            }
            "--pretty" => args.format = OutputFormat::JsonPretty,
            "--no-processes" => args.config.collect_processes = false,
            "--no-disks" => args.config.collect_disks = false,
            "--no-networks" => args.config.collect_networks = false,
            "--interval" => {
                let value = iter
                    .next()
                    .ok_or("--interval requires a value in milliseconds")?;
                let ms: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid interval: {value}"))?;
                if ms == 0 {
                    return Err("--interval must be greater than 0".into());
                }
                args.interval = Duration::from_millis(ms);
            }
            "--max-processes" => {
                let value = iter.next().ok_or("--max-processes requires a number")?;
                args.config.max_processes = value
                    .parse()
                    .map_err(|_| format!("invalid number: {value}"))?;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other} (use -h for help)")),
        }
    }
    Ok(args)
}

fn format_snapshot(snapshot: &SystemSnapshot, format: OutputFormat) -> String {
    match format {
        OutputFormat::Text => render::render(snapshot),
        OutputFormat::Json => {
            serde_json::to_string(snapshot).expect("snapshot is always serializable")
        }
        OutputFormat::JsonPretty => {
            serde_json::to_string_pretty(snapshot).expect("snapshot is always serializable")
        }
    }
}

/// One-shot mode: sample twice (500ms apart) to get meaningful CPU usage
/// and IO rates
fn run_once(config: CollectorConfig, format: OutputFormat) -> ExitCode {
    let mut collector = LocalCollector::new(config);
    if let Err(e) = collector.collect() {
        eprintln!("collect failed: {e}");
        return ExitCode::FAILURE;
    }
    std::thread::sleep(Duration::from_millis(500));

    match collector.collect() {
        Ok(snapshot) => {
            println!("{}", format_snapshot(&snapshot, format));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("collect failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Watch mode: collect and print at the given interval; Ctrl+C to exit
async fn run_watch(config: CollectorConfig, interval: Duration, format: OutputFormat) -> ExitCode {
    let collector = LocalCollector::new(config);
    let sink: Arc<dyn MetricSink> = match format {
        OutputFormat::Text => Arc::new(TextSink),
        OutputFormat::Json => Arc::new(StdoutSink::new()),
        OutputFormat::JsonPretty => Arc::new(StdoutSink::pretty()),
    };

    let mut scheduler = Scheduler::new(Box::new(collector), vec![sink], interval);
    if scheduler.start().await.is_err() {
        eprintln!("failed to start scheduler");
        return ExitCode::FAILURE;
    }

    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl-c");
    scheduler.stop().await;
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    if !args.watch {
        return run_once(args.config, args.format);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run_watch(args.config, args.interval, args.format))
}

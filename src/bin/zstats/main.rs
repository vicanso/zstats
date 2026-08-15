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

#[cfg(unix)]
mod alerts;
#[cfg(unix)]
mod daemon;
mod keys;
mod render;
mod settings;

use std::io::IsTerminal;
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
  zstats                      Foreground live view: everything sampled at a
                              fixed 2s interval, screen refreshes in place.
                              Keys: q/Ctrl+C quit, d detach into the daemon
  zstats serve                Background daemon driven by the config directory
                              (detaches by default; --foreground to stay)
  zstats attach               Attach to the daemon: history replay + live view.
                              Keys: q/Ctrl+C/d detach (daemon keeps running)
  zstats stop                 Stop the daemon
  zstats -add <key> <value>   Persist a setting into <config-dir>/config.toml;
                              a running daemon reloads alert settings within
                              ~30 collects, other changes apply on restart
  zstats -remove <key> [name] Reset a setting to its builtin default (name
                              drops a per-process alert override)
  zstats -list                Show the config file

Options:
  --config-dir <path>  Config directory (default ~/.zstats)
  --foreground         serve: stay in the foreground
  --json / --pretty    Print one JSON snapshot instead of the live view
  -h, --help           Show this help
  -V, --version        Print the version

Config keys for -add (also accepted as key=value). Durations take 500ms /
2s / 5m / 1h, or a bare integer meaning milliseconds; 0 = every collect:
  interval <dur>              [daemon] collection interval (default 2s)
  history <dur>               [daemon] history retention (default 5m)
  detach <bool>               [daemon] serve detaches by default (default true)
  process-interval <dur>      [collector] process cadence (serve default 10s)
  disk-interval <dur>         [collector] disk cadence (default 0)
  disk-storage-interval <dur> [collector] disk capacity cadence (default 60s)
  network-interval <dur>      [collector] network cadence (default 0)
  temp-interval <dur>         [collector] temperature cadence (default 15s)
  cpu-freq-interval <dur>     [collector] CPU frequency cadence (default 30s)
  battery-interval <dur>      [collector] battery cadence (default 30s)
  process-boost <cores>       [collector] busy-cores boost (default 2, 0 = off)
  max-processes <n>           [collector] kept processes (default 50)
  collect-processes | collect-disks | collect-networks | collect-temperatures
  collect-battery | process-disk-io | process-groups | dedupe-disks
  per-core-cpu                                         <true|false>
  alert-cpu <pct|name=pct>    [alerts] CPU rules: 5-min avg >= pct (chronic)
                              or 1-min avg >= 3x pct (runaway); name=pct sets
                              a per-process override, e.g. alert-cpu ghostty=100
                              name may lead and/or end with * to cover a family
                              of processes whose names are not stable, e.g.
                              alert-cpu 'rust-analyzer*=200' (quote it: the
                              shell would expand a bare *)
  alert-mem <pct|name=pct>    [alerts] 5-min avg memory rule, same forms
                              (default 25% of total). The effective bar is
                              the LOWER of this and alert-mem-bytes, so it
                              means the same thing on a laptop and a
                              workstation; 0 disables the rule. A per-name
                              override replaces both halves
  alert-mem-bytes <size>      [alerts] absolute ceiling for that rule
                              (default 4GiB; 0 removes it). Takes 4GiB /
                              512MiB / 2GB or bytes
  alert-app-cpu <pct|name=pct>  [alerts] whole-app CPU rule over a process
                              tree (default 200; catches the browser whose
                              helpers each stay under the per-process bar)
  alert-app-mem <pct|name=pct>  [alerts] whole-app memory-share rule (40)
  alert-disk <pct|mount=pct>  [alerts] volume used-capacity crossing alert
                              (default 90; re-arms 5 pts below; 0 disables)
  alert-pressure <level>      [alerts] kernel memory-pressure alert, macOS:
                              off | warning (default) | critical; alerts only
                              once the level persists (5m / 1m at critical),
                              and names the biggest memory holders so the
                              alert says what to close
  alert-cooldown <dur>        [alerts] minimum gap between EPISODES for one
                              process+rule (default 10m); a persisting
                              condition notifies once, plus one follow-up
                              after 30m — never on every cooldown
  alert-template <bool>       [alerts] builtin per-app override template
                              (default true; your overrides always win).
                              Drop a file at <config-dir>/template.toml to
                              replace the builtin table without rebuilding —
                              serve reloads it within ~1min of an mtime
                              change, so refreshing it can be a cron curl
";

#[derive(Clone, Copy, PartialEq)]
enum OutputFormat {
    Text,
    Json,
    JsonPretty,
}

/// Foreground live view: a fixed 2s beat for everything
const FOREGROUND_INTERVAL: Duration = Duration::from_secs(2);

/// Force every subsystem to refresh on each collect: the foreground view
/// shows current numbers, cadence throttles are for the daemon. Collection
/// toggles (what to collect) still come from the config file
fn foreground_config(mut config: CollectorConfig) -> CollectorConfig {
    config.process_refresh_interval = Duration::ZERO;
    config.cpu_frequency_refresh_interval = Duration::ZERO;
    config.disk_storage_refresh_interval = Duration::ZERO;
    config.disk_io_refresh_interval = Duration::ZERO;
    config.network_refresh_interval = Duration::ZERO;
    config.temperature_refresh_interval = Duration::ZERO;
    config
}

struct CliArgs {
    format: OutputFormat,
    /// serve: stay in the foreground instead of detaching
    foreground: bool,
}

fn parse_args(raw: Vec<String>) -> Result<CliArgs, String> {
    let mut args = CliArgs {
        format: OutputFormat::Text,
        foreground: false,
    };

    for arg in raw {
        match arg.as_str() {
            "--foreground" => args.foreground = true,
            "--json" => {
                if args.format == OutputFormat::Text {
                    args.format = OutputFormat::Json;
                }
            }
            "--pretty" => args.format = OutputFormat::JsonPretty,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("zstats {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other} (use -h for help)")),
        }
    }
    Ok(args)
}

/// Handle the persistent-config actions (`-add`, `-remove`, `-list`):
/// apply them to `<config-dir>/config.toml` and exit
fn run_config_actions(raw: &[String]) -> ExitCode {
    let mut config = match settings::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let mut modified = false;
    let mut list = false;
    let mut iter = raw.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-add" | "--add" => {
                let Some(first) = iter.next() else {
                    eprintln!("-add requires <key> <value> (or key=value)");
                    return ExitCode::FAILURE;
                };
                // Both `-add key value` and `-add key=value` are accepted;
                // for alert overrides the value itself contains '=', e.g.
                // `-add alert-cpu ghostty=100`
                let (key, value) = match first.split_once('=') {
                    Some((key, value)) => (key.to_string(), value.to_string()),
                    None => {
                        let Some(value) = iter.next() else {
                            eprintln!("-add {first} requires a value");
                            return ExitCode::FAILURE;
                        };
                        (first.clone(), value.clone())
                    }
                };
                match settings::apply_add(&mut config, &key, &value) {
                    Ok(description) => {
                        println!("{description}");
                        modified = true;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "-remove" | "--remove" => {
                let Some(key) = iter.next() else {
                    eprintln!("-remove requires <key> [name]");
                    return ExitCode::FAILURE;
                };
                // Optional trailing name for per-process alert overrides
                let name = match iter.peek() {
                    Some(next) if !next.starts_with('-') => iter.next().map(|s| s.as_str()),
                    _ => None,
                };
                match settings::apply_remove(&mut config, key, name) {
                    Ok(description) => {
                        println!("{description}");
                        modified = true;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "-list" | "--list" => list = true,
            other => {
                eprintln!("cannot combine {other} with config actions (use -h for help)");
                return ExitCode::FAILURE;
            }
        }
    }

    if modified {
        if let Err(e) = settings::save(&config) {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
        println!("saved to {}", settings::path().display());
    }
    if list {
        let path = settings::path();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                println!("# {}", path.display());
                print!("{content}");
            }
            Err(_) => println!(
                "(no config file at {}; builtin defaults apply)",
                path.display()
            ),
        }
    }
    ExitCode::SUCCESS
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

/// Hand the current watch session over to a background daemon
#[cfg(unix)]
fn detach_into_daemon() -> ExitCode {
    if daemon::is_running() {
        println!("zstats daemon is already running; view it with `zstats attach`");
        return ExitCode::SUCCESS;
    }
    // Re-run ourselves as `serve` (keeping e.g. --config-dir); --foreground
    // stops the already-detached child from detaching again
    let mut args = vec!["serve".to_string()];
    args.extend(
        std::env::args()
            .skip(1)
            .filter(|a| a != "--json" && a != "--pretty"),
    );
    args.push("--foreground".to_string());
    match spawn_detached(args) {
        Ok(pid) => {
            println!(
                "zstats daemon started (pid {pid}, log: {}); reattach with `zstats attach`",
                daemon::log_path().display()
            );
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn detach_into_daemon() -> ExitCode {
    eprintln!("daemon mode is only supported on unix");
    ExitCode::FAILURE
}

/// Watch mode: collect and print at the given interval
async fn run_watch(config: CollectorConfig, interval: Duration, format: OutputFormat) -> ExitCode {
    let collector = LocalCollector::new(config);
    let interactive = format == OutputFormat::Text && std::io::stdout().is_terminal();
    let sink: Arc<dyn MetricSink> = match format {
        OutputFormat::Text => {
            let mut sink = TextSink::new();
            if interactive {
                sink = sink.with_footer("q quit · d detach into daemon");
            }
            Arc::new(sink)
        }
        OutputFormat::Json => Arc::new(StdoutSink::new()),
        OutputFormat::JsonPretty => Arc::new(StdoutSink::pretty()),
    };

    // Text output on a TTY repaints in place: switch to the alternate
    // screen (like top/htop) and hide the cursor, restoring both on exit
    if interactive {
        render::enter_live_screen();
    }

    let mut scheduler = Scheduler::new(Box::new(collector), vec![sink], interval);
    if scheduler.start().await.is_err() {
        eprintln!("failed to start scheduler");
        return ExitCode::FAILURE;
    }

    let exit = keys::wait_live_exit(interactive).await;
    scheduler.stop().await;
    render::leave_live_screen(interactive);

    match exit {
        keys::LiveExit::Quit => ExitCode::SUCCESS,
        keys::LiveExit::Detach => detach_into_daemon(),
    }
}

/// Spawn ourselves detached from the terminal, with stdout/stderr appended
/// to the daemon log file so alert lines and warnings are not lost
#[cfg(unix)]
fn spawn_detached(args: Vec<String>) -> Result<u32, String> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let exe =
        std::env::current_exe().map_err(|e| format!("failed to locate own executable: {e}"))?;
    let log = daemon::log_path();
    let open_log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
    };
    let (stdout, stderr) = match (open_log(), open_log()) {
        (Ok(out), Ok(err)) => (Stdio::from(out), Stdio::from(err)),
        _ => (Stdio::null(), Stdio::null()),
    };

    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map(|child| child.id())
        .map_err(|e| format!("failed to start daemon: {e}"))
}

/// Re-exec ourselves detached from the terminal (for `serve --detach`)
#[cfg(unix)]
fn detach_self() -> ExitCode {
    // Probe first: the detached child's stderr goes to the log file, so its
    // own "already running" check would be easy to miss while we'd still
    // report a started pid
    if daemon::is_running() {
        eprintln!("zstats daemon is already running");
        return ExitCode::FAILURE;
    }

    // The child must not detach again (the config file may set
    // daemon.detach = true): force it into the foreground
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();
    args.push("--foreground".to_string());
    match spawn_detached(args) {
        Ok(pid) => {
            println!(
                "zstats daemon started (pid {pid}, log: {})",
                daemon::log_path().display()
            );
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

/// Log timestamps in the machine's local zone. The daemon log gets read
/// by a person lining it up against what they saw on screen, and UTC
/// makes that mental arithmetic. The offset stays in the output, so a
/// pasted line is still unambiguous.
///
/// Uses jiff rather than tracing-subscriber's own `LocalTime`, whose
/// backing crate refuses to resolve the local offset in a multi-threaded
/// process and silently drops the timestamp instead.
#[cfg(unix)]
struct LocalTime;

#[cfg(unix)]
impl tracing_subscriber::fmt::time::FormatTime for LocalTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%.6f%:z")
        )
    }
}

fn runtime() -> tokio::runtime::Runtime {
    // The async work here is tiny — a collect tick, one UDS accept loop,
    // a few attached clients. Two workers are plenty; the default would
    // park one thread per logical core (12+ on a desktop) for nothing
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

fn main() -> ExitCode {
    // Die silently on SIGPIPE like standard unix tools instead of panicking
    // when the read end of a pipe closes (e.g. `zstats attach | head`)
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let mut raw: Vec<String> = std::env::args().skip(1).collect();

    // --config-dir applies to every mode (including the config actions
    // below), so resolve it before anything reads settings
    if let Some(i) = raw.iter().position(|a| a == "--config-dir") {
        let Some(dir) = raw.get(i + 1).cloned() else {
            eprintln!("--config-dir requires a path");
            return ExitCode::FAILURE;
        };
        settings::set_dir(std::path::PathBuf::from(dir));
        raw.drain(i..=i + 1);
    }

    // Persistent-config actions are standalone: apply and exit
    if raw.iter().any(|a| {
        matches!(
            a.as_str(),
            "-add" | "--add" | "-remove" | "--remove" | "-list" | "--list"
        )
    }) {
        return run_config_actions(&raw);
    }

    let subcommand = raw.first().filter(|a| !a.starts_with('-')).cloned();
    if subcommand.is_some() {
        raw.remove(0);
    }

    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    // serve is driven by the config directory (default ~/.zstats); the
    // foreground view only takes its collection toggles from there.
    // attach/stop don't need the file (a broken config must not block
    // stopping a daemon)
    let needs_file = matches!(subcommand.as_deref(), None | Some("serve"));
    let file = if needs_file {
        match settings::load() {
            Ok(file) => file,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        settings::FileConfig::default()
    };
    let base_collector = file.collector.clone().unwrap_or_default();

    match subcommand.as_deref() {
        None => {
            // Foreground mode: everything sampled fresh at a fixed 2s
            // interval — the cadence throttles exist for the long-running
            // daemon, a live view wants current numbers
            let config = foreground_config(base_collector);
            match args.format {
                OutputFormat::Text => {
                    runtime().block_on(run_watch(config, FOREGROUND_INTERVAL, OutputFormat::Text))
                }
                format => run_once(config, format),
            }
        }
        #[cfg(unix)]
        Some("serve") => {
            // Background mode: detaches unless the config or --foreground
            // says otherwise
            let detach = !args.foreground && file.daemon.detach.unwrap_or(true);
            if detach {
                return detach_self();
            }

            // Timestamped, leveled logs for the daemon: visible in the
            // foreground, appended to the log file when detached. This also
            // surfaces the lib Scheduler's own tracing warnings (sink
            // failures, collect timeouts), which are dropped otherwise.
            // Level via ZSTATS_LOG (error|warn|info|debug|trace), default info
            let level = std::env::var("ZSTATS_LOG")
                .ok()
                .and_then(|l| l.parse::<tracing::Level>().ok())
                .unwrap_or(tracing::Level::INFO);
            let _ = tracing_subscriber::fmt()
                .with_max_level(level)
                .with_timer(LocalTime)
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .try_init();

            let interval = file.daemon.interval.unwrap_or(Duration::from_millis(2000));
            let history = file.daemon.history.unwrap_or(Duration::from_secs(300));
            let mut config = base_collector;
            // The daemon runs long in the background: default its process
            // list to a 10s cadence (~0.45% CPU instead of ~1.3%) unless
            // the config file sets one
            if config.process_refresh_interval.is_zero() {
                config.process_refresh_interval = Duration::from_secs(10);
            }

            let mut extra_sinks: Vec<Arc<dyn MetricSink>> = Vec::new();
            // Alert settings come from the config file and the optional
            // template override (both hot-reloaded by the sink on mtime
            // change, every 30 collects). A malformed template fails
            // fast here, exactly like a malformed config: at startup a
            // silently-ignored template is a silently-missing alert
            let template = settings::load_template().unwrap_or_else(|e| {
                eprintln!("zstats: {e}");
                std::process::exit(1);
            });
            let alert_sink = alerts::AlertSink::from_config(&file.alerts, &template);
            if alert_sink.enabled() {
                extra_sinks.push(Arc::new(alert_sink));
            }
            runtime().block_on(daemon::serve(config, interval, history, extra_sinks))
        }
        #[cfg(unix)]
        Some("attach") => runtime().block_on(daemon::attach()),
        #[cfg(unix)]
        Some("stop") => runtime().block_on(daemon::stop()),
        Some(other) => {
            eprintln!("unknown command: {other} (use -h for help)");
            ExitCode::FAILURE
        }
    }
}

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
  zstats [options]         Collect once and print (default)
  zstats serve [options]   Run the collector daemon, keeping recent history
  zstats attach            Attach to the daemon: replay history, then live view.
                           Keys: q/Ctrl+C/d detach (daemon keeps running)
  zstats stop              Stop the daemon

Options:
  --watch                  Collect continuously in the foreground; on a TTY the
                           screen refreshes in place with colors.
                           Keys: q/Ctrl+C quit, d detach into a daemon
  --interval <ms>          Collection interval for watch/serve (default 2000)
  --history <secs>         serve: how much history to keep (default 300)
  --detach                 serve: fork to the background and return
  --json                   Output as JSON (single line, machine-friendly)
  --pretty                 Output as pretty-printed JSON (implies --json)
  --no-processes           Skip process collection
  --no-disks               Skip disk collection
  --no-networks            Skip network collection
  --process-interval <ms>  Process list refresh cadence; 0 = every collect
                           (default: 0, but serve defaults to 10000)
  --process-boost <cores>  While overall load is at least this many logical
                           cores of work, refresh processes every collect
                           (default 1, 0 = off). Scales with CPU count:
                           1 core ≈ 25% overall on 4U, ~1.6% on 64U
  --alert-cpu <pct>        serve: notify when a process averages at least this
                           CPU% (single-core units) over the last minute
                           (default 30, 0 = off). Repeat with name=pct for
                           per-process overrides, e.g. --alert-cpu ghostty=100
  --alert-mem <pct>        serve: notify when a process's 1-minute average
                           share of total memory reaches this percentage
                           (default 25, 0 = off); also supports name=pct
  --alert-cooldown <secs>  serve: minimum time between repeated alerts for
                           the same process and rule (default 600)
  --add-alert <spec>       Persist a per-process alert override to
                           ~/.zstats/config.toml and exit. Spec is
                           [cpu:|mem:]name=pct, e.g. ghostty=100 or
                           mem:chrome=40; pct 0 disables that process.
                           serve picks the file up on its next start
  --remove-alert <spec>    Remove a persisted override ([cpu:|mem:]name;
                           without a prefix removes both rules) and exit
  --list-alerts            Show the persisted alert configuration and exit
  --max-processes <n>      Max number of processes to collect (default 50)
  --process-disk-io        Also collect per-process disk read/write rates
                           (off by default; adds cost to process refresh)
  --no-dedupe-disks        Keep every mount point (default collapses APFS
                           synthetic mounts that share a device name)
  -h, --help               Show this help
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
    history: Duration,
    detach: bool,
    /// None = mode-dependent default (serve throttles to 10s, others
    /// refresh on every collect)
    process_interval: Option<Duration>,
    /// Daemon alert thresholds. Outer `None` = flag not given (fall back
    /// to the config file, then the builtin default); inner `None` = rule
    /// explicitly disabled with 0. Overrides are per-process-name
    /// exceptions from `name=pct` flag values
    alert_cpu: Option<Option<f32>>,
    alert_cpu_overrides: Vec<(String, Option<f32>)>,
    alert_mem_fraction: Option<Option<f64>>,
    alert_mem_overrides: Vec<(String, Option<f64>)>,
    /// Minimum time between repeated alerts; None = not given on the CLI
    alert_cooldown: Option<Duration>,
}

fn parse_args(raw: Vec<String>) -> Result<CliArgs, String> {
    let mut args = CliArgs {
        watch: false,
        interval: Duration::from_millis(2000),
        format: OutputFormat::Text,
        config: CollectorConfig::default(),
        history: Duration::from_secs(300),
        detach: false,
        process_interval: None,
        alert_cpu: None,
        alert_cpu_overrides: Vec::new(),
        alert_mem_fraction: None,
        alert_mem_overrides: Vec::new(),
        alert_cooldown: None,
    };

    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--watch" => args.watch = true,
            "--detach" => args.detach = true,
            "--history" => {
                let value = iter.next().ok_or("--history requires seconds")?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid history: {value}"))?;
                args.history = Duration::from_secs(secs);
            }
            "--alert-cpu" => {
                let value = iter
                    .next()
                    .ok_or("--alert-cpu requires a percentage or name=pct")?;
                if let Some((name, pct)) = value.split_once('=') {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(format!("invalid process name in: {value}"));
                    }
                    let pct: f32 = pct
                        .parse()
                        .map_err(|_| format!("invalid percentage: {value}"))?;
                    args.alert_cpu_overrides
                        .push((name.to_string(), if pct <= 0.0 { None } else { Some(pct) }));
                } else {
                    let pct: f32 = value
                        .parse()
                        .map_err(|_| format!("invalid percentage: {value}"))?;
                    args.alert_cpu = Some(if pct <= 0.0 { None } else { Some(pct) });
                }
            }
            "--alert-mem" => {
                let value = iter
                    .next()
                    .ok_or("--alert-mem requires a percentage or name=pct")?;
                if let Some((name, pct)) = value.split_once('=') {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(format!("invalid process name in: {value}"));
                    }
                    let pct: f64 = pct
                        .parse()
                        .map_err(|_| format!("invalid percentage: {value}"))?;
                    args.alert_mem_overrides.push((
                        name.to_string(),
                        if pct <= 0.0 { None } else { Some(pct / 100.0) },
                    ));
                } else {
                    let pct: f64 = value
                        .parse()
                        .map_err(|_| format!("invalid percentage: {value}"))?;
                    args.alert_mem_fraction =
                        Some(if pct <= 0.0 { None } else { Some(pct / 100.0) });
                }
            }
            "--alert-cooldown" => {
                let value = iter.next().ok_or("--alert-cooldown requires seconds")?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid cooldown: {value}"))?;
                args.alert_cooldown = Some(Duration::from_secs(secs));
            }
            "--process-boost" => {
                let value = iter
                    .next()
                    .ok_or("--process-boost requires a core count (e.g. 1 or 2.5)")?;
                let cores: f32 = value
                    .parse()
                    .map_err(|_| format!("invalid core count: {value}"))?;
                args.config.process_boost_cpu_cores = if cores <= 0.0 { None } else { Some(cores) };
            }
            "--process-interval" => {
                let value = iter
                    .next()
                    .ok_or("--process-interval requires a value in milliseconds")?;
                let ms: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid interval: {value}"))?;
                args.process_interval = Some(Duration::from_millis(ms));
            }
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
            "--process-disk-io" => args.config.collect_process_disk_io = true,
            "--no-dedupe-disks" => args.config.dedupe_disks = false,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other} (use -h for help)")),
        }
    }
    Ok(args)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum AlertRule {
    Cpu,
    Mem,
}

/// Split an optional `cpu:` / `mem:` prefix off an alert spec; no prefix
/// means the CPU rule
fn split_alert_rule(spec: &str) -> (AlertRule, &str) {
    if let Some(rest) = spec.strip_prefix("mem:") {
        (AlertRule::Mem, rest)
    } else if let Some(rest) = spec.strip_prefix("cpu:") {
        (AlertRule::Cpu, rest)
    } else {
        (AlertRule::Cpu, spec)
    }
}

/// Parse an `--add-alert` spec: `[cpu:|mem:]name=pct`
fn parse_alert_spec(spec: &str) -> Result<(AlertRule, String, f64), String> {
    let (rule, rest) = split_alert_rule(spec);
    let (name, pct) = rest
        .split_once('=')
        .ok_or_else(|| format!("invalid spec: {spec} (expected [cpu:|mem:]name=pct)"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("invalid spec: {spec} (empty process name)"));
    }
    let pct: f64 = pct
        .trim()
        .parse()
        .map_err(|_| format!("invalid percentage in: {spec}"))?;
    if pct < 0.0 {
        return Err(format!("invalid percentage in: {spec} (must be >= 0)"));
    }
    Ok((rule, name.to_string(), pct))
}

/// Handle the persistent-config actions (`--add-alert`, `--remove-alert`,
/// `--list-alerts`): apply them to ~/.zstats/config.toml and exit
fn run_alert_config_actions(raw: &[String]) -> ExitCode {
    let mut config = match settings::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let mut modified = false;
    let mut list = false;
    let mut iter = raw.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--add-alert" => {
                let Some(spec) = iter.next() else {
                    eprintln!("--add-alert requires [cpu:|mem:]name=pct");
                    return ExitCode::FAILURE;
                };
                match parse_alert_spec(spec) {
                    Ok((AlertRule::Cpu, name, pct)) => {
                        println!("cpu alert for {name}: {pct}%");
                        config.alerts.cpu_overrides.insert(name, pct as f32);
                    }
                    Ok((AlertRule::Mem, name, pct)) => {
                        println!("mem alert for {name}: {pct}%");
                        config.alerts.mem_overrides.insert(name, pct);
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::FAILURE;
                    }
                }
                modified = true;
            }
            "--remove-alert" => {
                let Some(spec) = iter.next() else {
                    eprintln!("--remove-alert requires [cpu:|mem:]name");
                    return ExitCode::FAILURE;
                };
                let (explicit, name) = match spec.split_once(':') {
                    Some(("cpu", name)) => (Some(AlertRule::Cpu), name),
                    Some(("mem", name)) => (Some(AlertRule::Mem), name),
                    _ => (None, spec.as_str()),
                };
                let mut removed = false;
                if explicit != Some(AlertRule::Mem) {
                    removed |= config.alerts.cpu_overrides.remove(name).is_some();
                }
                if explicit != Some(AlertRule::Cpu) {
                    removed |= config.alerts.mem_overrides.remove(name).is_some();
                }
                if removed {
                    println!("removed alert override for {name}");
                    modified = true;
                } else {
                    println!("no alert override found for {name}");
                }
            }
            "--list-alerts" => list = true,
            other => {
                eprintln!("cannot combine {other} with alert config actions (use -h for help)");
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
        let a = &config.alerts;
        println!("alerts config ({}):", settings::path().display());
        match a.cpu {
            Some(p) if p > 0.0 => println!("  cpu default: {p}%"),
            Some(_) => println!("  cpu default: off"),
            None => println!("  cpu default: 30% (builtin)"),
        }
        match a.mem {
            Some(p) if p > 0.0 => println!("  mem default: {p}%"),
            Some(_) => println!("  mem default: off"),
            None => println!("  mem default: 25% (builtin)"),
        }
        match a.cooldown_secs {
            Some(s) => println!("  cooldown: {s}s"),
            None => println!("  cooldown: 600s (builtin)"),
        }
        println!("  cpu overrides:");
        if a.cpu_overrides.is_empty() {
            println!("    (none)");
        }
        for (name, pct) in &a.cpu_overrides {
            println!("    {name} = {pct}%");
        }
        println!("  mem overrides:");
        if a.mem_overrides.is_empty() {
            println!("    (none)");
        }
        for (name, pct) in &a.mem_overrides {
            println!("    {name} = {pct}%");
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
    // Re-run ourselves as `serve`, keeping the collection flags but
    // dropping the foreground-only ones
    let mut args = vec!["serve".to_string()];
    args.extend(
        std::env::args()
            .skip(1)
            .filter(|a| a != "--watch" && a != "--json" && a != "--pretty"),
    );
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

    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
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

    // Persistent-config actions are standalone: apply and exit
    if raw
        .iter()
        .any(|a| a == "--add-alert" || a == "--remove-alert" || a == "--list-alerts")
    {
        return run_alert_config_actions(&raw);
    }

    let subcommand = raw.first().filter(|a| !a.starts_with('-')).cloned();
    if subcommand.is_some() {
        raw.remove(0);
    }

    let mut args = match parse_args(raw) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    // The daemon runs long in the background: throttle its process list to
    // a 10s cadence by default (~0.45% CPU instead of ~1.3%); an explicit
    // --process-interval 0 restores per-collect refresh
    args.config.process_refresh_interval = match (args.process_interval, subcommand.as_deref()) {
        (Some(interval), _) => interval,
        (None, Some("serve")) => Duration::from_secs(10),
        (None, _) => Duration::ZERO,
    };

    match subcommand.as_deref() {
        None => {
            if !args.watch {
                return run_once(args.config, args.format);
            }
            runtime().block_on(run_watch(args.config, args.interval, args.format))
        }
        #[cfg(unix)]
        Some("serve") => {
            if args.detach {
                return detach_self();
            }
            let mut extra_sinks: Vec<Arc<dyn MetricSink>> = Vec::new();
            // Alert settings: CLI flags > config file > builtin defaults.
            // The file is loaded fail-fast here and then hot-reloaded by
            // the sink on mtime change (every 30 collects)
            let file = match settings::load() {
                Ok(file) => file,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            let cli_options = alerts::AlertCliOptions {
                cpu: args.alert_cpu,
                cpu_overrides: args.alert_cpu_overrides,
                mem: args.alert_mem_fraction,
                mem_overrides: args.alert_mem_overrides,
                cooldown: args.alert_cooldown,
            };
            let alert_sink = alerts::AlertSink::from_options(cli_options, &file.alerts);
            if alert_sink.enabled() {
                extra_sinks.push(Arc::new(alert_sink));
            }
            runtime().block_on(daemon::serve(
                args.config,
                args.interval,
                args.history,
                extra_sinks,
            ))
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

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

//! Per-process alert evaluation: sustained high CPU or a large share of
//! total memory over the last minute.
//!
//! This module is the pure rule engine — thresholds, rolling windows,
//! cooldowns, and the metrics-recording criteria — shared by every
//! frontend so CLI and GUI agree on what "over threshold" means. How an
//! [`AlertEvent`] reaches the user (desktop notification, in-app banner)
//! is deliberately the frontend's business, as is persisting the returned
//! [`MetricRecord`]s (see [`crate::records`]).
//!
//! [`AlertEvent`] is pure data — [`AlertSubject`] (whose pid / process
//! tree / volume) plus [`AlertDetail`] (every number the rule looked at,
//! in its own units). No wording is stored: [`AlertEvent::summary`]
//! renders the default English line on demand, and a frontend with its
//! own layout, language or gauges reads the fields instead.
//! [`AlertEvent::kind`] and [`AlertEvent::severity`] are derived, so they
//! cannot disagree with the data. Everything serialises, so events can
//! cross an IPC boundary or be logged structurally.
//!
//! Only sustained behavior alerts, in two tiers sharing one threshold
//! and one cooldown per (pid, kind):
//! - acute/runaway: 1-minute average CPU ≥ [`ACUTE_FACTOR`] × the
//!   process's effective threshold — fast notification;
//! - chronic: 5-minute average CPU (or memory share) at or above the
//!   effective threshold — "quietly always busy", confirmed by
//!   persistence; self-limiting bursts never get that far.
//!
//! The volume-capacity rule is different in kind: capacity is a slow
//! state, so it alerts on the upward crossing of the threshold and
//! re-arms only after dropping [`DISK_REARM_MARGIN`] below — one
//! notification per episode, no cooldown, per-mount overrides.
//!
//! System memory pressure is driven by the kernel's own verdict
//! (`pressure_level`) but requires PERSISTENCE, not just a crossing: a
//! memory-heavy machine sits at warning as its steady state and would
//! otherwise alert all day. Its episode is symmetric — it takes
//! [`PRESSURE_REARM`] of continuous normal to declare it over, the same
//! way it takes persistence to declare it started — and because the
//! condition can legitimately hold for hours, reminders inside one
//! episode BACK OFF ([`PERSIST_REMINDER`] doubling up to
//! [`PRESSURE_REMINDER_MAX`]) instead of stopping at one.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::records::MetricRecord;
use crate::rolling::ProcessWindows;
use crate::settings::AlertsConfig;
use crate::snapshot::SystemSnapshot;

/// Fast window: the acute (runaway) rule and metrics recording
const WINDOW: Duration = Duration::from_secs(60);
/// The fast window only fires once it is this full: startup blips never
/// alert
const MIN_SPAN: Duration = Duration::from_secs(50);
/// The chronic window spans this many acute windows: "chronic" is defined
/// as persistence across several acute periods, not as an independent
/// absolute — tuning [`WINDOW`] keeps the relationship
const SLOW_FACTOR: u64 = 5;
/// Slow window: the chronic rules (5 minutes with the current acute
/// window). One acute period cannot distinguish a self-limiting burst
/// (an editor re-indexing, a page load) from a process that is genuinely
/// always busy — several can
const SLOW_WINDOW: Duration = Duration::from_secs(WINDOW.as_secs() * SLOW_FACTOR);
const SLOW_MIN_SPAN: Duration = Duration::from_secs(MIN_SPAN.as_secs() * SLOW_FACTOR);
/// Acute rule: 1-minute average at or above this many times the
/// process's EFFECTIVE threshold (override or base — so template'd apps
/// like browsers get 3x their raised bar, not 3x the default, and a
/// video call does not read as a runaway)
const ACUTE_FACTOR: f64 = 3.0;
/// How often qualifying processes are reported as metric records
const RECORD_EVERY: Duration = Duration::from_secs(60);
/// Additionally record this many processes per pass purely for having
/// consumed the most CPU TIME in the last window, whatever their
/// percentages.
///
/// This is the only recording criterion that can see a process no
/// threshold will ever catch. Every rule in this file compares an
/// average against a bar, which structurally cannot notice a process
/// sitting at a steady 8% — yet twelve hours of that is 1 core-hour,
/// several times what a ten-minute runaway costs. Rules stay
/// threshold-based on purpose (a low-bar-long-window rule would fire on
/// every legitimate resident daemon, and a 12-hour window rarely even
/// completes on a laptop); the answer is to keep the DATA so the
/// question can be asked afterwards, which is the same
/// interruption-versus-history split the overrides already follow
const RECORD_TOP_CPU_TIME: usize = 5;
/// A condition still true this long after it first notified earns one
/// follow-up — insurance against the first notification being missed,
/// well clear of the cooldown so it can never feel like nagging
const PERSIST_REMINDER: Duration = Duration::from_secs(30 * 60);

/// How long the kernel must report normal before an above-normal
/// memory-pressure episode counts as over.
///
/// The fire side already demands persistence; the clear side used to
/// accept a single normal sample, and that asymmetry was a bug. The
/// kernel's level is a noisy step function, so a machine sitting near
/// the line dips back for a sample or two regularly — each dip ended
/// the episode and the next one alerted again as if it were news.
/// Measured on a real machine: four "new" warning alerts in 5.5h for
/// what was one continuous condition, all of them `repeat=false`. It
/// takes the same evidence to call the episode over as to call it
/// started, so this mirrors the warning tier's [`SLOW_WINDOW`]
const PRESSURE_REARM: Duration = SLOW_WINDOW;

/// Ceiling on the gap between reminders inside one memory-pressure
/// episode; the gap starts at [`PERSIST_REMINDER`] and doubles
/// (30m, 1h, 2h, 4h, 4h…).
///
/// Pressure is the one rule whose condition is a machine STATE rather
/// than a culprit: there is often nothing to do about it beyond closing
/// something, and it can hold for a whole workday. So unlike the
/// per-process rules — which notify once, follow up once, then go quiet
/// because you either acted or decided not to — this one keeps a sparse
/// pulse going for as long as the machine is actually swapping. The
/// backoff is what makes that bearable: 6 notifications across 12 hours,
/// each one flagged as a repeat rather than posing as a new event
const PRESSURE_REMINDER_MAX: Duration = Duration::from_secs(4 * 60 * 60);

/// Builtin defaults when the config file leaves a value unset
const DEFAULT_CPU_PERCENT: f32 = 30.0;
const DEFAULT_MEMORY_FRACTION: f64 = 0.25;
const DEFAULT_DISK_FRACTION: f32 = 0.90;
/// Whole-app defaults sit well above the per-process ones: an app is
/// allowed to be bigger than any one of its processes, and the point of
/// this rule is the total that per-process thresholds cannot see
const DEFAULT_APP_CPU_PERCENT: f32 = 200.0;
const DEFAULT_APP_MEMORY_FRACTION: f64 = 0.40;
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(600);

/// The disk rule is crossing-based, not windowed (capacity is a slow
/// state, not a rate): one alert when a volume crosses its threshold,
/// then silence until it drops this far below and crosses again — no
/// "still full" nagging, no cooldown involved
const DISK_REARM_MARGIN: f64 = 0.05;

/// Builtin CPU-override template: resident apps that legitimately sustain
/// The builtin template, compiled in from `templates/alerts.toml`.
///
/// It is a FILE rather than a table in this source for two reasons. It
/// is data, not logic — 144 entries that change as apps and toolchains
/// come and go, and every change used to be a code edit plus a hand-kept
/// duplicate in `config.example.toml`. And it is the same format, read
/// by the same parser, as the optional `<config-dir>/template.toml`
/// override, so updating the table never requires a new binary.
const BUILTIN_TEMPLATE: &str = include_str!("../templates/alerts.toml");

/// Format version of `templates/alerts.toml`. A file claiming anything
/// else is refused outright rather than half-read into thresholds the
/// user did not ask for
pub const TEMPLATE_VERSION: u32 = 1;

/// Raised (or zeroed) alert bars for processes that legitimately sustain
/// high CPU or a large share of RAM.
///
/// Applied below the user's own overrides (a matching user entry always
/// wins) unless `[alerts] template = false`, and skipped entirely when
/// the corresponding base rule is disabled. Keys are [`Matcher`]
/// patterns; see `templates/alerts.toml` for the tiers and the reasoning
/// behind each group.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// Absent in a hand-written file only if the author forgot; treated
    /// as a mismatch so it cannot be silently skipped
    pub version: Option<u32>,
    /// Per-process CPU, single-core percent
    #[serde(default)]
    pub cpu: std::collections::BTreeMap<String, f32>,
    /// Whole-application CPU, keyed on the tree root name
    #[serde(default)]
    pub app_cpu: std::collections::BTreeMap<String, f32>,
    /// Whole-application memory, percent of total
    #[serde(default)]
    pub app_mem: std::collections::BTreeMap<String, f64>,
}

impl Template {
    /// Parse one template file. Rejects a version mismatch, an unknown
    /// table, and any key the [`Matcher`] cannot honour — a template is
    /// only useful if you can trust that every line in it took effect
    pub fn parse(text: &str) -> Result<Self, String> {
        let template: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        if template.version != Some(TEMPLATE_VERSION) {
            return Err(format!(
                "unsupported template version {:?} (this build reads {TEMPLATE_VERSION})",
                template.version
            ));
        }
        let keys = template
            .cpu
            .keys()
            .chain(template.app_cpu.keys())
            .chain(template.app_mem.keys());
        for key in keys {
            Matcher::parse(key)?;
        }
        Ok(template)
    }

    /// The compiled-in template. Parsed once; a failure here is a build
    /// bug, and `builtin_template_parses` keeps `make check` honest
    pub fn builtin() -> &'static Self {
        static BUILTIN: std::sync::OnceLock<Template> = std::sync::OnceLock::new();
        BUILTIN.get_or_init(|| {
            Template::parse(BUILTIN_TEMPLATE).expect("the compiled-in template must parse")
        })
    }
}

/// Which rule produced an alert — the category a frontend groups,
/// filters or picks an icon by.
///
/// `Cpu`/`Memory`/`AppCpu`/`AppMemory` double as [`EpisodeState`] keys;
/// `Disk` and `Pressure` have their own state machines and appear only
/// on events.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Cpu,
    Memory,
    /// Whole-application totals, keyed by the group's root pid
    AppCpu,
    AppMemory,
    /// A volume crossing its used-capacity threshold
    Disk,
    /// The kernel's system memory-pressure verdict
    Pressure,
}

/// How alarming this is — enough for a frontend to choose a color
/// without parsing the message
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Sustained behavior worth knowing about
    Warning,
    /// A runaway process, or the kernel calling memory pressure critical
    Critical,
}

/// What the alert is about, so a frontend can link the notification back
/// to a row in its process table, a volume, or the machine itself
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AlertSubject {
    Process {
        pid: u32,
        name: String,
    },
    /// A whole process tree; `root_pid` matches
    /// `ProcessGroupSnapshot::root_pid`
    App {
        root_pid: u32,
        name: String,
        process_count: u32,
    },
    Volume {
        mount_point: String,
    },
    /// The machine as a whole (memory pressure)
    System,
}

/// The measurement behind an alert — every number the rule looked at,
/// in its own units, so a frontend can render a gauge, a chart or its
/// own wording without parsing text back apart
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "measure")]
pub enum AlertDetail {
    Cpu {
        /// Average over `window`, single-core percent (100 = one core)
        avg_percent: f64,
        /// The bar actually crossed — already multiplied by
        /// [`ACUTE_FACTOR`] when `runaway`
        threshold_percent: f64,
        /// The averaging window this came from
        window: Duration,
        /// The fast tier: several times the configured threshold
        runaway: bool,
    },
    Memory {
        avg_bytes: u64,
        share_percent: f64,
        threshold_percent: f64,
        window: Duration,
    },
    Disk {
        used_percent: f64,
        threshold_percent: f64,
        available_bytes: u64,
        total_bytes: u64,
    },
    Pressure {
        /// The kernel's verdict: 2 = warning, 4 = critical. Not a scale,
        /// which is why there is no percentage here
        level: u32,
        /// How long the episode had lasted when this fired
        sustained: Duration,
        swap_used_bytes: u64,
        swap_total_bytes: u64,
        compressed_bytes: Option<u64>,
    },
}

/// An alert that should reach the user now.
///
/// Pure data: what happened ([`AlertDetail`]) and to whom
/// ([`AlertSubject`]). Wording is not baked in — [`AlertEvent::summary`]
/// renders the default English one-liner on demand (and `Display` does
/// the same), while a frontend that wants its own layout, its own
/// language, or a gauge instead of a sentence reads the fields directly.
/// [`AlertEvent::kind`] and [`AlertEvent::severity`] are derived, so they
/// can never disagree with the data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertEvent {
    pub subject: AlertSubject,
    pub detail: AlertDetail,
    /// Set when this is the [`PERSIST_REMINDER`] follow-up rather than a
    /// newly crossed condition, carrying how long it had been going — a
    /// frontend may want to style or collapse those differently
    pub repeat_after: Option<Duration>,
}

impl AlertEvent {
    /// The category to group, filter or pick an icon by
    pub fn kind(&self) -> AlertKind {
        let app = matches!(self.subject, AlertSubject::App { .. });
        match self.detail {
            AlertDetail::Cpu { .. } if app => AlertKind::AppCpu,
            AlertDetail::Cpu { .. } => AlertKind::Cpu,
            AlertDetail::Memory { .. } if app => AlertKind::AppMemory,
            AlertDetail::Memory { .. } => AlertKind::Memory,
            AlertDetail::Disk { .. } => AlertKind::Disk,
            AlertDetail::Pressure { .. } => AlertKind::Pressure,
        }
    }

    /// How alarming this is — enough to choose a color
    pub fn severity(&self) -> Severity {
        match self.detail {
            AlertDetail::Cpu { runaway: true, .. } => Severity::Critical,
            AlertDetail::Pressure { level, .. } if level >= 4 => Severity::Critical,
            _ => Severity::Warning,
        }
    }

    /// The default one-line rendering, in English. A frontend with its
    /// own layout or language should build from the fields instead
    pub fn summary(&self) -> String {
        let who = self.subject.label();
        let mut text = match &self.detail {
            AlertDetail::Cpu {
                avg_percent,
                threshold_percent,
                window,
                runaway,
            } => format!(
                "{who}{} averaged {avg_percent:.0}% CPU over {} (threshold {threshold_percent:.0}%)",
                if *runaway { " runaway:" } else { "" },
                window_label(*window),
            ),
            AlertDetail::Memory {
                avg_bytes,
                share_percent,
                threshold_percent,
                window,
            } => format!(
                "{who} averaged {:.1} GiB — {share_percent:.0}% of total memory — over {} \
                 (threshold {threshold_percent:.0}%)",
                *avg_bytes as f64 / f64::from(1 << 30),
                window_label(*window),
            ),
            AlertDetail::Disk {
                used_percent,
                available_bytes,
                total_bytes,
                ..
            } => format!(
                "{who} is {used_percent:.0}% full — {:.1} GiB free of {:.1} GiB",
                *available_bytes as f64 / f64::from(1 << 30),
                *total_bytes as f64 / f64::from(1 << 30),
            ),
            AlertDetail::Pressure {
                level,
                sustained,
                swap_used_bytes,
                swap_total_bytes,
                compressed_bytes,
            } => format!(
                "{who} memory pressure: {} for {} min — swap {:.1}/{:.1} GiB{}",
                if *level >= 4 { "critical" } else { "warning" },
                sustained.as_secs() / 60,
                *swap_used_bytes as f64 / f64::from(1 << 30),
                *swap_total_bytes as f64 / f64::from(1 << 30),
                compressed_bytes
                    .map(|b| format!(", compressor {:.1} GiB", b as f64 / f64::from(1 << 30)))
                    .unwrap_or_default(),
            ),
        };
        if let Some(elapsed) = self.repeat_after {
            text.push_str(&format!(
                " — still going after {} min",
                elapsed.as_secs() / 60
            ));
        }
        text
    }
}

impl std::fmt::Display for AlertEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl AlertSubject {
    /// How this subject is named in a sentence
    fn label(&self) -> String {
        match self {
            Self::Process { pid, name } => format!("{name} (pid {pid})"),
            Self::App {
                name,
                process_count,
                ..
            } => format!("{name} ({process_count} processes)"),
            Self::Volume { mount_point } => format!("disk {mount_point}"),
            Self::System => "system".to_string(),
        }
    }
}

fn window_label(window: Duration) -> String {
    match window.as_secs() / 60 {
        0 | 1 => "the last minute".to_string(),
        minutes => format!("the last {minutes} minutes"),
    }
}

/// Result of one evaluation pass
#[derive(Default)]
pub struct Evaluation {
    pub events: Vec<AlertEvent>,
    /// Data points for the daily metrics history (non-empty only on
    /// minute boundaries); persisting them is the caller's business
    pub records: Vec<MetricRecord>,
}

/// How an override key matches a process name (always
/// case-insensitive).
///
/// A process name is not a stable identifier. Tool managers stamp the
/// version into the binary itself — Zed ships rust-analyzer as
/// `rust-analyzer-2026-08-10.1` and rewrites that name on every update
/// — and one tool commonly runs as several sibling processes
/// (`rust-analyzer-proc-macro-srv`). Exact matching missed both
/// silently: the builtin `rust-analyzer` entry had never applied on any
/// Zed install, so the indexer alerted against the 30% base instead of
/// its 100% bar.
///
/// Only a leading and/or trailing `*` is special — this is deliberately
/// not a glob engine, just enough to name a family. A `*` anywhere else
/// is REJECTED at parse time rather than treated as a literal, so a key
/// that looks like a pattern can never quietly behave as an exact name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    Exact(String),
    /// `name*`
    Prefix(String),
    /// `*name`
    Suffix(String),
    /// `*name*`
    Contains(String),
}

impl Matcher {
    /// Read an override key. Fails only on an interior `*`
    pub fn parse(key: &str) -> Result<Self, String> {
        let leading = key.starts_with('*');
        let rest = if leading { &key[1..] } else { key };
        let trailing = rest.ends_with('*');
        let literal = if trailing {
            &rest[..rest.len() - 1]
        } else {
            rest
        };
        if literal.contains('*') {
            return Err(format!(
                "`{key}`: `*` is only supported at the start or end of a name"
            ));
        }
        let literal = literal.to_string();
        Ok(match (leading, trailing) {
            (false, false) => Self::Exact(literal),
            (false, true) => Self::Prefix(literal),
            (true, false) => Self::Suffix(literal),
            (true, true) => Self::Contains(literal),
        })
    }

    fn literal(&self) -> &str {
        match self {
            Self::Exact(s) | Self::Prefix(s) | Self::Suffix(s) | Self::Contains(s) => s,
        }
    }

    /// Compared as ASCII bytes so no allocation or char-boundary
    /// slicing is needed for a name that may be arbitrary UTF-8
    pub fn matches(&self, name: &str) -> bool {
        let literal = self.literal().as_bytes();
        let name = name.as_bytes();
        let fits = name.len() >= literal.len();
        match self {
            Self::Exact(_) => name.eq_ignore_ascii_case(literal),
            Self::Prefix(_) => fits && name[..literal.len()].eq_ignore_ascii_case(literal),
            Self::Suffix(_) => {
                fits && name[name.len() - literal.len()..].eq_ignore_ascii_case(literal)
            }
            // `windows(0)` panics, and "contains nothing" is true anyway
            Self::Contains(_) => {
                literal.is_empty()
                    || (fits
                        && name
                            .windows(literal.len())
                            .any(|w| w.eq_ignore_ascii_case(literal)))
            }
        }
    }
}

/// One override entry: which names it claims, what it sets them to, and
/// which layer wrote it
struct Override<T> {
    matcher: Matcher,
    value: Option<T>,
    /// A user entry outranks a template entry whatever their shapes: an
    /// explicit override is a request, a template entry is only a
    /// better default
    user: bool,
}

impl<T> Override<T> {
    /// Higher wins. Layer first, then specificity: an exact name beats
    /// any pattern, and a longer pattern beats a shorter one.
    ///
    /// This has to be explicit rather than "first match wins in file
    /// order", because the config's override maps are `BTreeMap`s —
    /// what reaches us is alphabetical, not the order anything was
    /// written in
    fn rank(&self) -> (bool, bool, usize) {
        (
            self.user,
            matches!(self.matcher, Matcher::Exact(_)),
            self.matcher.literal().len(),
        )
    }
}

/// A rule threshold: a default plus per-process-name overrides
/// (case-insensitive, optionally patterns — see [`Matcher`]). An
/// override of `None` disables the rule for that process — so a
/// legitimately busy app (e.g. a terminal rendering streaming AI
/// output) can get a higher bar without being blind to a real runaway.
pub struct Thresholds<T> {
    default: Option<T>,
    overrides: Vec<Override<T>>,
}

impl<T: Copy> Thresholds<T> {
    pub fn new(default: Option<T>) -> Self {
        Self {
            default,
            overrides: Vec::new(),
        }
    }

    /// An override the user asked for. An unparseable key is kept as a
    /// literal name: `apply_add` already rejects those, so anything
    /// that reaches here from an old or hand-edited file should behave
    /// as it did before patterns existed rather than vanish
    pub fn with_override(self, name: String, value: Option<T>) -> Self {
        self.push(name, value, true)
    }

    /// An override from the builtin template, outranked by any user
    /// entry that also matches
    pub fn with_template_override(self, name: String, value: Option<T>) -> Self {
        self.push(name, value, false)
    }

    fn push(mut self, name: String, value: Option<T>, user: bool) -> Self {
        let matcher = Matcher::parse(&name).unwrap_or(Matcher::Exact(name));
        self.overrides.push(Override {
            matcher,
            value,
            user,
        });
        self
    }

    /// The effective threshold for a process: the highest-ranking
    /// matching override (see [`Override::rank`]), otherwise the
    /// default. `None` = rule disabled for this process
    pub fn for_process(&self, name: &str) -> Option<T> {
        let mut best: Option<&Override<T>> = None;
        for entry in &self.overrides {
            // Strictly greater, so equally ranked entries keep the
            // old "first one wins" behaviour
            if entry.matcher.matches(name) && best.is_none_or(|b| entry.rank() > b.rank()) {
                best = Some(entry);
            }
        }
        best.map_or(self.default, |entry| entry.value)
    }

    /// The base threshold, ignoring per-process overrides — this is what
    /// metrics recording uses
    pub fn base(&self) -> Option<T> {
        self.default
    }

    pub fn any_enabled(&self) -> bool {
        self.default.is_some() || self.overrides.iter().any(|o| o.value.is_some())
    }
}

/// The currently effective thresholds (config file over builtin defaults)
pub struct ActiveThresholds {
    pub cpu: Thresholds<f32>,
    pub memory: Thresholds<f64>,
    /// Volume used-capacity fractions, override key = mount point
    pub disk: Thresholds<f32>,
    /// Kernel memory-pressure level at which to start alerting (2 =
    /// warning, 4 = critical); None disables the rule
    pub pressure: Option<u32>,
    /// Whole-application totals, override key = the group's root name
    pub app_cpu: Thresholds<f32>,
    pub app_memory: Thresholds<f64>,
    /// Per (process, rule) re-alert cooldown while a condition persists
    pub cooldown: Duration,
}

impl ActiveThresholds {
    /// Merge the `[alerts]` config section over the builtin template over
    /// the builtin defaults (CPU 30%, memory 25%, cooldown 10m)
    pub fn from_config(file: &AlertsConfig) -> Self {
        Self::from_config_with_template(file, Template::builtin())
    }

    /// As [`Self::from_config`], but against a template the caller
    /// loaded — `<config-dir>/template.toml` replacing the compiled-in
    /// one, so the table can be updated without a new binary.
    ///
    /// Per-name precedence: a user override always beats a matching
    /// template entry, template entries fill the names the user did not
    /// configure, and every other process falls through to the base
    /// value. `[alerts] template = false` drops the template layer
    /// entirely. A configured 0 disables the rule; a 0 override disables
    /// it for that process only
    pub fn from_config_with_template(file: &AlertsConfig, template: &Template) -> Self {
        let cpu_default = match file.cpu {
            Some(p) if p > 0.0 => Some(p),
            Some(_) => None,
            None => Some(DEFAULT_CPU_PERCENT),
        };
        let mem_default = match file.mem {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(DEFAULT_MEMORY_FRACTION),
        };

        // User entries outrank template entries by layer, not by push
        // order — see Override::rank
        let mut cpu = Thresholds::new(cpu_default);
        for (name, pct) in &file.cpu_overrides {
            cpu = cpu.with_override(name.clone(), (*pct > 0.0).then_some(*pct));
        }
        // The template is a refinement of the base rule, not a source of
        // alerts in its own right: disabling the base value must not
        // leave ~60 templated apps still firing. An explicit user
        // override is different — that one is a request, and it stands
        if file.template.unwrap_or(true) && cpu_default.is_some() {
            for (name, pct) in &template.cpu {
                cpu = cpu.with_template_override(name.clone(), (*pct > 0.0).then_some(*pct));
            }
        }
        let mut memory = Thresholds::new(mem_default);
        for (name, pct) in &file.mem_overrides {
            memory = memory.with_override(name.clone(), (*pct > 0.0).then_some(*pct / 100.0));
        }
        let disk_default = match file.disk {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(DEFAULT_DISK_FRACTION),
        };
        let mut disk = Thresholds::new(disk_default);
        for (mount, pct) in &file.disk_overrides {
            disk = disk.with_override(mount.clone(), (*pct > 0.0).then_some(*pct / 100.0));
        }

        let app_cpu_default = match file.app_cpu {
            Some(p) if p > 0.0 => Some(p),
            Some(_) => None,
            None => Some(DEFAULT_APP_CPU_PERCENT),
        };
        let mut app_cpu = Thresholds::new(app_cpu_default);
        for (name, pct) in &file.app_cpu_overrides {
            app_cpu = app_cpu.with_override(name.clone(), (*pct > 0.0).then_some(*pct));
        }
        if file.template.unwrap_or(true) && app_cpu_default.is_some() {
            for (name, pct) in &template.app_cpu {
                app_cpu =
                    app_cpu.with_template_override(name.clone(), (*pct > 0.0).then_some(*pct));
            }
        }
        let app_mem_default = match file.app_mem {
            Some(p) if p > 0.0 => Some(p / 100.0),
            Some(_) => None,
            None => Some(DEFAULT_APP_MEMORY_FRACTION),
        };
        let mut app_memory = Thresholds::new(app_mem_default);
        for (name, pct) in &file.app_mem_overrides {
            app_memory =
                app_memory.with_override(name.clone(), (*pct > 0.0).then_some(*pct / 100.0));
        }
        if file.template.unwrap_or(true) && app_mem_default.is_some() {
            for (name, pct) in &template.app_mem {
                app_memory = app_memory
                    .with_template_override(name.clone(), (*pct > 0.0).then_some(*pct / 100.0));
            }
        }

        Self {
            cpu,
            memory,
            disk,
            app_cpu,
            app_memory,
            pressure: file
                .pressure
                .unwrap_or(crate::settings::PressureAlert::Warning)
                .level(),
            cooldown: file.cooldown.unwrap_or(DEFAULT_COOLDOWN),
        }
    }

    pub fn any_enabled(&self) -> bool {
        self.cpu.any_enabled()
            || self.memory.any_enabled()
            || self.disk.any_enabled()
            || self.app_cpu.any_enabled()
            || self.app_memory.any_enabled()
            || self.pressure.is_some()
    }
}

/// The alert rule engine: rolling windows, cooldown state, and the
/// once-a-minute recording gate. Feed it every snapshot via
/// [`AlertEngine::evaluate`]; thresholds are passed per call so the
/// caller can hot-reload them without touching accumulated state
#[derive(Default)]
pub struct AlertEngine {
    /// 1-minute windows: acute rule + recording
    fast: Option<ProcessWindows>,
    /// 5-minute windows: chronic rules
    slow: Option<ProcessWindows>,
    /// 5-minute windows over whole-application totals, keyed by root pid
    apps: Option<ProcessWindows>,
    episodes: EpisodeState,
    /// Mounts that already fired the disk alert and have not yet dropped
    /// [`DISK_REARM_MARGIN`] below their threshold
    disk_disarmed: std::collections::HashSet<String>,
    /// When the current above-normal pressure episode began; None only
    /// once the kernel has reported normal for [`PRESSURE_REARM`]
    pressure_since: Option<Instant>,
    /// When the kernel first reported normal INSIDE an ongoing episode.
    /// The episode ends only once this has held for [`PRESSURE_REARM`];
    /// a shorter dip is noise and leaves the episode untouched
    pressure_normal_since: Option<Instant>,
    /// Highest memory-pressure level already alerted this episode; reset
    /// to 0 when the episode ends. One FIRST alert per severity —
    /// worsening escalates immediately, and lingering is handled by the
    /// backing-off reminders rather than by re-crossing
    pressure_alerted: u32,
    /// When this episode last notified, and how long until it may again.
    /// The gap starts at [`PERSIST_REMINDER`], doubles per reminder up
    /// to [`PRESSURE_REMINDER_MAX`], and resets on escalation
    pressure_last_alert: Option<Instant>,
    pressure_reminder_gap: Duration,
    /// When this episode FIRST notified, so a reminder can report how
    /// long it has been going since the user was first told — the same
    /// meaning `repeat_after` carries for every other rule
    pressure_first_alert: Option<Instant>,
    /// When the last metrics-recording pass ran (`None` = record on the
    /// next evaluation)
    last_record: Option<Instant>,
}

impl AlertEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The memory-pressure episode is over: the next above-normal stretch
    /// starts a fresh one, serving its own persistence requirement and
    /// its own backoff from scratch
    fn clear_pressure(&mut self) {
        self.pressure_since = None;
        self.pressure_normal_since = None;
        self.pressure_alerted = 0;
        self.pressure_last_alert = None;
        self.pressure_first_alert = None;
        self.pressure_reminder_gap = Duration::ZERO;
    }

    /// Evaluate one snapshot observed at `now` (injectable for tests).
    ///
    /// Two tiers per CPU rule, sharing one cooldown per (pid, kind) so
    /// the same situation never notifies twice:
    /// - acute: 1-minute average ≥ [`ACUTE_FACTOR`] × the effective
    ///   threshold — a runaway, worth interrupting fast;
    /// - chronic: 5-minute average ≥ the effective threshold — the
    ///   "quietly always busy" case, interrupted only once persistence
    ///   is established (self-limiting bursts never get that far).
    ///
    /// Memory has no acute tier by design: transient legitimate spikes
    /// are exactly what must not alert. Recording stays on the 1-minute
    /// window with BASE thresholds — an override or the template
    /// suppresses the notification, not the data point — plus the
    /// window's [`RECORD_TOP_CPU_TIME`] biggest CPU-time spenders
    /// regardless of any threshold, which is what keeps a permanently
    /// low-but-nonzero process from being invisible
    pub fn evaluate(
        &mut self,
        now: Instant,
        snapshot: &SystemSnapshot,
        active: &ActiveThresholds,
    ) -> Evaluation {
        let mut evaluation = Evaluation::default();
        let total_memory = snapshot.memory.total_bytes;
        let mem_share = |avg_bytes: f64| {
            if total_memory > 0 {
                avg_bytes / total_memory as f64
            } else {
                0.0
            }
        };

        // Disk capacity runs first: it does not depend on process data.
        // Crossing detection with hysteresis — see DISK_REARM_MARGIN
        if let Some(disks) = &snapshot.disks {
            self.disk_disarmed
                .retain(|mount| disks.iter().any(|d| d.mount_point == *mount));
            for d in disks {
                let Some(threshold) = active.disk.for_process(&d.mount_point) else {
                    continue;
                };
                if d.total_bytes == 0 {
                    continue;
                }
                let used =
                    d.total_bytes.saturating_sub(d.available_bytes) as f64 / d.total_bytes as f64;
                if self.disk_disarmed.contains(&d.mount_point) {
                    if used < f64::from(threshold) - DISK_REARM_MARGIN {
                        self.disk_disarmed.remove(&d.mount_point);
                    }
                } else if used >= f64::from(threshold) {
                    self.disk_disarmed.insert(d.mount_point.clone());
                    evaluation.events.push(AlertEvent {
                        subject: AlertSubject::Volume {
                            mount_point: d.mount_point.clone(),
                        },
                        detail: AlertDetail::Disk {
                            used_percent: used * 100.0,
                            threshold_percent: f64::from(threshold) * 100.0,
                            available_bytes: d.available_bytes,
                            total_bytes: d.total_bytes,
                        },
                        repeat_after: None,
                    });
                }
            }
        }

        // System memory pressure: the kernel's own verdict, but only once
        // it PERSISTS. A memory-heavy machine sits at warning as its
        // normal state and crosses the line all day, and a build spike is
        // over in a minute — neither is actionable, and plain crossing
        // detection reports both. compressed_bytes deliberately has no
        // rule of its own: the kernel already folds compressor growth
        // into this level
        if let Some(threshold) = active.pressure
            && let Some(level) = snapshot.memory.pressure_level
        {
            if level <= 1 {
                // A dip to normal does not end the episode — see
                // PRESSURE_REARM. Until it has held, everything below
                // (including the reminder clock) stays exactly as it was
                let normal_since = *self.pressure_normal_since.get_or_insert(now);
                if now.duration_since(normal_since) >= PRESSURE_REARM {
                    self.clear_pressure();
                }
            } else {
                self.pressure_normal_since = None;
                let since = *self.pressure_since.get_or_insert(now);
                let elapsed = now.duration_since(since);
                // More severe means faster notification, mirroring the
                // acute/chronic split of the per-process rules
                let required = if level >= 4 { WINDOW } else { SLOW_WINDOW };
                let notify =
                    if level >= threshold && level > self.pressure_alerted && elapsed >= required {
                        // First alert at this severity. Worsening does not
                        // wait for the reminder clock — persistence is
                        // already established — and it restarts the backoff,
                        // because a new severity is genuinely new news
                        self.pressure_alerted = level;
                        self.pressure_reminder_gap = PERSIST_REMINDER;
                        self.pressure_first_alert.get_or_insert(now);
                        Some(None)
                    } else if level >= threshold
                        && self.pressure_alerted > 0
                        && self.pressure_last_alert.is_some_and(|last| {
                            now.duration_since(last) >= self.pressure_reminder_gap
                        })
                    {
                        // Still going. Reminders back off so a condition
                        // that holds all day stays a pulse, not a nag
                        self.pressure_reminder_gap =
                            (self.pressure_reminder_gap * 2).min(PRESSURE_REMINDER_MAX);
                        Some(self.pressure_first_alert.map(|at| now.duration_since(at)))
                    } else {
                        None
                    };
                if let Some(repeat_after) = notify {
                    self.pressure_last_alert = Some(now);
                    evaluation.events.push(AlertEvent {
                        subject: AlertSubject::System,
                        detail: AlertDetail::Pressure {
                            level,
                            sustained: elapsed,
                            swap_used_bytes: snapshot.memory.swap_used_bytes,
                            swap_total_bytes: snapshot.memory.swap_total_bytes,
                            compressed_bytes: snapshot.memory.compressed_bytes,
                        },
                        repeat_after,
                    });
                }
            }
        }

        // Whole-application totals: a browser split across 37 helpers can
        // hold gigabytes and cores while every member stays under the
        // per-process bar. Chronic only — a runaway member is already the
        // per-process acute rule's job, and adding an acute tier here
        // would just notify twice about one event
        if let Some(groups) = snapshot.process_groups.as_deref()
            && (active.app_cpu.any_enabled() || active.app_memory.any_enabled())
        {
            // ProcessWindows keys on pid; a group's root pid plays that
            // role, so the same rolling-average machinery applies
            let as_processes: Vec<crate::snapshot::ProcessSnapshot> = groups
                .iter()
                .map(|g| crate::snapshot::ProcessSnapshot {
                    pid: g.root_pid,
                    name: g.name.clone(),
                    cmd: String::new(),
                    cpu_usage_percent: g.cpu_usage_percent,
                    // Groups carry no CPU-time counter on purpose: their
                    // membership churns, so a summed counter would drop
                    // whenever a helper exits and the "diff two samples"
                    // contract would break. The app rules only use the
                    // averages anyway
                    cpu_time_ms: 0,
                    memory_bytes: g.memory_bytes,
                    // Not summed over the tree: EPERM'd members would make
                    // the total silently partial. The app rules don't read
                    // it; per-process rows carry the real value.
                    phys_footprint_bytes: None,
                    virtual_memory_bytes: 0,
                    run_time_secs: 0,
                    parent_pid: None,
                    user_id: None,
                    status: String::new(),
                    read_bytes_per_sec: None,
                    write_bytes_per_sec: None,
                })
                .collect();
            let stats = self
                .apps
                .get_or_insert_with(|| ProcessWindows::new(SLOW_WINDOW))
                .record(now, &as_processes);
            self.episodes.retain(|(pid, kind)| {
                !matches!(kind, AlertKind::AppCpu | AlertKind::AppMemory) || stats.contains_key(pid)
            });

            for g in groups {
                let Some(stat) = stats.get(&g.root_pid) else {
                    continue;
                };
                if stat.span < SLOW_MIN_SPAN {
                    continue;
                }
                if let Some(threshold) = active.app_cpu.for_process(&g.name) {
                    let key = (g.root_pid, AlertKind::AppCpu);
                    if stat.cpu_avg < f64::from(threshold) {
                        self.episodes.clear(key);
                    } else if let Some(notify) = self.episodes.notify(key, now, active.cooldown) {
                        evaluation.events.push(AlertEvent {
                            subject: AlertSubject::App {
                                root_pid: g.root_pid,
                                name: g.name.clone(),
                                process_count: g.process_count,
                            },
                            detail: AlertDetail::Cpu {
                                avg_percent: stat.cpu_avg,
                                threshold_percent: f64::from(threshold),
                                window: SLOW_WINDOW,
                                runaway: false,
                            },
                            repeat_after: notify.elapsed(),
                        });
                    }
                }
                if let Some(fraction) = active.app_memory.for_process(&g.name)
                    && total_memory > 0
                {
                    let key = (g.root_pid, AlertKind::AppMemory);
                    let share = mem_share(stat.memory_avg_bytes);
                    if share < fraction {
                        self.episodes.clear(key);
                    } else if let Some(notify) = self.episodes.notify(key, now, active.cooldown) {
                        evaluation.events.push(AlertEvent {
                            subject: AlertSubject::App {
                                root_pid: g.root_pid,
                                name: g.name.clone(),
                                process_count: g.process_count,
                            },
                            detail: AlertDetail::Memory {
                                avg_bytes: stat.memory_avg_bytes as u64,
                                share_percent: share * 100.0,
                                threshold_percent: fraction * 100.0,
                                window: SLOW_WINDOW,
                            },
                            repeat_after: notify.elapsed(),
                        });
                    }
                }
            }
        }

        let Some(processes) = snapshot.processes.as_deref() else {
            tracing::debug!("alert evaluation skipped: snapshot has no processes");
            return evaluation;
        };

        // Once a minute, qualifying processes become metric records
        let record_due = self
            .last_record
            .is_none_or(|t| now.duration_since(t) >= RECORD_EVERY);
        if record_due {
            self.last_record = Some(now);
        }

        let fast_stats = self
            .fast
            .get_or_insert_with(|| ProcessWindows::new(WINDOW))
            .record(now, processes);
        let slow_stats = self
            .slow
            .get_or_insert_with(|| ProcessWindows::new(SLOW_WINDOW))
            .record(now, processes);

        // The biggest CPU-time spenders of this window get recorded no
        // matter what their percentages look like — see RECORD_TOP_CPU_TIME
        let top_cpu_time: std::collections::HashSet<u32> = if record_due {
            let mut ranked: Vec<(u32, u64)> = fast_stats
                .iter()
                .filter(|(_, s)| s.span >= MIN_SPAN && s.cpu_time_delta_ms > 0)
                .map(|(pid, s)| (*pid, s.cpu_time_delta_ms))
                .collect();
            // Ties broken by pid so a run is reproducible
            ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.truncate(RECORD_TOP_CPU_TIME);
            ranked.into_iter().map(|(pid, _)| pid).collect()
        } else {
            std::collections::HashSet::new()
        };

        // Forget cooldowns of processes that disappeared — but only the
        // per-process ones: an app group is keyed by its ROOT pid, which
        // need not appear in the top-N process list at all, and wiping
        // those entries here would re-fire every group alert every round
        self.episodes.retain(|(pid, kind)| {
            matches!(kind, AlertKind::AppCpu | AlertKind::AppMemory) || fast_stats.contains_key(pid)
        });

        for p in processes {
            let (Some(fast), Some(slow)) = (fast_stats.get(&p.pid), slow_stats.get(&p.pid)) else {
                continue;
            };
            if p.cpu_usage_percent > 20.0 {
                tracing::debug!(
                    pid = p.pid,
                    name = %p.name,
                    cpu = p.cpu_usage_percent,
                    samples = fast.samples,
                    span_secs = fast.span.as_secs(),
                    slow_span_secs = slow.span.as_secs(),
                    "alert window state"
                );
            }

            if let Some(threshold) = active.cpu.for_process(&p.name) {
                let acute =
                    fast.span >= MIN_SPAN && fast.cpu_avg >= ACUTE_FACTOR * f64::from(threshold);
                let chronic = slow.span >= SLOW_MIN_SPAN && slow.cpu_avg >= f64::from(threshold);
                let key = (p.pid, AlertKind::Cpu);
                if !(acute || chronic) {
                    // Back under the bar: this episode is over
                    self.episodes.clear(key);
                } else if let Some(notify) = self.episodes.notify(key, now, active.cooldown) {
                    evaluation.events.push(AlertEvent {
                        subject: AlertSubject::Process {
                            pid: p.pid,
                            name: p.name.clone(),
                        },
                        // The tier that fired decides which window and
                        // which bar the numbers refer to
                        detail: AlertDetail::Cpu {
                            avg_percent: if acute { fast.cpu_avg } else { slow.cpu_avg },
                            threshold_percent: if acute {
                                ACUTE_FACTOR * f64::from(threshold)
                            } else {
                                f64::from(threshold)
                            },
                            window: if acute { WINDOW } else { SLOW_WINDOW },
                            runaway: acute,
                        },
                        repeat_after: notify.elapsed(),
                    });
                }
            }

            if let Some(fraction) = active.memory.for_process(&p.name)
                && total_memory > 0
                && slow.span >= SLOW_MIN_SPAN
            {
                let key = (p.pid, AlertKind::Memory);
                if mem_share(slow.memory_avg_bytes) < fraction {
                    self.episodes.clear(key);
                } else if let Some(notify) = self.episodes.notify(key, now, active.cooldown) {
                    evaluation.events.push(AlertEvent {
                        subject: AlertSubject::Process {
                            pid: p.pid,
                            name: p.name.clone(),
                        },
                        detail: AlertDetail::Memory {
                            avg_bytes: slow.memory_avg_bytes as u64,
                            share_percent: mem_share(slow.memory_avg_bytes) * 100.0,
                            threshold_percent: fraction * 100.0,
                            window: SLOW_WINDOW,
                        },
                        repeat_after: notify.elapsed(),
                    });
                }
            }

            // Metrics recording: 1-minute window, BASE thresholds only,
            // plus the window's top CPU-time spenders unconditionally
            if record_due && fast.span >= MIN_SPAN {
                let over_cpu = active
                    .cpu
                    .base()
                    .is_some_and(|threshold| fast.cpu_avg >= f64::from(threshold));
                let over_mem = active.memory.base().is_some_and(|fraction| {
                    total_memory > 0 && mem_share(fast.memory_avg_bytes) >= fraction
                });
                if over_cpu || over_mem || top_cpu_time.contains(&p.pid) {
                    evaluation.records.push(MetricRecord {
                        timestamp: snapshot.timestamp,
                        pid: p.pid,
                        name: p.name.clone(),
                        cpu_avg_percent: fast.cpu_avg as f32,
                        memory_avg_bytes: fast.memory_avg_bytes as u64,
                        memory_share_percent: (mem_share(fast.memory_avg_bytes) * 100.0) as f32,
                        // The raw counter, not the window's delta: the
                        // selection above is a per-minute ranking, but
                        // the FILE has to stay exact for a pid that only
                        // qualifies on some minutes — see MetricRecord
                        cpu_time_ms: p.cpu_time_ms,
                    });
                }
            }
        }
        evaluation
    }
}

/// One ongoing alerted condition
#[derive(Debug, Clone, Copy)]
struct Episode {
    started: Instant,
    /// Whether the single [`PERSIST_REMINDER`] follow-up already went out
    reminded: bool,
}

/// Why a notification is due
#[derive(Debug, Clone, Copy, PartialEq)]
enum Notify {
    /// The condition just crossed its threshold
    New,
    /// Still true after [`PERSIST_REMINDER`], carrying how long it has
    /// been going
    Reminder(Duration),
}

/// Per-(process, rule) alert bookkeeping: which conditions are currently
/// inside an alerted episode, and when each last notified
#[derive(Default)]
struct EpisodeState {
    /// Conditions already notified and not yet cleared
    active: HashMap<(u32, AlertKind), Episode>,
    last_alert: HashMap<(u32, AlertKind), Instant>,
}

impl EpisodeState {
    /// Whether this condition should notify now, and why.
    ///
    /// A crossing notifies once. Repeating that every cooldown adds
    /// nothing — you either acted or decided not to — but going silent
    /// forever is its own failure mode, because a single notification
    /// can be missed (macOS defers them behind Focus modes and
    /// summaries). So exactly one follow-up goes out if the condition
    /// is still true after [`PERSIST_REMINDER`], and then it is quiet
    /// until the condition actually clears.
    fn notify(
        &mut self,
        key: (u32, AlertKind),
        now: Instant,
        cooldown: Duration,
    ) -> Option<Notify> {
        let decision = match self.active.get_mut(&key) {
            Some(episode) => {
                let elapsed = now.duration_since(episode.started);
                if episode.reminded || elapsed < PERSIST_REMINDER {
                    None
                } else {
                    episode.reminded = true;
                    Some(Notify::Reminder(elapsed))
                }
            }
            // Floor between episodes, so a value hovering at the
            // threshold cannot alert on every crossing
            None => {
                let too_soon = self
                    .last_alert
                    .get(&key)
                    .is_some_and(|previous| now.duration_since(*previous) < cooldown);
                (!too_soon).then_some(Notify::New)
            }
        };

        if decision.is_some() {
            self.last_alert.insert(key, now);
        }
        if decision == Some(Notify::New) {
            self.active.insert(
                key,
                Episode {
                    started: now,
                    reminded: false,
                },
            );
        }
        decision
    }

    /// The condition no longer holds: the episode is over, so the next
    /// crossing may notify again
    fn clear(&mut self, key: (u32, AlertKind)) {
        self.active.remove(&key);
    }

    /// Forget everything about keys that no longer exist
    fn retain(&mut self, keep: impl Fn(&(u32, AlertKind)) -> bool) {
        self.active.retain(|k, _| keep(k));
        self.last_alert.retain(|k, _| keep(k));
    }
}

impl Notify {
    /// How long the condition had been going, for a follow-up; None for
    /// a freshly crossed one
    fn elapsed(self) -> Option<Duration> {
        match self {
            Self::New => None,
            Self::Reminder(elapsed) => Some(elapsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{CpuSnapshot, HostInfo, LoadSnapshot, MemorySnapshot, ProcessSnapshot};

    /// A process with no CPU-time counter, so it can never qualify for
    /// the top-spender recording — every threshold test stays about
    /// thresholds alone
    fn proc(pid: u32, cpu: f32, mem: u64) -> ProcessSnapshot {
        proc_burning(pid, cpu, mem, 0)
    }

    fn proc_burning(pid: u32, cpu: f32, mem: u64, cpu_time_ms: u64) -> ProcessSnapshot {
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

    fn snapshot(processes: Vec<ProcessSnapshot>, total_memory: u64) -> SystemSnapshot {
        SystemSnapshot {
            timestamp: jiff::Timestamp::from_second(0).expect("valid"),
            host: HostInfo {
                hostname: String::new(),
                os_name: String::new(),
                os_version: String::new(),
                kernel_version: None,
                arch: String::new(),
                uptime_secs: 0,
                labels: Default::default(),
            },
            cpu: CpuSnapshot {
                usage_percent: 0.0,
                per_core_usage: Vec::new(),
                logical_cores: 0,
                physical_cores: None,
                frequency_mhz: None,
                per_core_frequency_mhz: Vec::new(),
                brand: None,
                perf_levels: None,
            },
            memory: MemorySnapshot {
                total_bytes: total_memory,
                used_bytes: 0,
                available_bytes: 0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                used_percent: 0.0,
                swap_used_percent: 0.0,
                compressed_bytes: None,
                pressure_level: None,
            },
            disks: None,
            networks: None,
            processes: Some(std::sync::Arc::new(processes)),
            process_groups: None,
            total_processes: None,
            battery: None,
            load: LoadSnapshot {
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            temperatures: None,
            io_totals: Default::default(),
            extras: Default::default(),
        }
    }

    fn thresholds(
        cpu: Thresholds<f32>,
        memory: Thresholds<f64>,
        cooldown: Duration,
    ) -> ActiveThresholds {
        ActiveThresholds {
            cpu,
            memory,
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: None,
            cooldown,
        }
    }

    fn disk(mount: &str, total: u64, available: u64) -> crate::snapshot::DiskSnapshot {
        let used = total.saturating_sub(available);
        let used_percent = if total == 0 {
            0.0
        } else {
            used as f32 / total as f32 * 100.0
        };
        crate::snapshot::DiskSnapshot {
            name: "disk0".into(),
            mount_point: mount.into(),
            file_system: "apfs".into(),
            kind: "SSD".into(),
            is_removable: false,
            total_bytes: total,
            available_bytes: available,
            used_percent,
            read_bytes_per_sec: None,
            write_bytes_per_sec: None,
        }
    }

    fn snapshot_disks(disks: Vec<crate::snapshot::DiskSnapshot>) -> SystemSnapshot {
        let mut s = snapshot(Vec::new(), 100);
        s.disks = Some(disks);
        s
    }

    /// Feed one sample every 2s for `steps` steps starting at
    /// `base + offset`, returning how many alerts fired
    fn drive(
        engine: &mut AlertEngine,
        active: &ActiveThresholds,
        base: Instant,
        offset: Duration,
        steps: u64,
        make_proc: impl Fn(u64) -> ProcessSnapshot,
        total_memory: u64,
    ) -> usize {
        let mut fired = 0;
        for i in 0..steps {
            let now = base + offset + Duration::from_secs(2 * i);
            fired += engine
                .evaluate(now, &snapshot(vec![make_proc(i)], total_memory), active)
                .events
                .len();
        }
        fired
    }

    #[test]
    fn chronic_cpu_needs_full_slow_window_then_respects_cooldown() {
        // 150% is below the acute bar (3 x 100 = 300), so only the chronic
        // rule applies: nothing before the 5-minute window is ~full, then
        // one alert, then the cooldown suppresses re-fires
        let active = thresholds(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;

        for i in 0..160u64 {
            let now = base + Duration::from_secs(2 * i);
            fired += engine
                .evaluate(now, &snapshot(vec![proc(1, 150.0, 0)], 100), &active)
                .events
                .len();
            if 2 * i < 250 {
                assert_eq!(fired, 0, "no chronic alert before t=250s (t={}s)", 2 * i);
            }
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn acute_runaway_fires_within_a_minute() {
        // 150% >= 3 x 30: the acute tier fires as soon as the 1-minute
        // window is ~full, long before the chronic window could
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let events = engine
                .evaluate(now, &snapshot(vec![proc(1, 150.0, 0)], 100), &active)
                .events;
            if 2 * i < 50 {
                assert!(events.is_empty(), "no alert before t=50s (t={}s)", 2 * i);
            }
            messages.extend(events.into_iter().map(|e| e.summary()));
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("runaway"), "got: {}", messages[0]);
    }

    #[test]
    fn acute_scales_with_the_effective_override() {
        // ghostty's override (100) raises its acute bar to 300: the same
        // 150% that is a runaway for a default process stays silent for it
        let cpu = Thresholds::new(Some(30.0)).with_override("ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 150.0, 0);
            ghostty.name = "ghostty".into();
            let other = proc(2, 150.0, 0);
            messages.extend(
                engine
                    .evaluate(now, &snapshot(vec![ghostty, other], 100), &active)
                    .events
                    .into_iter()
                    .map(|e| e.summary()),
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("p2 (pid 2)"), "got: {}", messages[0]);
    }

    #[test]
    fn acute_and_chronic_share_one_cooldown() {
        // A runaway alerts acutely at ~50s; the chronic rule matching the
        // same process at ~250s must NOT re-notify within the cooldown
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 150.0, 0),
            100,
        );
        assert_eq!(fired, 1);
    }

    #[test]
    fn cpu_below_threshold_never_fires() {
        let active = thresholds(
            Thresholds::new(Some(100.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 60.0, 0),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn sustained_memory_fires_once_after_slow_window_fills() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        // 30% of total for 320s: one alert (memory is chronic-only, after
        // the 250s span), then cooldown
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 0.0, 30),
            100,
        );
        assert_eq!(fired, 1);
    }

    #[test]
    fn transient_memory_spike_does_not_alert() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        // A 10s spike at 60% averages away inside the 5-minute window
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |i| proc(1, 0.0, if i < 5 { 60 } else { 5 }),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn per_process_override_beats_default() {
        let cpu = Thresholds::new(Some(30.0)).with_override("Ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();

        // Two processes both averaging 60%: the default-threshold one fires
        // (chronic, 60 >= 30), the overridden one stays quiet (60 < 100);
        // name match ignores case
        let mut messages = Vec::new();
        for i in 0..160u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 60.0, 0);
            ghostty.name = "ghostty".into();
            let other = proc(2, 60.0, 0);
            for event in engine
                .evaluate(now, &snapshot(vec![ghostty, other], 100), &active)
                .events
            {
                messages.push(event.summary());
            }
        }
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("p2 (pid 2)"), "got: {}", messages[0]);
    }

    #[test]
    fn zero_override_disables_rule_for_that_process() {
        let cpu = Thresholds::new(Some(30.0)).with_override("p1".into(), None);
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            160,
            |_| proc(1, 150.0, 0),
            100,
        );
        assert_eq!(fired, 0);
    }

    #[test]
    fn a_persisting_condition_notifies_once_not_every_cooldown() {
        // The behaviour a real machine exposed: a system daemon pegged
        // for an hour used to produce one notification per cooldown.
        // Being told once is the point; repeats add nothing
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(20),
        );
        let mut engine = AlertEngine::new();
        let fired = drive(
            &mut engine,
            &active,
            Instant::now(),
            Duration::ZERO,
            400,
            |_| proc(1, 0.0, 30),
            100,
        );
        assert_eq!(fired, 1, "800s of sustained overload is still one event");
    }

    #[test]
    fn events_carry_structure_a_gui_can_render_without_parsing_text() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(Some(30.0)),
            memory: Thresholds::new(None),
            disk: Thresholds::new(Some(0.90)),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut events = Vec::new();

        for i in 0..40u64 {
            let mut s = snapshot(vec![proc(1, 150.0, 0)], 100);
            s.disks = Some(vec![disk("/", 1000, 50)]);
            events.extend(
                engine
                    .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                    .events,
            );
        }

        let cpu = events
            .iter()
            .find(|e| e.kind() == AlertKind::Cpu)
            .expect("cpu event");
        // 150% is 3x the 30% threshold: the runaway tier
        assert_eq!(cpu.severity(), Severity::Critical);
        assert_eq!(
            cpu.subject,
            AlertSubject::Process {
                pid: 1,
                name: "p1".into()
            }
        );
        let AlertDetail::Cpu {
            avg_percent,
            threshold_percent,
            window,
            runaway,
        } = cpu.detail
        else {
            panic!("expected a CPU measurement, got {:?}", cpu.detail);
        };
        assert!((avg_percent - 150.0).abs() < 0.5);
        // The runaway bar is the effective threshold, already multiplied
        assert_eq!(threshold_percent, 90.0);
        assert_eq!(window, WINDOW);
        assert!(runaway);
        assert!(cpu.repeat_after.is_none());

        let disk_event = events
            .iter()
            .find(|e| e.kind() == AlertKind::Disk)
            .expect("disk event");
        assert_eq!(disk_event.severity(), Severity::Warning);
        assert_eq!(
            disk_event.subject,
            AlertSubject::Volume {
                mount_point: "/".into()
            }
        );
        let AlertDetail::Disk { used_percent, .. } = disk_event.detail else {
            panic!("expected a disk measurement");
        };
        assert!((used_percent - 95.0).abs() < 0.5);

        // The default rendering is derived, not stored
        assert!(
            cpu.summary()
                .contains("p1 (pid 1) runaway: averaged 150% CPU"),
            "got: {}",
            cpu.summary()
        );
        assert_eq!(cpu.summary(), cpu.to_string());

        // The whole event round-trips as JSON, so it can cross an IPC
        // boundary or be logged structurally — and carries no prose
        let json = serde_json::to_string(cpu).expect("serialize");
        assert!(json.contains("\"measure\":\"cpu\""), "got: {json}");
        assert!(json.contains("\"type\":\"process\""), "got: {json}");
        assert!(!json.contains("runaway:"), "no baked wording: {json}");
    }

    #[test]
    fn a_long_episode_earns_exactly_one_follow_up() {
        // Going silent forever is its own failure mode: a single
        // notification can be missed. One reminder at 30 minutes, then
        // quiet again however long it lasts
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(20),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        // 2 hours of unbroken overload at a 2s cadence
        for i in 0..3600u64 {
            messages.extend(
                engine
                    .evaluate(
                        base + Duration::from_secs(2 * i),
                        &snapshot(vec![proc(1, 0.0, 30)], 100),
                        &active,
                    )
                    .events
                    .into_iter()
                    .map(|e| e.summary()),
            );
        }
        assert_eq!(messages.len(), 2, "one crossing plus one follow-up");
        assert!(!messages[0].contains("still going"));
        assert!(
            messages[1].contains("still going after 30 min"),
            "got: {}",
            messages[1]
        );
    }

    #[test]
    fn a_new_episode_notifies_again_once_the_condition_cleared() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(20),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();

        // Over the bar long enough to alert once
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::ZERO,
                160,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
        // Drops back under for a while: the episode ends (silently), and
        // the slow window refills with the low value
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::from_secs(320),
                160,
                |_| proc(1, 0.0, 5),
                100
            ),
            0
        );
        // Rising again is a NEW event and notifies
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::from_secs(640),
                160,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
    }

    #[test]
    fn cooldown_still_floors_flapping_between_episodes() {
        // Straddling the threshold must not alert on every crossing:
        // the cooldown is the floor between episodes
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;
        // 800s alternating high/low every 60s (30 samples), so the
        // 5-minute average crosses repeatedly
        for i in 0..400u64 {
            let mem = if (i / 30) % 2 == 0 { 40 } else { 0 };
            fired += engine
                .evaluate(
                    base + Duration::from_secs(2 * i),
                    &snapshot(vec![proc(1, 0.0, mem)], 100),
                    &active,
                )
                .events
                .len();
        }
        assert!(fired <= 2, "cooldown must bound flapping, got {fired}");
    }

    #[test]
    fn dead_process_resets_state() {
        let active = thresholds(
            Thresholds::new(None),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        // Sustained hog fires once (chronic, at ~250s)
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::ZERO,
                130,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
        // pid 1 disappears for one round: its history and cooldown reset
        let _ = engine.evaluate(
            base + Duration::from_secs(260),
            &snapshot(vec![proc(2, 0.0, 1)], 100),
            &active,
        );
        // The same pid reappears as a hog: refills its slow window, then
        // fires again well within the original cooldown span
        assert_eq!(
            drive(
                &mut engine,
                &active,
                base,
                Duration::from_secs(262),
                130,
                |_| proc(1, 0.0, 30),
                100
            ),
            1
        );
    }

    #[test]
    fn records_qualifying_process_once_per_minute() {
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(None),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut records = Vec::new();

        // 80s at a 2s cadence: the t=0 recording pass has an empty window,
        // the t=60s pass has a full one — exactly one record
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            records.extend(
                engine
                    .evaluate(now, &snapshot(vec![proc(1, 150.0, 40)], 100), &active)
                    .records,
            );
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 1);
        assert_eq!(records[0].name, "p1");
        assert!((records[0].cpu_avg_percent - 150.0).abs() < 0.5);
        assert!((records[0].memory_share_percent - 40.0).abs() < 0.5);
    }

    #[test]
    fn recording_uses_base_threshold_despite_override() {
        // Override silences ghostty's notification at 60% (< 100), but the
        // base threshold (30) still records it
        let cpu = Thresholds::new(Some(30.0)).with_override("ghostty".into(), Some(100.0));
        let active = thresholds(cpu, Thresholds::new(None), Duration::from_secs(600));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut records = Vec::new();
        let mut events = Vec::new();

        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let mut ghostty = proc(1, 60.0, 0);
            ghostty.name = "ghostty".into();
            let evaluation = engine.evaluate(now, &snapshot(vec![ghostty], 100), &active);
            events.extend(evaluation.events);
            records.extend(evaluation.records);
        }
        assert!(events.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "ghostty");
    }

    /// The blind spot this criterion exists to close: a process that
    /// never approaches a threshold, but quietly outspends everything
    /// that does. No rule here can catch it — and nothing should
    /// interrupt over it — but the data has to survive so the question
    /// can be asked a day later
    #[test]
    fn steady_low_cpu_is_recorded_even_though_no_rule_can_fire() {
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut records = Vec::new();
        let mut events = Vec::new();

        // A rock-steady 10% (200 core-ms per 2s tick) on a process that
        // has already been running a while — the counter is absolute
        const ALREADY_BURNED: u64 = 1_000_000;
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let p = proc_burning(1, 10.0, 5, ALREADY_BURNED + 200 * i);
            let evaluation = engine.evaluate(now, &snapshot(vec![p], 100), &active);
            events.extend(evaluation.events);
            records.extend(evaluation.records);
        }

        assert!(events.is_empty(), "10% must never interrupt anyone");
        assert_eq!(records.len(), 1, "one record per minute");
        assert!((records[0].cpu_avg_percent - 10.0).abs() < 0.5);
        // The lifetime counter, not the window's 6 core-seconds: only an
        // absolute value stays exact for a pid recorded on some minutes
        // and not others
        assert_eq!(records[0].cpu_time_ms, ALREADY_BURNED + 200 * 30);
    }

    #[test]
    fn below_base_thresholds_records_nothing() {
        let active = thresholds(
            Thresholds::new(Some(30.0)),
            Thresholds::new(Some(0.25)),
            Duration::from_secs(600),
        );
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        for i in 0..40u64 {
            let now = base + Duration::from_secs(2 * i);
            let evaluation = engine.evaluate(now, &snapshot(vec![proc(1, 10.0, 5)], 100), &active);
            assert!(evaluation.records.is_empty());
        }
    }

    #[test]
    fn disk_crossing_alerts_once_then_rearms_below_margin() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(Some(0.90)),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut step = |i: u64, used_pct: u64| {
            engine
                .evaluate(
                    base + Duration::from_secs(2 * i),
                    &snapshot_disks(vec![disk("/", 1000, 1000 - used_pct * 10)]),
                    &active,
                )
                .events
        };

        assert!(step(0, 85).is_empty(), "below threshold");
        let fired = step(1, 91);
        assert_eq!(fired.len(), 1, "crossing fires once");
        assert!(fired[0].summary().contains("disk / is 91% full"));
        assert!(step(2, 93).is_empty(), "still full: no nagging");
        assert!(step(3, 87).is_empty(), "above the re-arm line: still quiet");
        assert!(step(4, 84).is_empty(), "re-armed silently");
        assert_eq!(step(5, 92).len(), 1, "second crossing fires again");
    }

    #[test]
    fn disk_override_disables_backup_volume() {
        // A Time-Machine-style volume runs full by design: override 0
        // silences it while the system volume still alerts
        let disk_rule = Thresholds::new(Some(0.90)).with_override("/Volumes/Backup".into(), None);
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: disk_rule,
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let events = engine
            .evaluate(
                Instant::now(),
                &snapshot_disks(vec![disk("/Volumes/Backup", 1000, 10), disk("/", 1000, 50)]),
                &active,
            )
            .events;
        assert_eq!(events.len(), 1);
        assert!(events[0].summary().contains("disk / is 95% full"));
    }

    use crate::snapshot::ProcessGroupSnapshot;

    fn group(root_pid: u32, name: &str, count: u32, cpu: f32, mem: u64) -> ProcessGroupSnapshot {
        ProcessGroupSnapshot {
            root_pid,
            name: name.into(),
            process_count: count,
            cpu_usage_percent: cpu,
            memory_bytes: mem,
            read_bytes_per_sec: None,
            write_bytes_per_sec: None,
        }
    }

    fn app_thresholds(cpu: Thresholds<f32>, memory: Thresholds<f64>) -> ActiveThresholds {
        ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: cpu,
            app_memory: memory,
            pressure: None,
            cooldown: Duration::from_secs(600),
        }
    }

    #[test]
    fn app_group_alerts_on_the_total_no_member_reaches() {
        // Chrome-shaped: 12 helpers at 20% each is 240% for the app, and
        // not one of them would trip a per-process rule
        let active = app_thresholds(Thresholds::new(Some(200.0)), Thresholds::new(None));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..160u64 {
            let mut s = snapshot(Vec::new(), 100);
            s.process_groups = Some(std::sync::Arc::new(vec![group(
                10,
                "Google Chrome",
                12,
                240.0,
                0,
            )]));
            messages.extend(
                engine
                    .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                    .events
                    .into_iter()
                    .map(|e| e.summary()),
            );
        }
        assert_eq!(messages.len(), 1, "chronic app rule fires once");
        assert!(
            messages[0].contains("Google Chrome (12 processes) averaged 240% CPU"),
            "got: {}",
            messages[0]
        );
    }

    #[test]
    fn app_group_rules_respect_overrides_and_window() {
        let cpu = Thresholds::new(Some(200.0)).with_override("Google Chrome".into(), None);
        let active = app_thresholds(cpu, Thresholds::new(Some(0.40)));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut messages = Vec::new();

        for i in 0..160u64 {
            let mut s = snapshot(Vec::new(), 100);
            s.process_groups = Some(std::sync::Arc::new(vec![
                // CPU rule disabled for this app; memory rule still applies
                group(10, "Google Chrome", 12, 240.0, 50),
                // Under both thresholds
                group(20, "Finder", 1, 10.0, 5),
            ]));
            let events = engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events;
            if 2 * i < 250 {
                assert!(
                    events.is_empty(),
                    "not before the slow window (t={}s)",
                    2 * i
                );
            }
            messages.extend(events.into_iter().map(|e| e.summary()));
        }
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].contains("50% of total memory"),
            "got: {}",
            messages[0]
        );
    }

    #[test]
    fn app_group_rules_are_skipped_without_group_data() {
        let active = app_thresholds(Thresholds::new(Some(1.0)), Thresholds::new(Some(0.01)));
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        for i in 0..160u64 {
            // process_groups is None (collection disabled)
            let s = snapshot(Vec::new(), 100);
            assert!(
                engine
                    .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                    .events
                    .is_empty()
            );
        }
    }

    #[test]
    fn pressure_alerts_on_warning_escalates_to_critical_then_rearms() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut step = |i: u64, level: u32| {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(level);
            engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events
        };

        // Steps are 2s apart and the episode starts at step 1 (t=2s), so
        // the warning's SLOW_WINDOW (300s) is served at step 151
        assert!(step(0, 1).is_empty(), "normal is silent");
        for i in 1..151 {
            assert!(
                step(i, 2).is_empty(),
                "transient warning must stay silent (t={}s)",
                2 * i
            );
        }
        let warn = step(151, 2);
        assert_eq!(warn.len(), 1, "sustained warning alerts once");
        assert!(
            warn[0].summary().contains("warning for 5 min"),
            "got: {}",
            warn[0].summary()
        );
        assert!(step(152, 2).is_empty(), "lingering warning does not nag");
        let crit = step(153, 4);
        assert_eq!(crit.len(), 1, "worsening escalates without waiting again");
        assert!(crit[0].summary().contains("critical"));
        assert!(
            step(154, 2).is_empty(),
            "improving within the episode is silent"
        );
        // Normal must HOLD for PRESSURE_REARM (300s) before the episode
        // counts as over — it starts at step 155 (t=310s), so the
        // episode survives until t=610s
        for i in 155..305 {
            assert!(step(i, 1).is_empty(), "normal is silent (t={}s)", 2 * i);
        }
        assert!(step(305, 1).is_empty(), "the episode ends silently");
        // A new episode must serve its own persistence requirement
        assert!(step(306, 2).is_empty(), "new episode restarts the clock");
        for i in 307..456 {
            assert!(
                step(i, 2).is_empty(),
                "still inside the new persistence window (t={}s)",
                2 * i
            );
        }
        assert_eq!(step(456, 2).len(), 1, "and alerts once sustained again");
    }

    #[test]
    fn brief_return_to_normal_does_not_restart_the_pressure_episode() {
        // The real-machine failure this fixes: the fire side demanded
        // 5 minutes of persistence while the clear side accepted a
        // single normal sample, so one continuous stretch of memory
        // pressure was chopped into a string of alerts that each
        // arrived as news (all `repeat=false` in the daemon log)
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut step = |i: u64, level: u32| {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(level);
            engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events
        };

        assert!(step(0, 1).is_empty());
        for i in 1..151 {
            assert!(step(i, 2).is_empty());
        }
        assert_eq!(step(151, 2).len(), 1, "sustained warning alerts once");

        // Four minutes of normal — under PRESSURE_REARM, so noise
        for i in 152..272 {
            assert!(step(i, 1).is_empty(), "the dip is silent (t={}s)", 2 * i);
        }
        // Back over the line. This is the SAME episode, so it must not
        // alert again even once another full SLOW_WINDOW has passed —
        // under the old rule this is exactly where the duplicate landed
        for i in 272..600 {
            assert!(
                step(i, 2).is_empty(),
                "one episode alerts once, whatever the kernel did in \
                 between (t={}s)",
                2 * i
            );
        }
    }

    #[test]
    fn pressure_reminders_back_off_and_are_flagged_as_repeats() {
        // Ticks are irregular on purpose: the rule reads the clock, so
        // it must not depend on the sampling cadence
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut at = |secs: u64, level: u32| {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(level);
            engine
                .evaluate(base + Duration::from_secs(secs), &s, &active)
                .events
        };

        assert!(at(0, 2).is_empty(), "episode starts");
        assert!(at(299, 2).is_empty(), "one second short of persistence");
        let first = at(300, 2);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].repeat_after, None, "the first alert is news");

        // Gaps double from PERSIST_REMINDER and cap at
        // PRESSURE_REMINDER_MAX: 30m, 1h, 2h, 4h, 4h. repeat_after
        // counts from the FIRST alert, as it does for every other rule
        for (silent, fires, since_first) in [
            (2099u64, 2100u64, 1800u64),
            (5699, 5700, 5400),
            (12899, 12900, 12600),
            (27299, 27300, 27000),
            (41699, 41700, 41400),
        ] {
            assert!(at(silent, 2).is_empty(), "too soon at t={silent}s");
            let events = at(fires, 2);
            assert_eq!(events.len(), 1, "reminder due at t={fires}s");
            assert_eq!(
                events[0].repeat_after,
                Some(Duration::from_secs(since_first)),
                "at t={fires}s"
            );
            assert!(
                events[0]
                    .summary()
                    .contains(&format!("still going after {} min", since_first / 60)),
                "got: {}",
                events[0].summary()
            );
        }
    }

    #[test]
    fn worsening_pressure_restarts_the_reminder_backoff() {
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut at = |secs: u64, level: u32| {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(level);
            engine
                .evaluate(base + Duration::from_secs(secs), &s, &active)
                .events
        };

        assert!(at(0, 2).is_empty());
        assert_eq!(at(300, 2).len(), 1, "warning alerts once sustained");
        // Escalation does not wait for the reminder clock — persistence
        // is already established — and it reads as news, not a repeat
        let crit = at(360, 4);
        assert_eq!(crit.len(), 1, "worsening escalates immediately");
        assert_eq!(crit[0].repeat_after, None);
        assert_eq!(crit[0].severity(), Severity::Critical);

        // The backoff now runs from the escalation, not from the first
        // alert: t=2100 would have been due under the old clock
        assert!(at(2100, 4).is_empty(), "backoff restarted at t=360s");
        let reminder = at(2160, 4);
        assert_eq!(reminder.len(), 1);
        assert_eq!(
            reminder[0].repeat_after,
            Some(Duration::from_secs(1860)),
            "but repeat_after still counts from the first alert"
        );
    }

    #[test]
    fn critical_pressure_alerts_faster_than_warning() {
        // An episode that starts at critical needs only WINDOW (60s),
        // not the 5 minutes a warning must persist
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(2),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;
        for i in 0..40u64 {
            let mut s = snapshot(Vec::new(), 100);
            s.memory.pressure_level = Some(4);
            let events = engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events;
            if 2 * i < 60 {
                assert!(events.is_empty(), "not before 60s (t={}s)", 2 * i);
            }
            fired += events.len();
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn critical_only_setting_ignores_sustained_warning() {
        // The memory-heavy case: warning is this machine's normal state,
        // so only critical should ever interrupt
        let active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: Some(4),
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let base = Instant::now();
        let mut fired = 0;
        for i in 0..200u64 {
            let mut s = snapshot(Vec::new(), 100);
            // Warning for 400s straight, then critical
            s.memory.pressure_level = Some(if i < 200 { 2 } else { 4 });
            fired += engine
                .evaluate(base + Duration::from_secs(2 * i), &s, &active)
                .events
                .len();
        }
        assert_eq!(fired, 0, "sustained warning never alerts at critical-only");
    }

    #[test]
    fn pressure_rule_can_be_disabled_and_skips_absent_data() {
        let mut active = ActiveThresholds {
            cpu: Thresholds::new(None),
            memory: Thresholds::new(None),
            disk: Thresholds::new(None),
            app_cpu: Thresholds::new(None),
            app_memory: Thresholds::new(None),
            pressure: None,
            cooldown: Duration::from_secs(600),
        };
        let mut engine = AlertEngine::new();
        let mut s = snapshot(Vec::new(), 100);
        s.memory.pressure_level = Some(4);
        assert!(
            engine
                .evaluate(Instant::now(), &s, &active)
                .events
                .is_empty(),
            "disabled rule stays silent even at critical"
        );

        // Enabled but the platform reports no level (non-macOS): silent
        active.pressure = Some(2);
        let s = snapshot(Vec::new(), 100);
        assert!(
            engine
                .evaluate(Instant::now(), &s, &active)
                .events
                .is_empty()
        );
    }

    #[test]
    fn from_config_uses_builtin_defaults_when_nothing_is_set() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        assert_eq!(merged.cpu.for_process("any"), Some(30.0));
        assert_eq!(merged.memory.for_process("any"), Some(0.25));
        assert_eq!(merged.disk.for_process("/"), Some(0.90));
        assert_eq!(merged.pressure, Some(2));
        assert_eq!(merged.cooldown, Duration::from_secs(600));
        assert!(merged.any_enabled());

        // The key's original boolean form still parses, and the
        // three-way form maps to kernel levels
        let off: AlertsConfig = toml::from_str("pressure = false").expect("bool form");
        assert_eq!(ActiveThresholds::from_config(&off).pressure, None);
        let warn: AlertsConfig = toml::from_str("pressure = true").expect("bool form");
        assert_eq!(ActiveThresholds::from_config(&warn).pressure, Some(2));
        let crit: AlertsConfig = toml::from_str(r#"pressure = "critical""#).expect("name form");
        assert_eq!(ActiveThresholds::from_config(&crit).pressure, Some(4));
        assert!(toml::from_str::<AlertsConfig>(r#"pressure = "loud""#).is_err());
    }

    #[test]
    fn from_config_disk_zero_disables_and_overrides_map() {
        let mut file = AlertsConfig {
            disk: Some(0.0),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.disk.for_process("/"), None);

        file.disk = Some(80.0);
        file.disk_overrides.insert("/Volumes/Backup".into(), 0.0);
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.disk.for_process("/"), Some(0.80));
        assert_eq!(merged.disk.for_process("/Volumes/Backup"), None);
    }

    #[test]
    fn from_config_file_beats_builtin() {
        let mut file = AlertsConfig {
            cpu: Some(50.0),
            cooldown: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        file.cpu_overrides.insert("ghostty".into(), 100.0);

        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.cpu.for_process("other"), Some(50.0));
        assert_eq!(merged.cpu.for_process("Ghostty"), Some(100.0));
        assert_eq!(merged.cpu.base(), Some(50.0));
        assert_eq!(merged.cooldown, Duration::from_secs(300));
    }

    #[test]
    fn template_fills_names_the_user_did_not_configure() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        // Template entries are active out of the box
        assert_eq!(merged.cpu.for_process("Google Chrome"), Some(100.0));
        assert_eq!(merged.cpu.for_process("SourceKitService"), Some(200.0));
        // A template value of 0 disables the rule for that process
        assert_eq!(merged.cpu.for_process("mds_stores"), None);
        // Everything not templated falls through to the base default
        assert_eq!(merged.cpu.for_process("some-random-app"), Some(30.0));
    }

    #[test]
    fn user_override_shadows_template_entry() {
        let mut file = AlertsConfig::default();
        file.cpu_overrides.insert("ghostty".into(), 60.0);
        file.cpu_overrides.insert("mds_stores".into(), 50.0);
        let merged = ActiveThresholds::from_config(&file);

        // User values win over same-name template entries (case-insensitive)
        assert_eq!(merged.cpu.for_process("Ghostty"), Some(60.0));
        assert_eq!(merged.cpu.for_process("mds_stores"), Some(50.0));
        // Untouched template entries stay active
        assert_eq!(merged.cpu.for_process("gopls"), Some(100.0));
    }

    #[test]
    fn builtin_template_parses_and_every_key_is_usable() {
        // The compiled-in table is data in a file, so nothing but this
        // test stands between a typo there and a shipped binary whose
        // template silently does not apply
        let t = Template::builtin();
        assert_eq!(t.version, Some(TEMPLATE_VERSION));
        assert!(t.cpu.len() > 80, "cpu entries: {}", t.cpu.len());
        assert!(!t.app_cpu.is_empty() && !t.app_mem.is_empty());

        // Spot-check one entry per tier, so a bad value cannot pass by
        // merely parsing
        assert_eq!(t.cpu.get("rust-analyzer*"), Some(&100.0));
        assert_eq!(t.cpu.get("rustc"), Some(&0.0));
        assert_eq!(t.cpu.get("Xcode"), Some(&200.0));
        assert_eq!(t.app_cpu.get("login"), Some(&0.0));
        assert_eq!(t.app_mem.get("Google Chrome"), Some(&60.0));
    }

    #[test]
    fn template_parse_rejects_bad_versions_tables_and_keys() {
        assert!(Template::parse("[cpu]\nfoo = 1.0").is_err(), "no version");
        assert!(Template::parse("version = 999").is_err(), "wrong version");
        assert!(
            Template::parse("version = 1\n[nope]\na = 1.0").is_err(),
            "unknown table"
        );
        // A key the Matcher cannot honour would silently match nothing;
        // a template is only useful if every line in it took effect
        assert!(
            Template::parse("version = 1\n[cpu]\n\"a*b\" = 1.0").is_err(),
            "interior star"
        );
        let ok = Template::parse("version = 1\n[cpu]\n\"a*\" = 5.0").expect("valid");
        assert_eq!(ok.cpu.get("a*"), Some(&5.0));
        assert!(ok.app_cpu.is_empty(), "missing tables default to empty");
    }

    #[test]
    fn a_loaded_template_replaces_the_builtin_rather_than_layering() {
        let template = Template::parse("version = 1\n[cpu]\ngopls = 42.0").expect("valid");
        let merged =
            ActiveThresholds::from_config_with_template(&AlertsConfig::default(), &template);

        assert_eq!(merged.cpu.for_process("gopls"), Some(42.0));
        // Replacement, not a layer: an entry the file drops is gone, so
        // a name can actually be REMOVED from the table. It falls back
        // to the base rule, not to the builtin's value
        assert_eq!(merged.cpu.for_process("rustc"), Some(DEFAULT_CPU_PERCENT));
        assert_eq!(
            merged.cpu.for_process("rust-analyzer-2026-08-10.1"),
            Some(DEFAULT_CPU_PERCENT)
        );
        // A user override still outranks whatever the template says
        let mut file = AlertsConfig::default();
        file.cpu_overrides.insert("gopls".into(), 77.0);
        let merged = ActiveThresholds::from_config_with_template(&file, &template);
        assert_eq!(merged.cpu.for_process("gopls"), Some(77.0));
    }

    #[test]
    fn matcher_reads_leading_and_trailing_stars_only() {
        assert_eq!(
            Matcher::parse("rust-analyzer"),
            Ok(Matcher::Exact("rust-analyzer".into()))
        );
        assert_eq!(
            Matcher::parse("rust-analyzer*"),
            Ok(Matcher::Prefix("rust-analyzer".into()))
        );
        assert_eq!(
            Matcher::parse("*Helper (Renderer)"),
            Ok(Matcher::Suffix("Helper (Renderer)".into()))
        );
        assert_eq!(
            Matcher::parse("*analyzer*"),
            Ok(Matcher::Contains("analyzer".into()))
        );
        // An interior `*` is refused rather than taken literally: a key
        // that looks like a pattern must never quietly match nothing
        assert!(Matcher::parse("rust*analyzer").is_err());

        let prefix = Matcher::parse("rust-analyzer*").expect("prefix");
        assert!(prefix.matches("rust-analyzer-2026-08-10.1"));
        assert!(prefix.matches("rust-analyzer-proc-macro-srv"));
        assert!(prefix.matches("RUST-ANALYZER"), "case-insensitive");
        assert!(!prefix.matches("rust-analyze"), "shorter than the literal");
        assert!(!prefix.matches("my-rust-analyzer"), "prefix, not contains");

        let suffix = Matcher::parse("*Helper (Renderer)").expect("suffix");
        assert!(suffix.matches("Google Chrome Helper (Renderer)"));
        assert!(!suffix.matches("Google Chrome Helper (GPU)"));

        let contains = Matcher::parse("*analyzer*").expect("contains");
        assert!(contains.matches("my-rust-analyzer-1"));
        assert!(!contains.matches("rustc"));

        // Degenerate keys must not panic — `windows(0)` would
        assert!(Matcher::parse("*").expect("star").matches("anything"));
        assert!(Matcher::parse("**").expect("stars").matches("anything"));
        // A UTF-8 name is compared as bytes, so no boundary slicing
        assert!(!prefix.matches("中文进程"));
    }

    #[test]
    fn template_pattern_covers_a_versioned_binary_and_its_siblings() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        // Zed rewrites this name on every rust-analyzer update, so the
        // old exact `rust-analyzer` entry never matched and the indexer
        // alerted against the 30% base — seen in a real daemon log
        assert_eq!(
            merged.cpu.for_process("rust-analyzer-2026-08-10.1"),
            Some(100.0)
        );
        assert_eq!(
            merged.cpu.for_process("rust-analyzer-proc-macro-srv"),
            Some(100.0)
        );
        assert_eq!(merged.cpu.for_process("rust-analyzer"), Some(100.0));
        // Still a prefix: an unrelated process is not swept up
        assert_eq!(merged.cpu.for_process("rustc"), None);
    }

    #[test]
    fn exact_names_and_user_entries_outrank_patterns() {
        let mut file = AlertsConfig::default();
        // A user pattern beats the TEMPLATE's exact entry: an explicit
        // override is a request, a template entry is only a default
        file.cpu_overrides.insert("*Helper (Renderer)".into(), 55.0);
        // ...and the user's own exact name beats their own pattern
        file.cpu_overrides
            .insert("Google Chrome Helper (Renderer)".into(), 70.0);
        let merged = ActiveThresholds::from_config(&file);

        assert_eq!(
            merged.cpu.for_process("Google Chrome Helper (Renderer)"),
            Some(70.0),
            "exact beats pattern within the user layer"
        );
        assert_eq!(
            merged.cpu.for_process("Slack Helper (Renderer)"),
            Some(55.0),
            "user pattern beats the template's exact entry (100)"
        );
        assert_eq!(
            merged.cpu.for_process("Google Chrome Helper (GPU)"),
            Some(100.0),
            "no user entry matches, so the template still applies"
        );
    }

    #[test]
    fn longer_patterns_win_over_shorter_ones() {
        let mut file = AlertsConfig::default();
        file.cpu_overrides.insert("rust*".into(), 40.0);
        file.cpu_overrides.insert("rust-analyzer*".into(), 0.0);
        let merged = ActiveThresholds::from_config(&file);

        // The config's override map is a BTreeMap, so precedence cannot
        // depend on the order these were written in — the longer
        // literal is the more specific claim and wins
        assert_eq!(merged.cpu.for_process("rust-analyzer-2026-08-10.1"), None);
        assert_eq!(merged.cpu.for_process("rustc"), Some(40.0));
    }

    #[test]
    fn app_template_silences_terminal_sessions_and_raises_browsers() {
        let merged = ActiveThresholds::from_config(&AlertsConfig::default());
        // Terminal sessions group under a `login` root on macOS: a build
        // there is your own work, and its members still have per-process
        // rules, so the whole-app rules stay out of it
        assert_eq!(merged.app_cpu.for_process("login"), None);
        assert_eq!(merged.app_memory.for_process("login"), None);
        // Browsers get a raised bar, not an exemption
        assert_eq!(merged.app_cpu.for_process("Google Chrome"), Some(400.0));
        assert_eq!(merged.app_memory.for_process("Google Chrome"), Some(0.60));
        // Anything untemplated falls through to the base values
        assert_eq!(merged.app_cpu.for_process("some-app"), Some(200.0));
        assert_eq!(merged.app_memory.for_process("some-app"), Some(0.40));
    }

    #[test]
    fn user_app_overrides_shadow_the_app_template() {
        let mut file = AlertsConfig::default();
        file.app_cpu_overrides.insert("login".into(), 800.0);
        file.app_mem_overrides.insert("Google Chrome".into(), 30.0);
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.app_cpu.for_process("login"), Some(800.0));
        assert_eq!(merged.app_memory.for_process("Google Chrome"), Some(0.30));

        // ... and the template layer goes away with the same switch that
        // controls the per-process one
        let off = AlertsConfig {
            template: Some(false),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&off);
        assert_eq!(merged.app_cpu.for_process("login"), Some(200.0));
        assert_eq!(merged.app_memory.for_process("Google Chrome"), Some(0.40));
    }

    #[test]
    fn disabling_a_base_rule_also_retires_its_template() {
        // `alert-cpu 0` means "no CPU alerts", so the ~60 templated apps
        // must not keep firing behind the user's back
        let file = AlertsConfig {
            cpu: Some(0.0),
            app_cpu: Some(0.0),
            app_mem: Some(0.0),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.cpu.for_process("Google Chrome"), None);
        assert!(!merged.cpu.any_enabled());
        assert_eq!(merged.app_cpu.for_process("Google Chrome"), None);
        assert!(!merged.app_cpu.any_enabled());
        assert!(!merged.app_memory.any_enabled());

        // An explicit user override is a request, not a default: it
        // still applies over a disabled base
        let mut file = AlertsConfig {
            cpu: Some(0.0),
            ..Default::default()
        };
        file.cpu_overrides.insert("ghostty".into(), 100.0);
        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.cpu.for_process("ghostty"), Some(100.0));
        assert_eq!(merged.cpu.for_process("Google Chrome"), None);
    }

    #[test]
    fn template_can_be_disabled_entirely() {
        let file = AlertsConfig {
            template: Some(false),
            ..Default::default()
        };
        let merged = ActiveThresholds::from_config(&file);
        // Template names fall back to the base default like any other app
        assert_eq!(merged.cpu.for_process("Google Chrome"), Some(30.0));
        assert_eq!(merged.cpu.for_process("mds_stores"), Some(30.0));
    }

    #[test]
    fn from_config_zero_disables() {
        let mut file = AlertsConfig {
            mem: Some(0.0),
            ..Default::default()
        };
        file.cpu_overrides.insert("chrome".into(), 0.0);

        let merged = ActiveThresholds::from_config(&file);
        assert_eq!(merged.memory.for_process("any"), None);
        assert_eq!(merged.cpu.for_process("chrome"), None);
        assert_eq!(merged.cpu.for_process("other"), Some(30.0));
    }
}

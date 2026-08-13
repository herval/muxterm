//! Scheduled tasks ("automations"), shared by the GUI and the `mux` CLI.
//!
//! An automation is a name, a schedule, a folder, and a payload: either an
//! agent prompt or a plain shell command. Each one owns a dedicated muxterm
//! tab whose pane is where every execution runs - so the tmux scrollback is
//! the live log, and it survives quit/relaunch like any other session. The
//! GUI schedules (on its one-second tick, no daemon); this module owns the
//! parts both binaries need: the schedule grammar, the per-run records on
//! disk, and the runner behind `mux automations exec`.
//!
//! Everything above the runner is pure and clock-injected, so the schedule
//! math unit-tests without waiting for a Tuesday.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{
    DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike,
    Weekday,
};
use serde::{Deserialize, Serialize};

use crate::mesh;
use crate::state::{self, AutomationState};

/// Run records kept per automation; older ones are pruned after each run.
pub const KEEP_RUNS: usize = 50;

/// A run whose record still says `RUNNING` this long after its pane went
/// back to a shell was killed with its process (ctrl+c, cmd+w, a crash).
pub const STALE_AFTER: u64 = 15;

pub const RUNNING: &str = "running";
pub const OK: &str = "ok";
pub const FAILED: &str = "failed";
pub const INTERRUPTED: &str = "interrupted";

pub const TRIGGER_SCHEDULE: &str = "schedule";
pub const TRIGGER_MANUAL: &str = "manual";

// ---------------------------------------------------------------------------
// The automation itself
// ---------------------------------------------------------------------------

/// A scheduled task. Mirrors `state::AutomationState` the way
/// `workspace::Project` mirrors `state::ProjectState`; the GUI owns the live
/// list and the CLI reads it through `state::peek()`.
#[derive(Clone, Debug, PartialEq)]
pub struct Automation {
    pub id: String,
    pub name: String,
    /// The raw schedule text, re-parsed on use. Stored as written so a
    /// round trip through state.json never rewrites the user's wording.
    pub schedule: String,
    pub enabled: bool,
    pub root: Option<PathBuf>,
    /// `agent::Agent::id`, or None for a plain shell command.
    pub agent: Option<&'static str>,
    pub model: Option<String>,
    pub prompt: String,
    pub command: Option<String>,
    pub created_at: u64,
}

impl Automation {
    pub fn new(name: String, schedule: String) -> Self {
        Self {
            id: new_id(),
            name,
            schedule,
            enabled: true,
            root: None,
            agent: None,
            model: None,
            prompt: String::new(),
            command: None,
            created_at: mesh::now(),
        }
    }

    pub fn parsed(&self) -> Result<Schedule, String> {
        parse(&self.schedule)
    }

    /// What this automation will actually run, for display. Never the whole
    /// prompt - a row has one line.
    pub fn payload_label(&self) -> String {
        match (&self.agent, &self.command) {
            (Some(a), _) => format!("{a}: {}", first_line(&self.prompt)),
            (None, Some(c)) => first_line(c),
            (None, None) => "nothing to run".to_string(),
        }
    }

    pub fn to_state(&self) -> AutomationState {
        AutomationState {
            id: self.id.clone(),
            name: self.name.clone(),
            schedule: self.schedule.clone(),
            enabled: self.enabled,
            root: self.root.clone(),
            agent: self.agent.map(str::to_string),
            model: self.model.clone(),
            prompt: self.prompt.clone(),
            command: self.command.clone(),
            created_at: self.created_at,
        }
    }

    pub fn from_state(s: AutomationState) -> Self {
        Self {
            id: s.id,
            name: s.name,
            schedule: s.schedule,
            enabled: s.enabled,
            root: s.root,
            // An unknown agent id (registry changed under an old state file)
            // degrades to "no agent" rather than failing the load.
            agent: s.agent.as_deref().and_then(|id| {
                crate::agent::by_id(id).map(|a| a.id)
            }),
            model: s.model,
            prompt: s.prompt,
            command: s.command,
            created_at: s.created_at,
        }
    }
}

pub fn new_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("auto-{}", &id[..8])
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Every saved automation, straight off disk. The CLI's read path; the GUI
/// holds its own live list instead.
pub fn load_all() -> Vec<Automation> {
    state::peek()
        .map(|s| s.automations.into_iter().map(Automation::from_state).collect())
        .unwrap_or_default()
}

/// Resolve a user-typed automation reference: exact name (case-insensitive),
/// then exact id, then unique id prefix.
pub fn find<'a>(list: &'a [Automation], key: &str) -> Option<&'a Automation> {
    let k = key.trim();
    list.iter()
        .find(|a| a.name.eq_ignore_ascii_case(k))
        .or_else(|| list.iter().find(|a| a.id == k))
        .or_else(|| {
            let mut hits = list.iter().filter(|a| a.id.starts_with(k));
            let first = hits.next()?;
            hits.next().is_none().then_some(first)
        })
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

/// When an automation fires. One string in state.json, parsed on use:
/// `every 30m`, `daily at 09:00`, `weekly on mon at 09:00`, `cron 0 9 * * 1-5`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    Every { seconds: u64 },
    Daily { hour: u32, minute: u32 },
    Weekly { weekday: Weekday, hour: u32, minute: u32 },
    Cron(CronSpec),
}

/// A schedule may never come round again (`cron 0 0 30 2 *` - February 30th).
/// The search is bounded at this many days, and a miss is reported rather
/// than spun on.
const SEARCH_DAYS: i64 = 366;

/// The shortest cadence we accept. A sub-minute automation would fire faster
/// than the GUI's own tick can notice, and re-entrancy is guarded by "is the
/// pane busy", not by a queue.
const MIN_EVERY: u64 = 60;

pub fn parse(input: &str) -> Result<Schedule, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("empty schedule".to_string());
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    match words.first().copied() {
        Some("every") => parse_every(&words[1..]),
        Some("daily") => parse_daily(&words[1..]),
        Some("weekly") => parse_weekly(&words[1..]),
        Some("cron") => parse_cron(&words[1..]).map(Schedule::Cron),
        _ => Err(format!(
            "{input:?} is not a schedule; try \"every 30m\", \
             \"daily at 09:00\", \"weekly on mon at 09:00\" or \
             \"cron 0 9 * * 1-5\""
        )),
    }
}

/// `every 30m` / `every 30 minutes` / `every 2h` / `every 1d`.
fn parse_every(words: &[&str]) -> Result<Schedule, String> {
    let joined = words.join(" ");
    let (digits, unit) = match words.len() {
        1 => {
            let w = words[0];
            let split = w
                .find(|c: char| !c.is_ascii_digit())
                .ok_or_else(|| format!("\"every {w}\" needs a unit, e.g. \"every 30m\""))?;
            w.split_at(split)
        },
        2 => (words[0], words[1]),
        _ => {
            return Err(format!(
                "could not read \"every {joined}\"; try \"every 30m\""
            ))
        },
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{digits:?} is not a number"))?;
    let mult = match unit.trim_end_matches('s') {
        "m" | "min" | "minute" => 60,
        "h" | "hr" | "hour" => 3600,
        "d" | "day" => 86400,
        other => {
            return Err(format!(
                "unknown unit {other:?}; use m, h or d"
            ))
        },
    };
    let seconds = n.checked_mul(mult).ok_or("interval too large")?;
    if seconds < MIN_EVERY {
        return Err("the shortest interval is 1m".to_string());
    }
    Ok(Schedule::Every { seconds })
}

/// `daily at 09:00`.
fn parse_daily(words: &[&str]) -> Result<Schedule, String> {
    let rest = strip_lead(words, "at");
    let [time] = rest else {
        return Err("try \"daily at 09:00\"".to_string());
    };
    let (hour, minute) = parse_time(time)?;
    Ok(Schedule::Daily { hour, minute })
}

/// `weekly on mon at 09:00`.
fn parse_weekly(words: &[&str]) -> Result<Schedule, String> {
    let rest = strip_lead(words, "on");
    let (day, rest) = rest
        .split_first()
        .ok_or("try \"weekly on mon at 09:00\"")?;
    let weekday = parse_weekday(day)?;
    let rest = strip_lead(rest, "at");
    let [time] = rest else {
        return Err("try \"weekly on mon at 09:00\"".to_string());
    };
    let (hour, minute) = parse_time(time)?;
    Ok(Schedule::Weekly { weekday, hour, minute })
}

fn strip_lead<'a>(words: &'a [&'a str], word: &str) -> &'a [&'a str] {
    match words.first() {
        Some(w) if *w == word => &words[1..],
        _ => words,
    }
}

fn parse_time(s: &str) -> Result<(u32, u32), String> {
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| format!("{s:?} is not a HH:MM time"))?;
    let hour: u32 =
        h.parse().map_err(|_| format!("{s:?} is not a HH:MM time"))?;
    let minute: u32 =
        m.parse().map_err(|_| format!("{s:?} is not a HH:MM time"))?;
    if hour > 23 || minute > 59 {
        return Err(format!("{s:?} is not a valid time of day"));
    }
    Ok((hour, minute))
}

fn parse_weekday(s: &str) -> Result<Weekday, String> {
    match &s[..s.len().min(3)] {
        "mon" => Ok(Weekday::Mon),
        "tue" => Ok(Weekday::Tue),
        "wed" => Ok(Weekday::Wed),
        "thu" => Ok(Weekday::Thu),
        "fri" => Ok(Weekday::Fri),
        "sat" => Ok(Weekday::Sat),
        "sun" => Ok(Weekday::Sun),
        _ => Err(format!("{s:?} is not a weekday")),
    }
}

impl Schedule {
    /// The canonical text for this schedule. Round-trips through `parse`.
    pub fn label(&self) -> String {
        match self {
            Schedule::Every { seconds } => {
                let (n, unit) = match seconds {
                    s if s % 86400 == 0 => (s / 86400, "d"),
                    s if s % 3600 == 0 => (s / 3600, "h"),
                    s => (s / 60, "m"),
                };
                format!("every {n}{unit}")
            },
            Schedule::Daily { hour, minute } => {
                format!("daily at {hour:02}:{minute:02}")
            },
            Schedule::Weekly { weekday, hour, minute } => format!(
                "weekly on {} at {hour:02}:{minute:02}",
                weekday_name(*weekday)
            ),
            Schedule::Cron(c) => format!("cron {}", c.label()),
        }
    }

    /// The first firing strictly after `after`, in local time. `None` means
    /// the schedule has no next occurrence within a year - a broken cron
    /// like `cron 0 0 30 2 *`, which the caller surfaces rather than
    /// retrying forever.
    ///
    /// The clock is an argument, not `Local::now()`, so this tests without
    /// waiting for a Tuesday (the `attention.rs` doctrine).
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match self {
            Schedule::Every { seconds } => after
                .checked_add_signed(chrono::Duration::seconds(*seconds as i64)),
            Schedule::Daily { hour, minute } => {
                next_daily(after, None, *hour, *minute)
            },
            Schedule::Weekly { weekday, hour, minute } => {
                next_daily(after, Some(*weekday), *hour, *minute)
            },
            Schedule::Cron(c) => c.next_after(after),
        }
    }
}

fn weekday_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

/// Walk forward a day at a time looking for the next `hour:minute` (on
/// `weekday`, when given) that lands strictly after `after`.
fn next_daily(
    after: DateTime<Local>,
    weekday: Option<Weekday>,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Local>> {
    let mut day = after.date_naive();
    for _ in 0..=SEARCH_DAYS {
        if weekday.is_none_or(|w| day.weekday() == w) {
            if let Some(dt) = local_at(day, hour, minute) {
                if dt > after {
                    return Some(dt);
                }
            }
        }
        day = day.succ_opt()?;
    }
    None
}

/// Resolve a naive local wall-clock time to an instant. DST is why this is
/// a function: on a spring-forward morning `02:30` does not exist (skip that
/// day), and on a fall-back one it happens twice (take the first).
fn local_at(day: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    let naive = day.and_hms_opt(hour, minute, 0)?;
    resolve_local(naive)
}

fn resolve_local(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&naive).earliest()
}

// ---------------------------------------------------------------------------
// cron
// ---------------------------------------------------------------------------

/// A five-field cron spec: minute, hour, day-of-month, month, day-of-week.
/// Supports `*`, `N`, `a-b`, `a,b`, and `*/n` (and `a-b/n`), with three-letter
/// names accepted for months and weekdays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSpec {
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CronField {
    Any,
    Set(Vec<u32>),
}

impl CronField {
    fn matches(&self, v: u32) -> bool {
        match self {
            CronField::Any => true,
            CronField::Set(vals) => vals.contains(&v),
        }
    }

    fn is_any(&self) -> bool {
        matches!(self, CronField::Any)
    }
}

fn parse_cron(words: &[&str]) -> Result<CronSpec, String> {
    if words.len() != 5 {
        return Err(format!(
            "cron takes 5 fields (minute hour day month weekday), got {}",
            words.len()
        ));
    }
    Ok(CronSpec {
        minute: parse_cron_field(words[0], 0, 59, &[])?,
        hour: parse_cron_field(words[1], 0, 23, &[])?,
        dom: parse_cron_field(words[2], 1, 31, &[])?,
        month: parse_cron_field(words[3], 1, 12, MONTHS)?,
        dow: parse_cron_field(words[4], 0, 6, DAYS)?,
    })
}

const MONTHS: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct",
    "nov", "dec",
];
const DAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_cron_field(
    field: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<CronField, String> {
    if field == "*" {
        return Ok(CronField::Any);
    }
    let mut vals: Vec<u32> = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad step in {field:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step cannot be zero in {field:?}"));
        }
        let (lo, hi) = match range {
            "*" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (
                    cron_value(a, min, max, names)?,
                    cron_value(b, min, max, names)?,
                ),
                // A bare `N/step` counts up from N, as cron does.
                None => {
                    let v = cron_value(r, min, max, names)?;
                    (v, if step > 1 { max } else { v })
                },
            },
        };
        if lo > hi {
            return Err(format!("{range:?} is backwards in {field:?}"));
        }
        vals.extend((lo..=hi).step_by(step as usize));
    }
    vals.sort_unstable();
    vals.dedup();
    if vals.is_empty() {
        return Err(format!("{field:?} matches nothing"));
    }
    Ok(CronField::Set(vals))
}

fn cron_value(
    tok: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<u32, String> {
    let v = match tok.parse::<u32>() {
        Ok(v) => v,
        Err(_) => {
            let key = &tok[..tok.len().min(3)];
            let idx = names
                .iter()
                .position(|n| *n == key)
                .ok_or_else(|| format!("{tok:?} is not a valid value"))?
                as u32;
            // Month names start at 1, weekday names at 0.
            idx + min.min(1)
        },
    };
    // Sunday is both 0 and 7 in cron.
    let v = if max == 6 && v == 7 { 0 } else { v };
    if v < min || v > max {
        return Err(format!("{tok:?} is out of range ({min}-{max})"));
    }
    Ok(v)
}

impl CronSpec {
    fn label(&self) -> String {
        [&self.minute, &self.hour, &self.dom, &self.month, &self.dow]
            .iter()
            .map(|f| field_label(f))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Standard cron day semantics: when *both* day-of-month and day-of-week
    /// are restricted they are OR'd (`0 0 1 * mon` is the 1st **or** any
    /// Monday); when only one is, it simply applies.
    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom = self.dom.matches(date.day());
        let dow =
            self.dow.matches(date.weekday().num_days_from_sunday());
        match (self.dom.is_any(), self.dow.is_any()) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        }
    }

    /// Step minute by minute. The field tests are on the *naive* clock, so
    /// only an actual match pays for a timezone resolution.
    fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let minute = chrono::Duration::minutes(1);
        let mut naive = after
            .naive_local()
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(minute)?;
        for _ in 0..(SEARCH_DAYS * 24 * 60) {
            if self.minute.matches(naive.minute())
                && self.hour.matches(naive.hour())
                && self.month.matches(naive.month())
                && self.day_matches(naive.date())
            {
                // A nonexistent wall-clock minute (spring forward) simply
                // does not fire; the search moves on.
                if let Some(dt) = resolve_local(naive) {
                    if dt > after {
                        return Some(dt);
                    }
                }
            }
            naive = naive.checked_add_signed(minute)?;
        }
        None
    }
}

fn field_label(f: &CronField) -> String {
    match f {
        CronField::Any => "*".to_string(),
        CronField::Set(v) => {
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
        },
    }
}

// ---------------------------------------------------------------------------
// Run records
// ---------------------------------------------------------------------------

/// One execution. `status` is deliberately an open string - an unknown value
/// renders as "unknown" rather than failing the read, the same contract as
/// `mesh::AgentState`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Run {
    pub id: String,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub trigger: String,
}

impl Run {
    pub fn is_running(&self) -> bool {
        self.status == RUNNING
    }

    pub fn duration(&self) -> Option<u64> {
        self.finished_at.map(|f| f.saturating_sub(self.started_at))
    }
}

pub fn automations_dir() -> PathBuf {
    state::config_dir().join("automations")
}

pub fn runs_dir(automation: &str) -> PathBuf {
    automations_dir().join(automation)
}

pub fn run_path(automation: &str, run: &str) -> PathBuf {
    runs_dir(automation).join(format!("{run}.json"))
}

pub fn log_path(automation: &str, run: &str) -> PathBuf {
    runs_dir(automation).join(format!("{run}.log"))
}

/// `<unix-seconds>-<4hex>`: zero-padded so lexicographic order is
/// chronological (ten digits until the year 2286), with a suffix so two runs
/// in the same second cannot collide.
pub fn new_run_id(now: u64) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{now:010}-{}", &id[..4])
}

/// Overwrite a run's record. Atomic (pid-unique temp + rename) because the
/// GUI reads this directory on every tick while the runner is writing it.
pub fn write_run(automation: &str, run: &Run) -> anyhow::Result<()> {
    let dir = runs_dir(automation);
    let _ = fs::create_dir_all(&dir);
    let tmp = dir.join(format!("{}.json.{}", run.id, std::process::id()));
    fs::write(&tmp, serde_json::to_string(run)?)?;
    fs::rename(&tmp, run_path(automation, &run.id))?;
    Ok(())
}

/// Every run of one automation, newest first. Unreadable entries are skipped
/// - a torn or foreign file must not break the history.
pub fn read_runs(automation: &str) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let Ok(entries) = fs::read_dir(runs_dir(automation)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(run) = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Run>(&t).ok())
        {
            out.push(run);
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

pub fn read_log(automation: &str, run: &str) -> Option<String> {
    fs::read_to_string(log_path(automation, run)).ok()
}

/// The run directory's mtime, which a tmp+rename bumps on every record
/// write. The GUI keys its history cache on this instead of re-reading every
/// run on every tick (the `config::mtime` precedent).
pub fn runs_mtime(automation: &str) -> Option<SystemTime> {
    fs::metadata(runs_dir(automation)).and_then(|m| m.modified()).ok()
}

/// Keep the newest `keep` runs and drop the rest, records and logs together.
pub fn prune_runs(automation: &str, keep: usize) {
    let runs = read_runs(automation);
    for run in runs.into_iter().skip(keep) {
        let _ = fs::remove_file(run_path(automation, &run.id));
        let _ = fs::remove_file(log_path(automation, &run.id));
    }
}

/// Drop an automation's whole history (it was deleted).
pub fn remove_runs(automation: &str) {
    let _ = fs::remove_dir_all(runs_dir(automation));
}

// ---------------------------------------------------------------------------
// The runner (`mux automations exec`)
// ---------------------------------------------------------------------------

/// Run one automation here and now, in the pane this was typed into.
///
/// Output is teed: straight to this terminal, so the automation's tab shows
/// the work live and tmux keeps it in scrollback, *and* to the run's `.log`,
/// so the sidebar overlay can show it without opening the tab. Two pump
/// threads is what keeps a chatty child from filling a pipe and deadlocking
/// (the reason `agent::output_with_timeout` cannot be used here - see its
/// doc comment).
pub fn exec(automation: &Automation, trigger: &str) -> Result<i32, String> {
    let started = mesh::now();
    let run_id = new_run_id(started);
    let mut record = Run {
        id: run_id.clone(),
        started_at: started,
        finished_at: None,
        status: RUNNING.to_string(),
        exit_code: None,
        trigger: trigger.to_string(),
    };
    if let Err(e) = write_run(&automation.id, &record) {
        return Err(format!("could not record the run: {e:#}"));
    }

    let _ = fs::create_dir_all(runs_dir(&automation.id));
    let log = fs::File::create(log_path(&automation.id, &run_id))
        .map_err(|e| format!("could not open the run log: {e}"))?;
    let log = Arc::new(Mutex::new(log));

    let banner = format!(
        "[muxterm] {} - {} run at {}\n",
        automation.name,
        trigger,
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    emit(&log, banner.as_bytes());

    let mut cmd = match build_command(automation) {
        Ok(c) => c,
        Err(e) => {
            emit(&log, format!("[muxterm] {e}\n").as_bytes());
            finish(automation, &mut record, FAILED, None);
            return Err(e);
        },
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not start the run: {e}");
            emit(&log, format!("[muxterm] {msg}\n").as_bytes());
            finish(automation, &mut record, FAILED, None);
            return Err(msg);
        },
    };

    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(pump(out, log.clone(), Sink::Out));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(pump(err, log.clone(), Sink::Err));
    }
    let code = child
        .wait()
        .map(|s| s.code().unwrap_or(1))
        .map_err(|e| format!("waiting for the run: {e}"))?;
    for p in pumps {
        let _ = p.join();
    }

    let status = if code == 0 { OK } else { FAILED };
    emit(
        &log,
        format!(
            "[muxterm] {} in {}s (exit {code})\n",
            status,
            mesh::now().saturating_sub(started)
        )
        .as_bytes(),
    );
    finish(automation, &mut record, status, Some(code));
    Ok(code)
}

fn finish(
    automation: &Automation,
    record: &mut Run,
    status: &str,
    code: Option<i32>,
) {
    record.finished_at = Some(mesh::now());
    record.status = status.to_string();
    record.exit_code = code;
    if let Err(e) = write_run(&automation.id, record) {
        log::warn!("could not record the run result: {e:#}");
    }
    prune_runs(&automation.id, KEEP_RUNS);
}

/// What an automation actually spawns. The agent case is deliberately the
/// unattended registry posture (`agent::scheduled_argv`) - there is nobody at
/// the keyboard to approve a tool call at 3am.
fn build_command(a: &Automation) -> Result<Command, String> {
    let mut cmd = match (a.agent, &a.command) {
        (Some(id), _) => {
            let agent = crate::agent::by_id(id)
                .ok_or_else(|| format!("unknown agent {id:?}"))?;
            if a.prompt.trim().is_empty() {
                return Err("this automation has no prompt".to_string());
            }
            let argv = crate::agent::scheduled_argv(
                agent,
                a.model.as_deref(),
                &a.prompt,
            );
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        },
        (None, Some(line)) if !line.trim().is_empty() => {
            let shell = std::env::var("SHELL")
                .unwrap_or_else(|_| "/bin/sh".to_string());
            let mut c = Command::new(shell);
            c.arg("-lc").arg(line);
            c
        },
        _ => return Err("this automation has nothing to run".to_string()),
    };
    if let Some(root) = a.root.as_ref().filter(|r| r.is_dir()) {
        cmd.current_dir(root);
    }
    cmd.env("MUXTERM_AUTOMATION", &a.id);
    Ok(cmd)
}

enum Sink {
    Out,
    Err,
}

fn pump<R: Read + Send + 'static>(
    mut reader: R,
    log: Arc<Mutex<fs::File>>,
    sink: Sink,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    match sink {
                        Sink::Out => {
                            let mut o = std::io::stdout();
                            let _ = o.write_all(chunk);
                            let _ = o.flush();
                        },
                        Sink::Err => {
                            let mut e = std::io::stderr();
                            let _ = e.write_all(chunk);
                            let _ = e.flush();
                        },
                    }
                    if let Ok(mut f) = log.lock() {
                        let _ = f.write_all(chunk);
                    }
                },
            }
        }
    })
}

/// Write muxterm's own narration to both the terminal and the log, so the
/// captured log reads exactly like the pane did.
fn emit(log: &Arc<Mutex<fs::File>>, bytes: &[u8]) {
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(bytes);
    }
}

/// Human-readable "3m ago" / "in 2h" for a unix timestamp against `now`.
pub fn relative(ts: u64, now: u64) -> String {
    let (delta, suffix, prefix) = if ts >= now {
        (ts - now, "", "in ")
    } else {
        (now - ts, " ago", "")
    };
    if delta == 0 {
        return "now".to_string();
    }
    let body = match delta {
        0..=59 => format!("{delta}s"),
        60..=3599 => format!("{}m", delta / 60),
        3600..=86399 => format!("{}h", delta / 3600),
        _ => format!("{}d", delta / 86400),
    };
    format!("{prefix}{body}{suffix}")
}

/// Format a unix timestamp as a local wall-clock stamp for run rows.
pub fn stamp(ts: u64) -> String {
    match Local.timestamp_opt(ts as i64, 0).earliest() {
        Some(dt) => dt.format("%b %d %H:%M").to_string(),
        None => "-".to_string(),
    }
}

/// `Path` -> display string, for the CLI's list output.
pub fn root_label(root: Option<&Path>) -> String {
    root.map(|r| r.display().to_string()).unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        local_at(NaiveDate::from_ymd_opt(y, m, d).unwrap(), h, min)
            .expect("test times are real local times")
    }

    #[test]
    fn presets_round_trip_through_parse() {
        for text in [
            "every 30m",
            "every 2h",
            "every 1d",
            "daily at 09:00",
            "weekly on mon at 09:00",
            "cron 0 9 * * 1,2,3,4,5",
        ] {
            let parsed = parse(text).expect(text);
            assert_eq!(parsed.label(), text, "{text} did not round trip");
            assert_eq!(parse(&parsed.label()).unwrap(), parsed);
        }
    }

    #[test]
    fn spelled_out_intervals_parse() {
        assert_eq!(
            parse("every 30 minutes").unwrap(),
            Schedule::Every { seconds: 1800 }
        );
        assert_eq!(
            parse("EVERY 2 Hours").unwrap(),
            Schedule::Every { seconds: 7200 }
        );
        assert_eq!(
            parse("daily 09:30").unwrap(),
            Schedule::Daily { hour: 9, minute: 30 }
        );
    }

    #[test]
    fn bad_schedules_say_why() {
        for bad in [
            "", "evry 5m", "every", "every 5x", "every 30s",
            "daily at 25:00", "daily at nine", "weekly on funday at 09:00",
            "cron 0 9 * *", "cron 99 * * * *",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(!err.is_empty(), "{bad:?} produced an empty error");
        }
    }

    #[test]
    fn every_counts_from_the_given_moment() {
        let now = at(2026, 3, 10, 9, 0);
        let next = parse("every 30m").unwrap().next_after(now).unwrap();
        assert_eq!(next, at(2026, 3, 10, 9, 30));
    }

    #[test]
    fn daily_rolls_to_tomorrow_once_past() {
        let s = parse("daily at 09:00").unwrap();
        assert_eq!(
            s.next_after(at(2026, 3, 10, 8, 0)).unwrap(),
            at(2026, 3, 10, 9, 0)
        );
        // Exactly at the firing minute, the *next* one is tomorrow.
        assert_eq!(
            s.next_after(at(2026, 3, 10, 9, 0)).unwrap(),
            at(2026, 3, 11, 9, 0)
        );
    }

    #[test]
    fn weekly_lands_on_its_weekday() {
        let s = parse("weekly on mon at 09:00").unwrap();
        let next = s.next_after(at(2026, 3, 10, 9, 0)).unwrap();
        assert_eq!(next.weekday(), Weekday::Mon);
        assert!(next > at(2026, 3, 10, 9, 0));
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn a_daily_time_that_dst_skips_does_not_hang() {
        // Whatever this machine's zone, the search is bounded and either
        // finds a real instant or reports none - it never spins.
        let s = parse("daily at 02:30").unwrap();
        let next = s.next_after(at(2026, 1, 15, 12, 0));
        assert!(next.is_some());
        assert!(next.unwrap() > at(2026, 1, 15, 12, 0));
    }

    #[test]
    fn cron_fields_expand() {
        let s = parse("cron */15 9-11 * * mon,fri").unwrap();
        let Schedule::Cron(c) = &s else { panic!("not a cron") };
        assert_eq!(c.minute, CronField::Set(vec![0, 15, 30, 45]));
        assert_eq!(c.hour, CronField::Set(vec![9, 10, 11]));
        assert_eq!(c.dom, CronField::Any);
        assert_eq!(c.dow, CronField::Set(vec![1, 5]));
    }

    #[test]
    fn cron_finds_the_next_weekday_morning() {
        // 2026-03-10 is a Tuesday.
        let s = parse("cron 0 9 * * 1-5").unwrap();
        let next = s.next_after(at(2026, 3, 10, 9, 0)).unwrap();
        assert_eq!(next, at(2026, 3, 11, 9, 0));
        // Friday evening rolls to Monday.
        let next = s.next_after(at(2026, 3, 13, 18, 0)).unwrap();
        assert_eq!(next, at(2026, 3, 16, 9, 0));
        assert_eq!(next.weekday(), Weekday::Mon);
    }

    #[test]
    fn an_impossible_cron_reports_no_next_rather_than_spinning() {
        let s = parse("cron 0 0 30 2 *").unwrap();
        assert!(s.next_after(at(2026, 3, 10, 9, 0)).is_none());
    }

    #[test]
    fn cron_ors_day_of_month_with_day_of_week() {
        let s = parse("cron 0 0 1 * mon").unwrap();
        let Schedule::Cron(c) = &s else { panic!("not a cron") };
        // The 1st of a month that is not a Monday still matches.
        assert!(c.day_matches(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()));
        // And any Monday matches even when it is not the 1st.
        assert!(c.day_matches(NaiveDate::from_ymd_opt(2026, 3, 9).unwrap()));
        assert!(!c.day_matches(NaiveDate::from_ymd_opt(2026, 3, 10).unwrap()));
    }

    #[test]
    fn run_ids_sort_chronologically() {
        let early = new_run_id(1_700_000_000);
        let late = new_run_id(1_800_000_000);
        assert!(early < late);
        assert_eq!(early.len(), late.len());
    }

    #[test]
    fn relative_reads_both_directions() {
        let now = 1_800_000_000;
        assert_eq!(relative(now, now), "now");
        assert_eq!(relative(now - 30, now), "30s ago");
        assert_eq!(relative(now - 300, now), "5m ago");
        assert_eq!(relative(now + 7200, now), "in 2h");
        assert_eq!(relative(now + 172_800, now), "in 2d");
    }

    #[test]
    fn find_matches_name_then_id_then_prefix() {
        let mut a = Automation::new("Nightly".into(), "every 1h".into());
        a.id = "auto-12345678".into();
        let mut b = Automation::new("other".into(), "every 1h".into());
        b.id = "auto-abcdef01".into();
        let list = vec![a.clone(), b];
        assert_eq!(find(&list, "nightly").unwrap().id, a.id);
        assert_eq!(find(&list, "auto-12345678").unwrap().id, a.id);
        assert_eq!(find(&list, "auto-1234").unwrap().id, a.id);
        // Ambiguous prefixes match nothing.
        assert!(find(&list, "auto-").is_none());
        assert!(find(&list, "nope").is_none());
    }
}

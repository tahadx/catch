use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{
    Duration, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike,
};
use serde::Serialize;

use crate::state::{max_parallel, now_secs, with_lock, Status};

// Two independent rules share this module:
//
// 1. The recurring daily window (shell.json: schedEnabled/schedStart/
//    schedStop) says *when the queue may start work at all*.
// 2. A per-entry `scheduled_at` (Unix seconds) says *when that one entry
//    becomes eligible*. It is a one-time delay, not a second window:
//    once its time has arrived the entry obeys the daily window, the
//    parallel limit and every other queue rule like any other row.
//
// Both are enforced in the backend only: `drain` filters with
// `eligible()`, `tick` parks/releases scheduler-owned transfers, and the
// panel reads the resulting state instead of re-deriving it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub start_min: u16,
    pub stop_min: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Config {
    /// No daily window: downloads may run any time.
    Off,
    On(Schedule),
    /// Enabled but the saved times are unusable. Treated as "open" so a
    /// typo cannot silently freeze the whole queue; the panel reports it.
    Invalid(String),
}

impl Schedule {
    /// Equal start and end means "the whole day", not "a zero-length
    /// window": enabling the window then constrains nothing, which is the
    /// only reading that cannot surprise a user with a download that never
    /// starts. The UI says so in the summary.
    pub fn always_open(&self) -> bool {
        self.start_min == self.stop_min
    }

    pub fn is_open_at(&self, now_min: u16) -> bool {
        in_window(now_min, self.start_min, self.stop_min)
    }

    pub fn summary(&self) -> String {
        if self.always_open() {
            "any time".to_string()
        } else if self.start_min > self.stop_min {
            format!(
                "{}–{} (overnight)",
                hhmm(self.start_min),
                hhmm(self.stop_min)
            )
        } else {
            format!("{}–{}", hhmm(self.start_min), hhmm(self.stop_min))
        }
    }

    /// Minutes until the window next opens or closes, and which one.
    /// `None` for an always-open window. Boundaries are minute-aligned and
    /// start-inclusive / stop-exclusive, so the returned wait is >= 1.
    pub fn next_flip(&self, now_min: u16) -> Option<(bool, u16)> {
        if self.always_open() {
            return None;
        }
        let open = self.is_open_at(now_min);
        let target = if open { self.stop_min } else { self.start_min };
        let minutes = if target > now_min {
            target - now_min
        } else {
            target + 1440 - now_min
        };
        Some((!open, minutes))
    }
}

fn hhmm(min: u16) -> String {
    format!("{:02}:{:02}", min / 60, min % 60)
}

/// Minute-of-day arithmetic shared by the backend and its tests.
/// Start is inclusive, stop is exclusive; an equal pair is the whole day.
pub fn in_window(now: u16, start: u16, stop: u16) -> bool {
    if start == stop {
        return true;
    }
    if start < stop {
        now >= start && now < stop
    } else {
        now >= start || now < stop
    }
}

fn now_minutes() -> u16 {
    let now = Local::now();
    (now.hour() * 60 + now.minute()) as u16
}

fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let h: u16 = h.parse().ok()?;
    let m: u16 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

fn widget_settings() -> Option<serde_json::Value> {
    let home = std::env::var_os("HOME")?;
    let shell = PathBuf::from(home).join(".config/omarchy/shell.json");
    let text = std::fs::read_to_string(&shell).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        // A layout section is often absent; a missing "left" must not hide
        // the widget entry under "right".
        let Some(entries) = layout.get(section).and_then(|v| v.as_array()) else {
            continue;
        };
        for e in entries {
            if e.get("id").and_then(|v| v.as_str()) == Some("io.github.tahadx.catch") {
                return Some(e.clone());
            }
        }
    }
    None
}

/// The saved daily-window configuration, including *why* it is unusable
/// when it is. `read_schedule()` stays the cheap Option-shaped accessor
/// used by the drainer.
pub fn read_config() -> Config {
    let Some(e) = widget_settings() else {
        return Config::Off;
    };
    if e.get("schedEnabled")
        .and_then(|v| v.as_str())
        .unwrap_or("off")
        != "on"
    {
        return Config::Off;
    }
    let start = e.get("schedStart").and_then(|v| v.as_str()).unwrap_or("");
    let stop = e.get("schedStop").and_then(|v| v.as_str()).unwrap_or("");
    match (parse_hhmm(start), parse_hhmm(stop)) {
        (Some(start_min), Some(stop_min)) => Config::On(Schedule {
            start_min,
            stop_min,
        }),
        _ => Config::Invalid(format!(
            "window times must be HH:MM (saved {start:?}–{stop:?})"
        )),
    }
}

pub fn read_schedule() -> Option<Schedule> {
    match read_config() {
        Config::On(s) => Some(s),
        _ => None,
    }
}

/// None when the daily window is off or invalid (do not block work);
/// Some(true) when it is shut and new work must wait.
pub fn window_closed() -> Option<bool> {
    let sched = read_schedule()?;
    Some(!sched.is_open_at(now_minutes()))
}

/// Can this entry be started by a drain *because of timing*? Concurrency,
/// status and the window are checked by the drainer around this; this is
/// the one-time rule that a manual drain must not bypass either.
pub fn one_time_ready(d: &crate::state::Download, now: u64) -> bool {
    d.scheduled_at == 0 || d.scheduled_at <= now
}

/// One scheduler pass, run every minute by the systemd timer and on demand
/// from the panel. Inside the window it releases scheduler-owned pauses;
/// outside it parks transfers the scheduler started (never a manual one).
pub fn tick() -> Result<()> {
    let config = read_config();
    let open = match &config {
        Config::On(s) => s.is_open_at(now_minutes()),
        _ => true,
    };
    let stats = with_lock(|| {
        let mut state = crate::state::load_state()?;
        let stats = apply_window(&mut state, open);
        if stats.released > 0 || stats.parked > 0 {
            crate::state::save_state(&state)?;
        }
        Ok(stats)
    })?;
    if stats.released > 0 {
        eprintln!("catch: window open, released {stats}")
    }
    if stats.parked > 0 {
        eprintln!("catch: window closed, parked {stats}")
    }
    crate::drain::drain(max_parallel())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    pub released: usize,
    pub parked: usize,
}

impl std::fmt::Display for TickStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.parked, self.released) {
            (n, 0) => write!(f, "{n} download(s)"),
            (0, n) => write!(f, "{n} parked download(s)"),
            (p, r) => write!(f, "{p} parked and {r} released download(s)"),
        }
    }
}

/// Apply the window to queue state. Split out from `tick` so the exact
/// rules are unit-testable without a clock or systemd:
///
/// - open: only rows *the scheduler parked* resume. A manual pause has
///   `sched_hold == false` and is never touched.
/// - closed: only a live transfer the drainer started is parked. A manual
///   `catch download` is left alone, as is a user stop/cancel.
pub fn apply_window(state: &mut crate::state::State, open: bool) -> TickStats {
    let mut stats = TickStats::default();
    for d in state.downloads.iter_mut() {
        if open {
            if d.sched_hold && matches!(d.status, Status::Paused | Status::Queued) {
                if d.status == Status::Paused {
                    d.status = Status::Queued;
                }
                d.sched_hold = false;
                d.sched_owned = false;
                stats.released += 1;
            }
        } else if d.status == Status::Downloading && d.sched_owned {
            d.status = Status::Paused;
            d.sched_hold = true;
            d.sched_owned = false;
            stats.parked += 1;
        }
    }
    stats
}

// --------------------------------------------------------------- one-time

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    At(u64),
    Now,
    Clear,
}

/// Schedule, reschedule, start now, or cancel the one-time delay on one
/// queue entry. The daily window is intentionally not consulted here:
/// `Now` only makes the entry due, and the drainer still enforces the
/// window plus the parallel limit.
pub fn plan(id: &str, action: PlanAction) -> Result<String> {
    let mut message = String::new();
    crate::state::update_state(|state| {
        let d = state
            .downloads
            .iter_mut()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown download id {id}"))?;
        match action {
            PlanAction::At(ts) => {
                if d.status == Status::Downloading {
                    bail!("{id} is transferring; stop it before rescheduling");
                }
                if matches!(d.status, Status::Done | Status::Cancelled) {
                    bail!("{id} is {}; re-queue it before scheduling", d.status);
                }
                d.scheduled_at = ts;
                d.status = Status::Queued;
                d.sched_hold = false;
                d.sched_owned = false;
                d.error.clear();
                d.next_retry_at = 0;
                d.pid = 0;
                message = format!("{} starts {}", label(d), format_local(ts));
            }
            PlanAction::Now => {
                if d.status == Status::Downloading {
                    // The transfer is already live: just consume the delay,
                    // never rewrite the status under a running worker.
                    d.scheduled_at = 0;
                    message = format!("{} is already transferring", label(d));
                    return Ok(());
                }
                if matches!(d.status, Status::Done | Status::Cancelled) {
                    bail!("{id} is {}; re-queue it before starting", d.status);
                }
                d.scheduled_at = 0;
                d.status = Status::Queued;
                d.sched_hold = false;
                d.sched_owned = false;
                d.error.clear();
                d.next_retry_at = 0;
                d.pid = 0;
                message = format!("{} starts now", label(d));
            }
            PlanAction::Clear => {
                d.scheduled_at = 0;
                d.sched_hold = false;
                // Deliberately keep the status: cancelling a delay must not
                // silently resume a row the user had paused.
                message = format!("{} no longer has a one-time schedule", label(d));
            }
        }
        Ok(())
    })?;
    // A due/now row should not wait for the minute timer.
    crate::state::spawn_drain();
    Ok(message)
}

fn label(d: &crate::state::Download) -> String {
    if !d.title.trim().is_empty() {
        d.title.clone()
    } else if !d.filename.trim().is_empty() {
        d.filename.clone()
    } else {
        d.id.clone()
    }
}

/// Attach a one-time time and/or destination to a freshly enqueued row.
pub fn configure_row(id: &str, at: Option<u64>, dir: Option<&str>) -> Result<()> {
    crate::state::update_state(|state| {
        let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) else {
            return Ok(());
        };
        if let Some(ts) = at {
            d.scheduled_at = ts;
        }
        if let Some(raw) = dir {
            let trimmed = raw.trim();
            d.dir = if trimmed.is_empty() {
                String::new()
            } else {
                // Stored with the row, so it must not depend on the CWD of a
                // later worker: expand `~` and resolve a relative folder
                // against the configured download folder.
                let resolved = crate::state::resolve_download_dir(Some(trimmed));
                let absolute = if resolved.is_absolute() {
                    resolved
                } else {
                    crate::state::resolve_download_dir(None).join(resolved)
                };
                absolute.to_string_lossy().into_owned()
            };
        }
        Ok(())
    })
}

fn local_stamp(naive: NaiveDateTime) -> Result<i64> {
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Ok(dt.timestamp()),
        // Ambiguous (clocks go back): the earlier occurrence is the
        // conservative choice — the download is due as soon as the user
        // asked, never an hour later.
        LocalResult::Ambiguous(early, _late) => Ok(early.timestamp()),
        LocalResult::None => bail!(
            "{} does not exist in local time (daylight-saving gap)",
            naive.format("%Y-%m-%d %H:%M")
        ),
    }
}

/// Parse a user-facing one-time schedule:
/// `HH:MM` (next occurrence), `YYYY-MM-DD HH:MM` / `YYYY-MM-DDTHH:MM`
/// (local), `+30m` / `+2h` / `+1d`, or 10+ digits of Unix seconds.
/// Past absolute times are rejected explicitly.
pub fn parse_when(raw: &str, now: chrono::DateTime<Local>) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("a time is required: HH:MM, YYYY-MM-DD HH:MM, +30m, or Unix seconds");
    }
    if let Some(rest) = raw.strip_prefix('+') {
        let (number, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = number
            .trim()
            .parse()
            .with_context(|| format!("invalid relative time {raw:?}"))?;
        if n <= 0 {
            bail!("relative time must be positive: {raw}");
        }
        let seconds = match unit {
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86400,
            _ => bail!("relative time ends in m, h, or d: {raw}"),
        };
        return Ok((now + Duration::seconds(seconds)).timestamp() as u64);
    }
    if raw.len() >= 10 && raw.chars().all(|c| c.is_ascii_digit()) {
        let ts: i64 = raw.parse().context("invalid Unix timestamp")?;
        if ts <= now.timestamp() {
            bail!("that time is in the past ({})", format_local(ts as u64));
        }
        return Ok(ts as u64);
    }
    if let Some(min) = parse_hhmm(raw) {
        let h = (min / 60) as u32;
        let m = (min % 60) as u32;
        let today = now.date_naive().and_hms_opt(h, m, 0).context("bad time")?;
        let mut ts = local_stamp(today)?;
        if ts <= now.timestamp() {
            let tomorrow = (now.date_naive() + Duration::days(1))
                .and_hms_opt(h, m, 0)
                .context("bad time")?;
            ts = local_stamp(tomorrow)?;
        }
        return Ok(ts as u64);
    }
    let normalized = raw.replace('T', " ");
    let (date, time) = normalized
        .split_once(' ')
        .with_context(|| format!("cannot read {raw:?}: use HH:MM or YYYY-MM-DD HH:MM"))?;
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .with_context(|| format!("invalid date {date:?}"))?;
    let time = NaiveTime::parse_from_str(time.trim(), "%H:%M")
        .with_context(|| format!("invalid time {time:?}"))?;
    let ts = local_stamp(NaiveDateTime::new(date, time))?;
    if ts <= now.timestamp() {
        bail!("that time is in the past ({})", format_local(ts as u64));
    }
    Ok(ts as u64)
}

pub fn format_local(ts: u64) -> String {
    match Local.timestamp_opt(ts as i64, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => ts.to_string(),
    }
}

// -------------------------------------------------------------- status

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub enabled: bool,
    pub valid: bool,
    pub error: String,
    pub start: String,
    pub stop: String,
    pub policy: String,
    pub open: bool,
    pub now: u64,
    pub next_transition: Option<u64>,
    pub waiting_window: usize,
    pub waiting_time: usize,
    pub paused_by_scheduler: usize,
    pub next_scheduled: Option<u64>,
    pub timer_available: bool,
    pub timer_enabled: bool,
    pub timer_active: bool,
    pub message: String,
}

pub fn status_report() -> Result<StatusReport> {
    let config = read_config();
    let now = now_secs();
    let now_min = now_minutes();
    let (enabled, valid, error, start, stop, policy, open, next_transition) = match &config {
        Config::Off => (
            false,
            true,
            String::new(),
            "00:00".to_string(),
            "00:00".to_string(),
            "Any time".to_string(),
            true,
            None,
        ),
        Config::Invalid(why) => (
            true,
            false,
            why.clone(),
            "00:00".to_string(),
            "00:00".to_string(),
            "Invalid".to_string(),
            true,
            None,
        ),
        Config::On(s) => (
            true,
            true,
            String::new(),
            hhmm(s.start_min),
            hhmm(s.stop_min),
            format!("Daily window {}", s.summary()),
            s.is_open_at(now_min),
            next_transition_ts(Local::now(), s),
        ),
    };
    let state = crate::state::load_state()?;
    let mut waiting_window = 0;
    let mut waiting_time = 0;
    let mut paused_by_scheduler = 0;
    let mut next_scheduled: Option<u64> = None;
    for d in &state.downloads {
        if matches!(d.status, Status::Queued | Status::Paused) && d.scheduled_at > now {
            waiting_time += 1;
            next_scheduled = Some(next_scheduled.map_or(d.scheduled_at, |n| n.min(d.scheduled_at)));
        }
        if d.status == Status::Queued
            && enabled
            && valid
            && !open
            && (d.scheduled_at == 0 || d.scheduled_at <= now)
        {
            waiting_window += 1;
        }
        if d.status == Status::Paused && d.sched_hold {
            paused_by_scheduler += 1;
        }
    }
    let timer = timer_state(&systemctl_path());
    let message = status_message(
        &config,
        open,
        waiting_window,
        waiting_time,
        next_transition,
        next_scheduled,
        &timer,
    );
    let message = if paused_by_scheduler > 0 {
        format!("{message} · {paused_by_scheduler} download(s) parked by the scheduler")
    } else {
        message
    };
    Ok(StatusReport {
        enabled,
        valid,
        error,
        start,
        stop,
        policy,
        open,
        now,
        next_transition,
        waiting_window,
        waiting_time,
        paused_by_scheduler,
        next_scheduled,
        timer_available: timer.available,
        timer_enabled: timer.enabled,
        timer_active: timer.active,
        message,
    })
}

fn status_message(
    config: &Config,
    open: bool,
    waiting_window: usize,
    waiting_time: usize,
    next_transition: Option<u64>,
    next_scheduled: Option<u64>,
    timer: &TimerState,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match config {
        Config::Invalid(why) => parts.push(format!("Schedule settings are invalid: {why}")),
        Config::Off => parts.push("Daily window off — downloads may run any time".to_string()),
        Config::On(s) if open => {
            let mut line = "Downloads allowed now".to_string();
            if let Some(ts) = next_transition {
                line.push_str(&format!(" · window closes at {}", clock_of(ts)));
            }
            if s.start_min == s.stop_min {
                line = "Downloads allowed any time · window is set to the whole day".to_string();
            }
            parts.push(line);
        }
        Config::On(_) => {
            let when = next_transition
                .map(clock_of)
                .map(|c| format!("opens at {c}"))
                .unwrap_or_else(|| "is closed".to_string());
            parts.push(format!("Daily window {when}"));
        }
    }
    if waiting_window > 0 {
        parts.push(format!(
            "{waiting_window} download(s) waiting for the window"
        ));
    }
    if waiting_time > 0 {
        let next = next_scheduled
            .map(|ts| format!(" · next at {}", format_local(ts)))
            .unwrap_or_default();
        parts.push(format!("{waiting_time} scheduled download(s){next}"));
    }
    if (config_is_on(config) || waiting_time > 0) && !timer.active {
        parts.push("scheduler timer is not active".to_string());
    }
    parts.join(" · ")
}

fn config_is_on(config: &Config) -> bool {
    matches!(config, Config::On(_))
}

fn clock_of(ts: u64) -> String {
    match Local.timestamp_opt(ts as i64, 0).single() {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => String::new(),
    }
}

fn next_transition_ts(now: chrono::DateTime<Local>, s: &Schedule) -> Option<u64> {
    let now_min = (now.hour() * 60 + now.minute()) as u16;
    let (_, minutes) = s.next_flip(now_min)?;
    let base = now.with_second(0)?.with_nanosecond(0)?;
    Some((base + Duration::minutes(minutes as i64)).timestamp() as u64)
}

/// Human-readable scheduler status (the panel asks for `--json`).
pub fn status() -> Result<()> {
    let r = status_report()?;
    println!("{}", r.policy);
    println!(
        "window: {}",
        if !r.enabled {
            "off".to_string()
        } else if !r.valid {
            format!("invalid ({})", r.error)
        } else if r.open {
            "open".to_string()
        } else {
            "closed".to_string()
        }
    );
    println!("{}", r.message);
    println!(
        "timer: {}",
        if !r.timer_available {
            "systemctl --user unavailable".to_string()
        } else if r.timer_active {
            "active".to_string()
        } else if r.timer_enabled {
            "enabled but inactive".to_string()
        } else {
            "not installed".to_string()
        }
    );
    Ok(())
}

// --------------------------------------------------------------- systemd

const SERVICE: &str = r#"[Unit]
Description=Catch download scheduler pass

[Service]
Type=oneshot
# A pass exits as soon as it has spawned workers, and those workers have to
# outlive it. Under the default control-group kill they share this unit's
# cgroup and die the moment the pass ends, leaving the row at "downloading"
# with no process behind it until the reaper requeues it a minute later.
KillMode=process
ExecStart=%h/.local/bin/catch tick
"#;

const TIMER: &str = r#"[Unit]
Description=Catch download scheduler (checks every minute)

[Timer]
OnCalendar=*-*-* *:*:00
Persistent=true
AccuracySec=1s
Unit=catch-schedule.service

[Install]
WantedBy=timers.target
"#;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TimerState {
    pub available: bool,
    pub enabled: bool,
    pub active: bool,
}

fn systemctl_path() -> PathBuf {
    std::env::var_os("CATCH_SYSTEMCTL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("systemctl"))
}

fn unit_dir() -> Result<PathBuf> {
    // systemd's user manager reads units from $XDG_CONFIG_HOME/systemd/user
    // (defaulting to ~/.config). Writing to ~/.config when the session moved
    // its config home would install units the manager never loads.
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("systemd/user"));
        }
    }
    Ok(PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".config/systemd/user"))
}

fn systemctl(program: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {} {}", program.display(), args.join(" ")))
}

fn must_succeed(program: &Path, args: &[&str], what: &str) -> Result<std::process::Output> {
    let out = systemctl(program, args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", out.status)
        };
        bail!("{what} failed: {detail}");
    }
    Ok(out)
}

pub fn timer_state(program: &Path) -> TimerState {
    let enabled = match systemctl(program, &["--user", "is-enabled", "catch-schedule.timer"]) {
        Ok(out) => out.status.success(),
        Err(_) => return TimerState::default(),
    };
    let active = match systemctl(program, &["--user", "is-active", "catch-schedule.timer"]) {
        Ok(out) => out.status.success(),
        Err(_) => return TimerState::default(),
    };
    TimerState {
        available: true,
        enabled,
        active,
    }
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Install the per-user timer and verify it actually runs. The unit files
/// are written first, then every systemctl step is checked; a failure is
/// returned with its stderr instead of being swallowed behind a success
/// message.
pub fn install() -> Result<()> {
    install_with(&systemctl_path(), &unit_dir()?)
}

fn install_with(program: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_if_changed(&dir.join("catch-schedule.service"), SERVICE)?;
    write_if_changed(&dir.join("catch-schedule.timer"), TIMER)?;
    must_succeed(
        program,
        &["--user", "daemon-reload"],
        "systemd daemon-reload",
    )?;
    must_succeed(
        program,
        &["--user", "enable", "--now", "catch-schedule.timer"],
        "enable the scheduler timer",
    )?;
    let state = timer_state(program);
    if !state.available {
        bail!("systemctl --user is unavailable; the scheduler timer could not be verified");
    }
    if !state.enabled || !state.active {
        bail!(
            "the scheduler timer did not become active (enabled={}, active={})",
            state.enabled,
            state.active
        );
    }
    Ok(())
}

/// Stop and remove the timer. Idempotent: a timer that is already gone is
/// success, and a removal that leaves it active is not.
pub fn remove() -> Result<()> {
    remove_with(&systemctl_path(), &unit_dir()?)
}

fn remove_with(program: &Path, dir: &Path) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    match systemctl(
        program,
        &["--user", "disable", "--now", "catch-schedule.timer"],
    ) {
        Ok(out) if !out.status.success() => {
            let before = timer_state(program);
            if before.active {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                problems.push(if stderr.is_empty() {
                    "disable --now failed".to_string()
                } else {
                    stderr
                });
            }
        }
        Ok(_) => {}
        Err(e) => problems.push(format!("{e:#}")),
    }
    let _ = std::fs::remove_file(dir.join("catch-schedule.service"));
    let _ = std::fs::remove_file(dir.join("catch-schedule.timer"));
    if let Err(e) = must_succeed(
        program,
        &["--user", "daemon-reload"],
        "systemd daemon-reload",
    ) {
        problems.push(format!("{e:#}"));
    }
    let after = timer_state(program);
    if after.active {
        problems.push("the scheduler timer is still active".to_string());
    }
    if !problems.is_empty() {
        bail!(
            "could not remove the scheduler timer: {}",
            problems.join("; ")
        );
    }
    Ok(())
}

/// Make sure the systemd timer is running. Called by the CLI when a
/// one-time schedule is created so a future job does not depend on the
/// panel being open. Returns an actionable error instead of claiming
/// success when this machine cannot run user timers.
pub fn ensure_timer() -> Result<()> {
    let program = systemctl_path();
    let dir = unit_dir()?;
    let state = timer_state(&program);
    // Active is not enough: a stale manager can report a loaded unit whose
    // files are gone. Both the units and a live timer must be present.
    if state.available
        && state.active
        && dir.join("catch-schedule.timer").is_file()
        && dir.join("catch-schedule.service").is_file()
    {
        return Ok(());
    }
    install_with(&program, &dir)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Download, DownloadKind};

    fn at(h: u32, m: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 6, 15, h, m, 0)
            .single()
            .expect("valid local time")
    }

    fn row(id: &str, status: Status) -> Download {
        Download {
            id: id.to_string(),
            url: "https://x.test/a".to_string(),
            filename: "a.bin".to_string(),
            status,
            bytes: 0,
            total: 0,
            error: String::new(),
            kind: DownloadKind::File,
            pid: 0,
            claimed_at: 0,
            sched_hold: false,
            scheduled_at: 0,
            sched_owned: false,
            dir: String::new(),
            audio_only: false,
            title: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            priority: 0,
            attempts: 0,
            next_retry_at: 0,
            speed: 0,
            avg_speed: 0,
            eta: 0,
            expected_sha256: String::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn time_of_day_is_strict_but_forgiving_about_one_digit_hours() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("2:05"), Some(125));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("12"), None);
        assert_eq!(parse_hhmm("12:5"), None);
        assert_eq!(parse_hhmm("aa:bb"), None);
    }

    #[test]
    fn daytime_window_is_start_inclusive_and_stop_exclusive() {
        assert!(!in_window(9 * 60 + 59, 10 * 60, 12 * 60));
        assert!(in_window(10 * 60, 10 * 60, 12 * 60));
        assert!(in_window(11 * 60, 10 * 60, 12 * 60));
        assert!(!in_window(12 * 60, 10 * 60, 12 * 60));
    }

    #[test]
    fn overnight_window_wraps_midnight() {
        let (start, stop) = (22 * 60, 6 * 60);
        assert!(in_window(22 * 60, start, stop));
        assert!(in_window(23 * 60, start, stop));
        assert!(in_window(0, start, stop));
        assert!(in_window(5 * 60 + 59, start, stop));
        assert!(!in_window(6 * 60, start, stop));
        assert!(!in_window(12 * 60, start, stop));
    }

    #[test]
    fn equal_times_mean_the_whole_day() {
        let s = Schedule {
            start_min: 0,
            stop_min: 0,
        };
        assert!(s.always_open());
        assert!(s.is_open_at(0));
        assert!(s.is_open_at(12 * 60));
        assert!(s.is_open_at(1439));
        assert_eq!(s.next_flip(12 * 60), None);
        assert_eq!(s.summary(), "any time");
    }

    #[test]
    fn next_flip_handles_both_window_shapes() {
        let day = Schedule {
            start_min: 10 * 60,
            stop_min: 12 * 60,
        };
        assert_eq!(day.next_flip(9 * 60), Some((true, 60)));
        assert_eq!(day.next_flip(11 * 60), Some((false, 60)));
        assert_eq!(day.next_flip(13 * 60), Some((true, 21 * 60)));
        let night = Schedule {
            start_min: 22 * 60,
            stop_min: 6 * 60,
        };
        assert_eq!(night.next_flip(23 * 60), Some((false, 7 * 60)));
        assert_eq!(night.next_flip(5 * 60), Some((false, 60)));
        assert_eq!(night.next_flip(12 * 60), Some((true, 10 * 60)));
    }

    #[test]
    fn relative_schedules_are_positive_and_understand_m_h_d() {
        let now = at(10, 0);
        assert_eq!(
            parse_when("+30m", now).unwrap(),
            at(10, 30).timestamp() as u64
        );
        assert_eq!(
            parse_when("+2h", now).unwrap(),
            at(12, 0).timestamp() as u64
        );
        assert_eq!(
            parse_when("+1d", now).unwrap(),
            at(10, 0).timestamp() as u64 + 86400
        );
        assert!(parse_when("+0m", now).is_err());
        assert!(parse_when("+2w", now).is_err());
        assert!(parse_when("+nope", now).is_err());
    }

    #[test]
    fn clock_times_roll_to_tomorrow_only_when_today_has_passed() {
        let now = at(10, 0);
        let later = parse_when("12:30", now).unwrap();
        assert_eq!(later, at(12, 30).timestamp() as u64);
        let earlier = parse_when("09:00", now).unwrap();
        assert_eq!(earlier, at(9, 0).timestamp() as u64 + 86400);
        let exactly = parse_when("10:00", now).unwrap();
        assert_eq!(exactly, at(10, 0).timestamp() as u64 + 86400);
    }

    #[test]
    fn absolute_dates_are_local_and_past_ones_are_refused() {
        let now = at(10, 0);
        assert_eq!(
            parse_when("2026-06-16 14:30", now).unwrap(),
            at(14, 30).timestamp() as u64 + 86400
        );
        assert_eq!(
            parse_when("2026-06-16T14:30", now).unwrap(),
            at(14, 30).timestamp() as u64 + 86400
        );
        assert!(parse_when("2026-06-15 09:59", now).is_err());
        assert!(parse_when("2026-06-15", now).is_err());
        assert!(parse_when("yesterday", now).is_err());
    }

    #[test]
    fn unix_seconds_are_accepted_but_must_be_future() {
        let now = at(10, 0);
        let future = at(11, 0).timestamp() as u64;
        assert_eq!(parse_when(&future.to_string(), now).unwrap(), future);
        let past = at(9, 0).timestamp() as u64;
        assert!(parse_when(&past.to_string(), now).is_err());
    }

    #[test]
    fn window_close_parks_only_scheduler_owned_transfers() {
        let mut state = crate::state::State {
            version: 1,
            downloads: vec![
                {
                    let mut d = row("sched", Status::Downloading);
                    d.sched_owned = true;
                    d
                },
                row("manual", Status::Downloading),
                row("stopped", Status::Cancelled),
                row("paused", Status::Paused),
                row("future", Status::Queued),
            ],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let stats = apply_window(&mut state, false);
        assert_eq!(stats.parked, 1);
        assert_eq!(state.downloads[0].status, Status::Paused);
        assert!(state.downloads[0].sched_hold);
        assert!(!state.downloads[0].sched_owned);
        assert_eq!(state.downloads[1].status, Status::Downloading);
        assert_eq!(state.downloads[2].status, Status::Cancelled);
        assert_eq!(state.downloads[3].status, Status::Paused);
        assert_eq!(state.downloads[4].status, Status::Queued);
    }

    #[test]
    fn window_open_releases_only_scheduler_pauses() {
        let mut state = crate::state::State {
            version: 1,
            downloads: vec![
                {
                    let mut d = row("parked", Status::Paused);
                    d.sched_hold = true;
                    d
                },
                row("manual-pause", Status::Paused),
                {
                    let mut d = row("held-queue", Status::Queued);
                    d.sched_hold = true;
                    d
                },
            ],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let stats = apply_window(&mut state, true);
        assert_eq!(stats.released, 2);
        assert_eq!(state.downloads[0].status, Status::Queued);
        assert!(!state.downloads[0].sched_hold);
        assert_eq!(state.downloads[1].status, Status::Paused);
        assert_eq!(state.downloads[2].status, Status::Queued);
        assert!(!state.downloads[2].sched_hold);
    }

    #[test]
    fn one_time_rule_never_starts_early() {
        let now = 1_800_000_000;
        let mut d = row("s", Status::Queued);
        d.scheduled_at = now + 3600;
        assert!(!one_time_ready(&d, now));
        d.scheduled_at = now;
        assert!(one_time_ready(&d, now));
        d.scheduled_at = 0;
        assert!(one_time_ready(&d, now));
    }

    #[test]
    fn status_message_names_the_next_window_and_waiting_counts() {
        let cfg = Config::On(Schedule {
            start_min: 2 * 60,
            stop_min: 6 * 60,
        });
        let timer = TimerState {
            available: true,
            enabled: true,
            active: true,
        };
        let msg = status_message(&cfg, false, 3, 1, None, None, &timer);
        assert!(msg.contains("Daily window is closed"), "{msg}");
        assert!(
            msg.contains("3 download(s) waiting for the window"),
            "{msg}"
        );
        assert!(msg.contains("1 scheduled download(s)"), "{msg}");
        let open = status_message(&cfg, true, 0, 0, None, None, &timer);
        assert!(open.contains("Downloads allowed now"), "{open}");
        let inactive = TimerState {
            available: true,
            enabled: false,
            active: false,
        };
        let msg = status_message(&cfg, true, 0, 0, None, None, &inactive);
        assert!(msg.contains("timer is not active"), "{msg}");
    }

    // ---- systemd lifecycle, with an injected systemctl -----------------

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("catch-sched-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_systemctl(dir: &Path, mode: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("systemctl");
        let log = dir.join("systemctl.log");
        let disabled = dir.join("disabled");
        // `mode` is "ok" or "fail-enable". is-enabled/is-active answer the
        // way a working user manager does (disable flips them to
        // disabled/inactive); the enable step can be made to fail to prove
        // installation does not lie about success.
        let enable = if mode == "fail-enable" {
            "echo 'Failed to enable unit: simulated' >&2; exit 1"
        } else {
            "rm -f \"$DISABLED\"; exit 0"
        };
        let script = format!(
            "#!/bin/sh\n\
             DISABLED='{}'\n\
             printf '%s\\n' \"$*\" >> '{}'\n\
             case \"$2\" in\n\
               is-enabled)\n\
                 if [ -f \"$DISABLED\" ]; then echo disabled; exit 1; fi\n\
                 echo enabled; exit 0;;\n\
               is-active)\n\
                 if [ -f \"$DISABLED\" ]; then echo inactive; exit 3; fi\n\
                 echo active; exit 0;;\n\
               enable)\n\
                 {}\n\
                 ;;\n\
               disable)\n\
                 touch \"$DISABLED\"; exit 0;;\n\
               daemon-reload) exit 0;;\n\
             esac\n\
             exit 0\n",
            disabled.display(),
            log.display(),
            enable
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn install_writes_units_and_verifies_the_timer() {
        let dir = temp_dir("install");
        let units = dir.join("units");
        let fake = fake_systemctl(&dir, "ok");
        install_with(&fake, &units).unwrap();
        assert!(units.join("catch-schedule.timer").is_file());
        assert!(units.join("catch-schedule.service").is_file());
        let log = std::fs::read_to_string(dir.join("systemctl.log")).unwrap();
        assert!(log.contains("--user daemon-reload"), "{log}");
        assert!(log.contains("enable --now catch-schedule.timer"), "{log}");
        assert!(log.contains("is-enabled catch-schedule.timer"), "{log}");
        assert!(log.contains("is-active catch-schedule.timer"), "{log}");
        // A pass spawns workers and exits; the default control-group kill
        // took them down with it and stranded the row at "downloading".
        let unit = std::fs::read_to_string(units.join("catch-schedule.service")).unwrap();
        assert!(unit.contains("KillMode=process"), "{unit}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_reports_systemctl_failure_instead_of_claiming_success() {
        let dir = temp_dir("fail");
        let units = dir.join("units");
        let fake = fake_systemctl(&dir, "fail-enable");
        let err = install_with(&fake, &units).expect_err("enable failure must surface");
        let text = format!("{err:#}");
        assert!(text.contains("simulated"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_is_idempotent_and_reports_a_timer_that_stays_active() {
        let dir = temp_dir("remove");
        let units = dir.join("units");
        std::fs::create_dir_all(&units).unwrap();
        let fake = fake_systemctl(&dir, "ok");
        remove_with(&fake, &units).unwrap();
        remove_with(&fake, &units).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

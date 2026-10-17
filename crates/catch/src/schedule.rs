use anyhow::{Context, Result};

use crate::state::{max_parallel, update_state, Status};

// NOT READY — not wired into the UI and not exercised by CI.
//
// The window maths, the systemd timer and the power action are all here and
// work, but nothing parks a transfer while the window is shut: `tick` only
// *starts* queued entries, it never holds them back, so the switch would be a
// promise Catch does not keep yet. The settings are kept out of
// manifest.json and the panel hides its controls (see the NOT READY notes
// there). Enable it by finishing the hold/resume half and dropping those.
//
// Overnight-friendly schedule read from the widget settings in shell.json:
// schedEnabled ("on"/"off"), schedStart/schedStop ("HH:MM", overnight wrap
// allowed), schedWhenDone ("nothing"/"suspend"/"shutdown").
pub struct Schedule {
    pub start_min: u16,
    pub stop_min: u16,
    pub when_done: String,
}

fn widget_settings() -> Option<serde_json::Value> {
    let home = std::env::var_os("HOME")?;
    let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
    let text = std::fs::read_to_string(&shell).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        for e in layout.get(section)?.as_array()? {
            if e.get("id")?.as_str()? == "io.github.tahadx.catch" {
                return Some(e.clone());
            }
        }
    }
    None
}

fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u16 = h.parse().ok()?;
    let m: u16 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

use chrono::{Datelike, Timelike};

fn now_minutes() -> u16 {
    let now = chrono::Local::now();
    (now.hour() * 60 + now.minute()) as u16
}

fn in_window(now: u16, start: u16, stop: u16) -> bool {
    if start == stop {
        return true;
    }
    if start < stop {
        now >= start && now < stop
    } else {
        now >= start || now < stop
    }
}

pub fn read_schedule() -> Option<Schedule> {
    let e = widget_settings()?;
    if e.get("schedEnabled")?.as_str()? != "on" {
        return None;
    }
    let start = parse_hhmm(e.get("schedStart")?.as_str()?)?;
    let stop = parse_hhmm(e.get("schedStop")?.as_str()?)?;
    let when_done = e
        .get("schedWhenDone")
        .and_then(|v| v.as_str())
        .unwrap_or("nothing")
        .to_string();
    Some(Schedule {
        start_min: start,
        stop_min: stop,
        when_done,
    })
}

fn today_marker() -> std::path::PathBuf {
    let day = chrono::Local::now().date_naive().num_days_from_ce();
    crate::state::state_dir().join(format!("done-{day}"))
}

/// One scheduler pass, run every minute by the systemd timer:
/// inside the window, release parked entries and drain; outside it,
/// park live transfers. When the queue runs dry out of hours, run the
/// configured when-done action once per day.
pub fn tick() -> Result<()> {
    let Some(sched) = read_schedule() else {
        // Scheduler off: plain drain (manual mode untouched).
        return crate::drain::drain(max_parallel());
    };
    let now = now_minutes();
    if in_window(now, sched.start_min, sched.stop_min) {
        let mut released = 0;
        update_state(|state| {
            for d in state.downloads.iter_mut() {
                if d.status == Status::Paused && d.sched_hold {
                    d.status = Status::Queued;
                    d.sched_hold = false;
                    released += 1;
                }
            }
            Ok(())
        })?;
        if released > 0 {
            eprintln!("catch: window open, released {released} parked download(s)");
        }
        // A fresh day resets the when-done marker.
        let _ = std::fs::remove_file(today_marker());
        return crate::drain::drain(max_parallel());
    }
    let mut parked = 0;
    update_state(|state| {
        for d in state.downloads.iter_mut() {
            if d.status == Status::Downloading {
                d.status = Status::Paused;
                d.sched_hold = true;
                parked += 1;
            }
        }
        Ok(())
    })?;
    if parked > 0 {
        eprintln!("catch: window closed, parked {parked} download(s)");
    }
    // Queue dry outside hours: fire the when-done action once per day.
    let state = crate::state::load_state()?;
    let busy = state
        .downloads
        .iter()
        .any(|d| matches!(d.status, Status::Downloading | Status::Queued));
    if !busy && sched.when_done != "nothing" && !today_marker().exists() {
        std::fs::create_dir_all(crate::state::state_dir())?;
        let _ = std::fs::write(today_marker(), "fired");
        eprintln!("catch: queue dry, running when-done: {}", sched.when_done);
        let target = match sched.when_done.as_str() {
            "suspend" => "suspend",
            "shutdown" => "poweroff",
            _ => return Ok(()),
        };
        let _ = std::process::Command::new("systemctl")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

const SERVICE: &str = r#"[Unit]
Description=Catch download scheduler pass

[Service]
Type=oneshot
ExecStart=%h/.local/bin/catch tick
"#;

const TIMER: &str = r#"[Unit]
Description=Catch download scheduler (every minute, wakes for window)

[Timer]
OnCalendar=*:0/1
Persistent=true
WakeSystem=true

[Install]
WantedBy=timers.target
"#;

pub fn install() -> Result<()> {
    let dir = std::path::PathBuf::from(std::env::var_os("HOME").context("no HOME")?)
        .join(".config/systemd/user");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("catch-schedule.service"), SERVICE)?;
    std::fs::write(dir.join("catch-schedule.timer"), TIMER)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "catch-schedule.timer"])
        .status();
    println!("scheduler timer installed (every minute, wakes system)");
    Ok(())
}

pub fn remove() -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "catch-schedule.timer"])
        .status();
    println!("scheduler timer removed");
    Ok(())
}

pub fn status() -> Result<()> {
    match read_schedule() {
        Some(s) => println!(
            "schedule on {:02}:{:02}–{:02}:{:02}, when-done: {}",
            s.start_min / 60,
            s.start_min % 60,
            s.stop_min / 60,
            s.stop_min % 60,
            s.when_done
        ),
        None => println!("schedule off (or invalid times)"),
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "catch-schedule.timer"])
        .status();
    Ok(())
}

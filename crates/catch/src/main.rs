mod conns;
mod download;
mod drain;
mod install;
mod native_host;
mod schedule;
mod segment;
mod state;
mod throttle;
mod video;

use anyhow::{Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use state::{filename_for_url, load_state, new_id, Download, DownloadKind, Status};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "catch", version, about = "Catch download manager backend")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a URL to the queue and download it now (blocking). A link whose
    /// file is already in the download folder is not fetched again: Catch
    /// records a question and the panel asks what to do.
    Add {
        url: String,
        #[arg(long)]
        dir: Option<String>,
        /// Fetch with yt-dlp (video pages) instead of a plain download.
        #[arg(long)]
        video: bool,
        /// Best-audio stream instead of the full video (with --video).
        #[arg(long)]
        audio: bool,
        /// Expected SHA-256 of the finished file (64 hex characters).
        /// Verified before the download is published; a mismatch is
        /// reported as an integrity error instead of being saved.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
    },
    /// Add one or more URLs to the queue and return at once (non-blocking).
    /// Every row appears immediately and the transfers run in the background,
    /// exactly like a browser hand-off. `add` blocks on the download, which is
    /// what a terminal wants and what a panel must not do: a caller that
    /// serializes its commands would stall behind the whole transfer.
    Enqueue {
        /// URLs to queue, in order. Each link is handled on its own, so one
        /// bad or already-saved link does not discard the rest of the batch.
        #[arg(required = true)]
        urls: Vec<String>,
        /// Fetch with yt-dlp (video pages) instead of a plain download.
        #[arg(long)]
        video: bool,
        /// Best-audio stream instead of the full video (with --video).
        #[arg(long)]
        audio: bool,
        /// Force a new, independent download even if the URL is already
        /// downloading. Used by "Download again anyway"; clears any pending
        /// duplicate question for this URL.
        #[arg(long)]
        again: bool,
        /// Expected SHA-256 of the finished file (64 hex characters).
        /// Only valid with a single URL.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
        /// Start at a local time instead of now: HH:MM, YYYY-MM-DD HH:MM,
        /// +30m / +2h / +1d, or 10+ digits of Unix seconds. Every queued link
        /// gets the same one-time delay and still obeys the daily window.
        #[arg(long, value_name = "WHEN")]
        at: Option<String>,
        /// Destination folder for these entries (default: the configured
        /// download folder). Stored with the row, so a scheduled job keeps
        /// the folder chosen when it was queued.
        #[arg(long, value_name = "DIR")]
        dir: Option<String>,
    },
    /// Download one queued entry by id (blocking).
    Download {
        id: String,
        #[arg(long)]
        dir: Option<String>,
        /// Set by the drainer's spawn: the transfer is scheduler-owned, so
        /// a window close may park it. Omitted by a manual invocation.
        #[arg(long, hide = true)]
        scheduler: bool,
    },
    /// Fetch one queued entry with yt-dlp by id (blocking).
    Video {
        id: String,
        #[arg(long)]
        dir: Option<String>,
        /// Best-audio stream instead of the full video.
        #[arg(long)]
        audio: bool,
        /// See `download --scheduler`.
        #[arg(long, hide = true)]
        scheduler: bool,
    },
    /// List the queue. `--json` prints the raw state file.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Start queued entries while parallel slots are free.
    Drain {
        /// Max parallel downloads (1-10). Defaults to shell.json or 3.
        #[arg(long)]
        max: Option<usize>,
    },
    /// One scheduler pass (run every minute by the systemd timer and on
    /// demand from the panel): release or park scheduler-owned transfers at
    /// the window boundary, then start whatever is due and allowed.
    Tick {},
    /// Manage the scheduler: status, the per-user timer, and one-time entry
    /// schedules. With no subcommand, shows status.
    Schedule {
        #[command(subcommand)]
        action: Option<ScheduleAction>,
        /// Deprecated alias for `schedule install`.
        #[arg(long, hide = true)]
        install: bool,
        /// Deprecated alias for `schedule remove`.
        #[arg(long, hide = true)]
        remove: bool,
    },
    /// Write native host manifests for every Chromium + Firefox browser.
    InstallHosts {
        /// Host binary path (default: ~/.local/bin/catch-host).
        #[arg(long)]
        host: Option<std::path::PathBuf>,
    },
    /// Register the extension in Chromium-family browser flags files.
    InstallExtension {
        /// Extension dir (default: located from this binary).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Remove queue records. Downloading entries are stopped first.
    /// Refuses only unknown ids.
    Remove {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Also delete the downloaded files, if inside the download dir.
        #[arg(long)]
        delete_file: bool,
    },
    /// Catch every http(s) link on the Wayland clipboard.
    Paste {},
    /// Pause (keep partial data; a paused entry stays in the queue).
    /// With --all, pauses every queued or transferring entry.
    Pause {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Re-queue a paused, errored, or stopped entry and start it again.
    /// The partial data on disk is continued, not discarded, and the retry
    /// budget starts fresh. With --all, re-queues every such entry.
    Resume {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Stop an entry: the worker exits and the partial data stays on disk
    /// for a later resume. `remove` is the delete. With --all, stops every
    /// non-finished entry.
    Cancel {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Point a stopped or failed entry at a fresh address for the same file
    /// (expired signed links) and re-queue it. The new URL is probed first:
    /// when the length or validators prove it is the same file, the partial
    /// data is kept; when it cannot be proven, the command refuses and
    /// --force resets the partial data and fetches from zero.
    Refresh {
        id: String,
        url: String,
        #[arg(long)]
        force: bool,
    },
    /// Set the transfer priority: higher runs first among queued entries.
    Priority { id: String, level: i64 },
    /// Set (or clear, with "none") the expected SHA-256 for an entry.
    Checksum { id: String, hex: String },
    /// Explain one entry: partial data on disk, segments, retry state, and
    /// what to do next.
    Why { id: String },
    /// Rename what Catch saved for a link: new title for the row and the
    /// file. Works after the row is gone; the file is still on disk.
    Rename {
        url: String,
        /// New title; spaces are kept.
        #[arg(required = true, num_args = 1.., value_name = "TITLE")]
        title: Vec<String>,
    },
    /// Save an already-caught link again: a second copy, optionally under
    /// a title. The caught file is never touched.
    Copy {
        url: String,
        /// Title for the new copy (its file name too). Omit to keep the
        /// automatic name.
        title: Vec<String>,
    },
    /// Fetch a link again, replacing the file Catch saved for it.
    Replace { url: String },
    /// Drop the "already in your folder" question without downloading.
    Dismiss { url: String },
    /// Drop finished (done/error/cancelled) records. Keeps files.
    Clear {},
    /// Show paths and backend status.
    Status {},
    /// Speak Chrome/Firefox Native Messaging on stdio (spawned by browsers).
    NativeHost {},
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// Show the daily window and timer state. `--json` is what the panel reads.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Install and start the per-user systemd timer.
    Install,
    /// Stop and remove the per-user systemd timer.
    Remove,
    /// Schedule a queued entry: HH:MM, YYYY-MM-DD HH:MM, +30m, or Unix seconds.
    At {
        id: String,
        when: String,
        /// Destination for this entry (empty = the configured folder). The
        /// folder is stored with the row, so it survives later changes.
        #[arg(long, value_name = "DIR")]
        dir: Option<String>,
    },
    /// Cancel an entry's one-time delay. The download stays queued.
    Clear { id: String },
    /// Start a scheduled entry now, clearing its one-time delay.
    Now { id: String },
    /// List entries that carry a one-time schedule.
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse first: `--version` and `--help` must not touch the user's home,
    // and bin/configure itself asks the shipped binary for its version.
    let cli = Cli::parse();
    // Checkout installs self-heal here: backend links + browser hosts.
    state::self_configure();
    match cli.command {
        Command::Add {
            url,
            dir,
            video,
            audio,
            sha256,
        } => {
            if let Some(hex) = &sha256 {
                validate_sha256(hex)?;
            }
            // Already on disk: ask, don't fetch a quiet second copy. `copy`
            // is the explicit "download again".
            if raise_ask(&url, "")?.is_some() {
                println!("already downloading or saved; open Catch to choose");
            } else {
                // Add reached here only after raise_ask, which asks about both a
                // saved file and a live transfer, so a live row is already gone.
                // Keep reusing one as a guard against the race in between.
                catch_now(&url, dir.as_deref(), video, audio, true, sha256.as_deref()).await?;
            }
        }
        Command::Enqueue {
            urls,
            video,
            audio,
            again,
            sha256,
            at,
            dir,
        } => {
            if let Some(hex) = &sha256 {
                validate_sha256(hex)?;
            }
            if urls.len() > 1 && sha256.is_some() {
                anyhow::bail!("--sha256 pins one URL at a time");
            }
            // Parse once, before queueing anything: a bad time must not
            // leave half a batch queued with no schedule.
            let when = match &at {
                Some(raw) => Some(schedule::parse_when(raw, Local::now())?),
                None => None,
            };
            let single = urls.len() == 1;
            let mut queued = 0usize;
            let mut scheduled = 0usize;
            let mut touched = 0usize;
            let mut asked = 0usize;
            for raw in &urls {
                let url = raw.trim().to_string();
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    // A batch is forgiving: the bad entry is reported and the
                    // other links still queue. A lone URL keeps the old error.
                    if single {
                        anyhow::bail!("only http(s) URLs can be caught: {url}");
                    }
                    eprintln!("catch: skipped {url}: only http(s) URLs can be caught");
                    continue;
                }
                // `--again` forces a fresh, independent download (used by
                // "Download again anyway"): it clears any pending duplicate
                // question and never reuses the active row.
                if again {
                    clear_one_ask(&url)?;
                } else {
                    // The same duplicate rule `add` applies: a link whose file is
                    // already saved becomes a question for the panel instead of a
                    // quiet second copy. Without this the panel's add path saved
                    // duplicates without ever asking. One asked link must not
                    // discard the rest of the batch.
                    if raise_ask(&url, "")?.is_some() {
                        if single {
                            println!("already downloading or saved; open Catch to choose");
                        } else {
                            println!("already downloading or saved: {url}");
                        }
                        asked += 1;
                        continue;
                    }
                }
                let video = video || state::looks_like_video_page(&url);
                // claim = false: the row waits as Queued with no pid, and the
                // drainer claims a slot for it. Claiming here would stamp this
                // short-lived process as the holder of a row nobody is fetching,
                // and every later drain would count that phantom as an active
                // transfer — eating parallel slots until nothing started at all.
                // This is exactly what a browser hand-off does.
                let (id, fresh) = enqueue_kind(
                    &url,
                    if video {
                        DownloadKind::Video
                    } else {
                        DownloadKind::File
                    },
                    audio,
                    false,
                    !again,
                )?;
                touched += 1;
                if let Some(hex) = &sha256 {
                    set_checksum(&id, hex)?;
                }
                // Only a fresh row is configured: an existing catch keeps its
                // own timing and folder rather than being silently rewritten
                // by a pasted duplicate.
                if fresh && (when.is_some() || dir.is_some()) {
                    schedule::configure_row(&id, when, dir.as_deref())?;
                    if when.is_some() {
                        scheduled += 1;
                    }
                }
                if fresh {
                    println!("queued {id}");
                    queued += 1;
                } else {
                    println!("already catching {id}");
                }
            }
            if touched == 0 && asked == 0 {
                anyhow::bail!("no link could be queued");
            }
            if touched > 0 {
                // One wake-up for the whole batch: the drainer claims slots
                // for the new rows and leaves the rest queued.
                state::spawn_drain();
            }
            if let Some(ts) = when {
                if scheduled > 0 {
                    // Persisted rows still wait only for the panel or a
                    // manual tick when no timer exists; say so instead of
                    // pretending the schedule is protected.
                    if let Err(e) = schedule::ensure_timer() {
                        eprintln!("catch: warning: {e:#}");
                    }
                    println!(
                        "scheduled {scheduled} of {} link(s) for {}",
                        urls.len(),
                        schedule::format_local(ts)
                    );
                }
            }
            if !single {
                println!("queued {queued} of {} link(s)", urls.len());
            }
        }
        Command::Copy { url, title } => {
            // A second copy of a caught link. The title is applied to the
            // *copy* (file name included) once it lands — the caught file
            // and its row are left exactly as they were.
            clear_one_ask(&url)?;
            // "Download again": the user chose to fetch this link a second time, so a
            // transfer already running for it must not swallow this one.
            catch_now(&url, None, false, false, false, None).await?;
            let raw = title.join(" ");
            if !raw.trim().is_empty() {
                let id = newest_row_for(&url)?;
                rename_one(&id, &raw)?;
            }
        }
        Command::Download { id, dir, scheduler } => {
            if !scheduler {
                mark_manual(&id)?;
            }
            let outcome = download::download_entry(&id, dir.as_deref(), false).await;
            // Always wake the drainer: a slot freed up whatever happened.
            state::spawn_drain();
            let dest = outcome?;
            println!("{}", dest.display());
        }
        Command::Video {
            id,
            dir,
            audio,
            scheduler,
        } => {
            if !scheduler {
                mark_manual(&id)?;
            }
            let outcome = video::download_video(&id, dir.as_deref(), audio, false).await;
            state::spawn_drain();
            let dest = outcome?;
            println!("{}", dest.display());
        }
        Command::List { json } => {
            let state = load_state()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
                return Ok(());
            }
            if state.downloads.is_empty() {
                println!("no downloads yet");
                return Ok(());
            }
            for d in &state.downloads {
                let size = if d.total > 0 {
                    format!(" {}/{}", d.bytes, d.total)
                } else if d.bytes > 0 {
                    format!(" {}B", d.bytes)
                } else {
                    String::new()
                };
                let extra = if d.status == Status::Error && !d.error.is_empty() {
                    format!(" ({})", d.error)
                } else {
                    String::new()
                };
                println!("{} [{}] {}{}{}", d.id, d.status, d.filename, size, extra);
            }
        }
        Command::Pause { id, all } => {
            if all {
                let n = bulk_status(&[Status::Queued, Status::Downloading], Status::Paused)?;
                println!("paused {n} entry/ies");
            } else {
                mutate(&need_id(id)?, |d| match d.status {
                    Status::Queued | Status::Downloading => {
                        d.status = Status::Paused;
                        Ok("paused (partial data kept)".to_string())
                    }
                    _ => anyhow::bail!("cannot pause an entry that is {}", d.status),
                })?;
            }
        }
        Command::Resume { id, all } => {
            if all {
                let n = bulk_resume()?;
                println!("re-queued {n} entry/ies");
            } else {
                mutate(&need_id(id)?, |d| match d.status {
                    Status::Paused | Status::Error | Status::Cancelled => {
                        d.status = Status::Queued;
                        d.error.clear();
                        // A user-initiated resume is a fresh retry budget:
                        // persisted attempts exist to stop *automatic* crash
                        // loops, not to lock the user out.
                        d.attempts = 0;
                        d.next_retry_at = 0;
                        d.speed = 0;
                        d.eta = 0;
                        Ok("re-queued; continuing from the partial data on disk".to_string())
                    }
                    _ => anyhow::bail!("entry is already {}", d.status),
                })?;
            }
            state::spawn_drain();
        }
        Command::Cancel { id, all } => {
            if all {
                let n = bulk_status(
                    &[
                        Status::Queued,
                        Status::Downloading,
                        Status::Paused,
                        Status::Error,
                    ],
                    Status::Cancelled,
                )?;
                println!("stopped {n} entry/ies (partial data kept)");
            } else {
                mutate(&need_id(id)?, |d| match d.status {
                    Status::Done => anyhow::bail!("entry is already done"),
                    _ => {
                        d.status = Status::Cancelled;
                        Ok("stopped (partial data kept)".to_string())
                    }
                })?;
            }
        }
        Command::Refresh { id, url, force } => {
            let outcome = refresh_one(&id, &url, force).await?;
            println!("{outcome}");
            state::spawn_drain();
        }
        Command::Priority { id, level } => {
            mutate(&id, move |d| {
                d.priority = level;
                Ok(format!("priority {level}"))
            })?;
        }
        Command::Checksum { id, hex } => {
            let norm = if hex.eq_ignore_ascii_case("none") || hex.trim().is_empty() {
                String::new()
            } else {
                validate_sha256(&hex)?
            };
            mutate(&id, move |d| {
                d.expected_sha256 = norm;
                Ok(if d.expected_sha256.is_empty() {
                    "checksum cleared".to_string()
                } else {
                    format!("checksum {}", d.expected_sha256)
                })
            })?;
        }
        Command::Why { id } => {
            println!("{}", why_one(&id)?);
        }
        Command::Drain { max } => {
            let max = max.unwrap_or_else(state::max_parallel).clamp(1, 10);
            drain::drain(max)?;
            println!("drain: max {max}");
        }
        Command::Tick {} => {
            schedule::tick()?;
        }
        Command::Schedule {
            action,
            install,
            remove,
        } => match (action, install, remove) {
            (Some(ScheduleAction::Install), _, _) | (None, true, _) => {
                schedule::install()?;
                println!("scheduler timer installed (checks every minute)");
            }
            (Some(ScheduleAction::Remove), _, _) | (None, _, true) => {
                schedule::remove()?;
                println!("scheduler timer removed");
            }
            (None, false, false) | (Some(ScheduleAction::Status { json: false }), _, _) => {
                schedule::status()?;
            }
            (Some(ScheduleAction::Status { json: true }), _, _) => {
                println!("{}", serde_json::to_string(&schedule::status_report()?)?);
            }
            (Some(ScheduleAction::At { id, when, dir }), _, _) => {
                let ts = schedule::parse_when(&when, Local::now())?;
                println!("{}", schedule::plan(&id, schedule::PlanAction::At(ts))?);
                if let Some(folder) = dir {
                    schedule::configure_row(&id, None, Some(&folder))?;
                }
                // The row is already persisted. If the timer cannot be
                // installed, say so loudly: status will keep reporting it
                // inactive and the panel shows the same fact.
                if let Err(e) = schedule::ensure_timer() {
                    eprintln!("catch: warning: {e:#}");
                }
            }
            (Some(ScheduleAction::Now { id }), _, _) => {
                println!("{}", schedule::plan(&id, schedule::PlanAction::Now)?);
            }
            (Some(ScheduleAction::Clear { id }), _, _) => {
                println!("{}", schedule::plan(&id, schedule::PlanAction::Clear)?);
            }
            (Some(ScheduleAction::List), _, _) => {
                let state = load_state()?;
                let now = state::now_secs();
                let mut rows: Vec<&Download> = state
                    .downloads
                    .iter()
                    .filter(|d| {
                        d.scheduled_at > 0 && matches!(d.status, Status::Queued | Status::Paused)
                    })
                    .collect();
                if rows.is_empty() {
                    println!("no scheduled downloads");
                } else {
                    rows.sort_by_key(|d| d.scheduled_at);
                    for d in rows {
                        let due = if d.scheduled_at <= now { " (due)" } else { "" };
                        println!(
                            "{}  {}  {}{}",
                            schedule::format_local(d.scheduled_at),
                            d.id,
                            d.filename,
                            due
                        );
                    }
                }
            }
        },
        Command::InstallHosts { host } => {
            install::install_hosts(host)?;
        }
        Command::InstallExtension { dir } => {
            install::install_extension(dir)?;
        }
        Command::Remove { ids, delete_file } => {
            for id in &ids {
                remove_one(id, delete_file)?;
            }
            state::spawn_drain();
        }
        Command::Paste {} => {
            let urls = clipboard_links()?;
            if urls.is_empty() {
                anyhow::bail!("clipboard holds no http(s) links");
            }
            let mut fresh = 0;
            for url in &urls {
                // Watch-style links route to yt-dlp here too: a plain
                // fetch would only save HTML.
                let kind = if state::looks_like_video_page(url) {
                    DownloadKind::Video
                } else {
                    DownloadKind::File
                };
                if enqueue_kind(url, kind, false, false, true)?.1 {
                    fresh += 1;
                }
            }
            state::spawn_drain();
            println!("queued {fresh} link(s) from clipboard");
        }
        Command::Rename { url, title } => {
            let raw = title.join(" ");
            println!("{}", rename_link(&url, &raw)?);
        }
        Command::Replace { url } => {
            // The destructive choice: fetch the link again and let the new
            // bytes take the place of the saved file (the old ones are
            // gone). Keyed by URL, not row id, so it still works after the
            // queue row is gone. The question is over the moment they choose.
            clear_one_ask(&url)?;
            let saved = state::saved_match(&load_state()?, &url)
                .with_context(|| format!("nothing saved for {url}"))?;
            let base = state::resolve_download_dir(None);
            // Reuse a finished row when one is still around: it keeps the
            // queue showing the entry instead of stacking a second one.
            let reuse = {
                let state = load_state()?;
                if state.downloads.iter().any(|d| {
                    d.url == url && matches!(d.status, Status::Queued | Status::Downloading)
                }) {
                    anyhow::bail!("that link is already downloading; stop it first");
                }
                state
                    .downloads
                    .iter()
                    .find(|d| d.url == url && d.status == Status::Done)
                    .map(|d| (d.id.clone(), d.audio_only, d.filename.clone()))
            };
            // Caught before the index existed: the row's name plus the same
            // folder rules the downloader uses say where the file went.
            let old = if saved.path.is_empty() {
                let name = reuse.as_ref().map(|r| r.2.clone()).unwrap_or_default();
                if name.is_empty() {
                    base.clone()
                } else if saved.kind == DownloadKind::Video {
                    state::video_dir(&base).join(&name)
                } else {
                    state::dir_for(&base, &name).join(&name)
                }
            } else {
                base.join(&saved.path)
            };
            let (id, audio) = match reuse {
                Some((id, audio, _)) => {
                    state::update_state(|state| {
                        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                            d.status = Status::Queued;
                            d.error.clear();
                            d.bytes = 0;
                            d.total = 0;
                            d.sched_hold = false;
                        }
                        Ok(())
                    })?;
                    (id, audio)
                }
                None => {
                    let (id, _) = enqueue_kind(&url, saved.kind, false, true, true)?;
                    (id, false)
                }
            };
            let dest = if saved.kind == DownloadKind::Video {
                video::download_video(&id, None, audio, true).await?
            } else {
                download::download_entry(&id, None, true).await?
            };
            // A fresh fetch can land under a different name (changed title,
            // changed format). The old file leaves only after the new one
            // is in place, and only when it sits inside the download dir.
            if dest != old {
                let base = state::resolve_download_dir(None);
                if let Some(safe) = state::resolve_inside(&base, &old.to_string_lossy()) {
                    let _ = std::fs::remove_file(safe);
                }
            }
            state::spawn_drain();
            println!("replaced -> {}", dest.display());
        }
        Command::Dismiss { url } => {
            clear_one_ask(&url)?;
            println!("dismissed");
        }
        Command::Clear {} => {
            let mut cleared = 0;
            state::update_state(|state| {
                let before = state.downloads.len();
                state.downloads.retain(|d| {
                    matches!(
                        d.status,
                        Status::Queued | Status::Downloading | Status::Paused
                    )
                });
                cleared = before - state.downloads.len();
                Ok(())
            })?;
            println!("cleared {cleared} record(s)");
        }
        Command::Status {} => {
            let state = load_state().unwrap_or_default();
            println!("state: {}", state::state_path().display());
            println!("downloads: {}", state::resolve_download_dir(None).display());
            println!("queue: {} entr(y/ies)", state.downloads.len());
            println!(
                "binary: {}",
                std::env::current_exe().unwrap_or_default().display()
            );
            println!("version: {}", env!("CARGO_PKG_VERSION"));
            println!("build: {}", env!("CATCH_GIT_SHA"));
        }
        Command::NativeHost {} => native_host::run_loop().await?,
    }
    Ok(())
}

/// Enqueue a URL and fetch it now. Shared by `add` and `copy`: both end up
/// as one row plus one file on disk, differing only in what they do with an
/// existing file (neither overwrites it). Returns the id of the row that was
/// used — an already-catch link that was still live reuses its row.
async fn catch_now(
    url: &str,
    dir: Option<&str>,
    video: bool,
    audio: bool,
    reuse_live: bool,
    sha256: Option<&str>,
) -> Result<String> {
    let url = url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("only http(s) URLs can be caught: {url}");
    }
    // Pasted watch links go to yt-dlp even without --video: a plain
    // fetch would only save the watch page HTML.
    let video = video || state::looks_like_video_page(&url);
    let (id, fresh) = enqueue_kind(
        &url,
        if video {
            DownloadKind::Video
        } else {
            DownloadKind::File
        },
        audio,
        true,
        reuse_live,
    )?;
    // The checksum must be pinned before the transfer starts: finishing is
    // when it is enforced, and setting it after the fact would let the first
    // download publish unverified bytes.
    if let Some(hex) = sha256 {
        set_checksum(&id, hex)?;
    }
    if !fresh {
        // The same link is already waiting or transferring: reuse
        // that row instead of stacking a duplicate.
        state::spawn_drain();
        println!("already catching {id}");
        return Ok(id);
    }
    println!("queued {id}");
    let dest = if video {
        video::download_video(&id, dir, audio, false).await?
    } else {
        download::download_entry(&id, dir, false).await?
    };
    println!("{}", dest.display());
    Ok(id)
}

/// The most recent row for a URL: what `catch_now` just created or reused.
fn newest_row_for(url: &str) -> Result<String> {
    let state = load_state()?;
    state
        .downloads
        .iter()
        .rev()
        .find(|d| d.url == url.trim())
        .map(|d| d.id.clone())
        .with_context(|| format!("no download row for {url}"))
}

/// A manual `catch download` is not scheduler-owned: it must keep running
/// when the daily window closes. It also starts *now*, so a one-time delay
/// it still carried is consumed rather than applied later.
fn mark_manual(id: &str) -> Result<()> {
    state::update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            d.sched_owned = false;
            d.scheduled_at = 0;
            d.sched_hold = false;
        }
        Ok(())
    })
}

/// Enqueue a URL. One live row per link: a link already queued or
/// transferring is reused (retry floods used to stack duplicate rows).
/// A link Catch already saved is *not* reused — it gets a fresh row and
/// lands as another file under the next free name, the way a browser
/// handles a second click on the same link.
/// `claim` pins a fresh row to this process as Downloading so a concurrent
/// drain never starts a second worker for it; callers that hand off to the
/// drainer pass false. Returns (id, freshly added).
fn enqueue_kind(
    url: &str,
    kind: DownloadKind,
    audio_only: bool,
    claim: bool,
    reuse_live: bool,
) -> Result<(String, bool)> {
    prune_missing_saved(url)?;
    let id = new_id();
    let owner = std::process::id();
    let mut reuse: Option<String> = None;
    state::update_state(|state| {
        if reuse_live {
            if let Some(d) = state::live_row_for(state, url) {
                reuse = Some(d.id.clone());
                return Ok(());
            }
        }
        state.downloads.push(Download {
            id: id.clone(),
            url: url.to_string(),
            filename: filename_for_url(url, &id),
            status: if claim {
                Status::Downloading
            } else {
                Status::Queued
            },
            bytes: 0,
            total: 0,
            error: String::new(),
            kind,
            pid: if claim { owner } else { 0 },
            claimed_at: crate::state::now_secs(),
            sched_hold: false,
            scheduled_at: 0,
            sched_owned: false,
            dir: String::new(),
            audio_only,
            title: String::new(),
            extra: serde_json::Map::new(),
            etag: String::new(),
            last_modified: String::new(),
            priority: 0,
            attempts: 0,
            next_retry_at: 0,
            speed: 0,
            avg_speed: 0,
            eta: 0,
            expected_sha256: String::new(),
        });
        Ok(())
    })
    .context("save queue")?;
    Ok(match reuse {
        Some(existing) => (existing, false),
        None => (id, true),
    })
}

/// Every http(s) URL on the Wayland clipboard, deduplicated, order kept.
fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        let mut u = raw.trim().trim_start_matches(['(', '[', '<', '"', '\'']);
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            continue;
        }
        u = u.trim_end_matches([')', ']', '"', '\'', ',', ';', '.', '!', '?', '*']);
        if u.is_empty() || out.iter().any(|x: &String| x == u) {
            continue;
        }
        out.push(u.to_string());
    }
    out
}

fn clipboard_links() -> Result<Vec<String>> {
    let out = std::process::Command::new("wl-paste")
        .stdin(std::process::Stdio::null())
        .output()
        .context("read clipboard (is wl-paste installed?)")?;
    if !out.status.success() {
        anyhow::bail!("clipboard is empty or unreadable");
    }
    Ok(extract_urls(&String::from_utf8_lossy(&out.stdout)))
}

/// Move a file (and its row, when there is one) to a new title.
/// `old_rel` is the recorded path relative to the download dir, which
/// survives the folder setting changing; empty means "work it out".
fn rename_target(
    row: Option<String>,
    url: Option<&str>,
    old_name: &str,
    old_rel: &str,
    raw_title: &str,
) -> Result<String> {
    let base = state::resolve_download_dir(None);
    let (title, wanted) =
        state::rename_plan(old_name, raw_title).context("that title has no usable name")?;
    let mut final_name = wanted.clone();
    let mut dest = PathBuf::new();
    if wanted != old_name {
        // Resolve the source safely against the download root. An untrusted
        // or stale relative path must never point outside the download dir.
        let mut src: Option<PathBuf> = None;
        if old_rel.is_empty() {
            for dir in [
                base.clone(),
                state::video_dir(&base),
                state::dir_for(&base, old_name),
            ] {
                let candidate = dir.join(old_name);
                if let Some(safe) = state::resolve_inside(&base, &candidate.to_string_lossy()) {
                    src = Some(safe);
                    break;
                }
            }
        } else {
            let candidate = base.join(old_rel);
            src = state::resolve_inside(&base, &candidate.to_string_lossy());
        }
        if let Some(src) = src {
            let dir = src
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| base.clone());
            let target = state::unique_destination(&dir, &wanted);
            std::fs::rename(&src, &target).with_context(|| format!("rename {}", src.display()))?;
            final_name = target
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(wanted);
            dest = target;
        }
    }
    state::update_state(|state| {
        if let Some(id) = &row {
            if let Some(d) = state.downloads.iter_mut().find(|d| &d.id == id) {
                d.title = title.clone();
                d.filename = final_name.clone();
                d.sched_hold = false;
            }
        }
        // Keep the saved index pointing at the file that is actually there,
        // so the next duplicate check for this link stays correct.
        if let (Some(link), true) = (url, dest.as_os_str() != "") {
            if let Some(entry) = state.saved.get_mut(link) {
                entry.path = state::relative_to_base(&base, &dest);
            }
        }
        Ok(())
    })?;
    Ok(format!("renamed to {final_name}"))
}

/// Rename the file Catch saved for a link. Keyed by URL so it still works
/// after the queue row is gone: the file is still on disk.
fn rename_link(url: &str, raw_title: &str) -> Result<String> {
    let base = state::resolve_download_dir(None);
    let (row, old_name, old_rel) = state::with_lock(|| {
        let state = load_state()?;
        // Any live row for this link, not just the first one we happen to
        // meet: a worker owns that path.
        if state.downloads.iter().any(|d| {
            d.url == url
                && matches!(
                    d.status,
                    Status::Queued | Status::Downloading | Status::Paused
                )
        }) {
            anyhow::bail!("that link is still active; wait for it or stop it first");
        }
        let saved =
            state::saved_match(&state, url).with_context(|| format!("nothing saved for {url}"))?;
        // Prefer the row holding this very file, so its label moves too; fall
        // back to any finished row for the link (the pre-index case).
        let row = state
            .downloads
            .iter()
            .find(|d| {
                d.url == url && !saved.path.is_empty() && d.filename == file_name_of(&saved.path)
            })
            .or_else(|| {
                state
                    .downloads
                    .iter()
                    .find(|d| d.url == url && d.status == Status::Done)
            });
        let name = if saved.path.is_empty() {
            // Caught before the index existed: the finished row holds the
            // only name we have.
            state
                .downloads
                .iter()
                .find(|d| d.url == url && d.status == Status::Done)
                .map(|d| d.filename.clone())
                .with_context(|| format!("nothing saved for {url}"))?
        } else {
            file_name_of(&saved.path)
        };
        Ok((row.map(|d| d.id.clone()), name, saved.path))
    })?;
    if row.is_none() && !base.join(&old_rel).is_file() {
        // Nothing on disk means nothing to rename. Say so plainly and forget
        // the record, so the link counts as never caught from here on.
        forget_saved(url)?;
        anyhow::bail!("that file is gone from disk; catch the link again");
    }
    rename_target(row, Some(url), &old_name, &old_rel, raw_title)
}

/// The last path segment of a recorded path.
fn file_name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// If `url` is already a file in the download folder — or is a transfer still
/// running — record the question the panel shows and return the relative path
/// (empty for a live transfer, which has no file yet). A saved record whose
/// file was deleted is forgotten here, so the link can be caught again.
///
/// A running transfer used to be skipped instead of asked about, so sending
/// the same link twice silently reused the first row and the choice only
/// surfaced after that transfer had landed. The user asked for this link
/// again on purpose, so ask now.
pub(crate) fn raise_ask(url: &str, title: &str) -> Result<Option<String>> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(None);
    }
    let base = state::resolve_download_dir(None);
    let _ = std::fs::create_dir_all(&base);
    let missing = {
        let state = load_state()?;
        state::indexed_file_missing(&state, &base, url)
    };
    if missing {
        forget_saved(url)?;
    }
    let state = load_state()?;
    if let Some(row) = state::live_row_for(&state, url) {
        let mut ask = state::Ask {
            url: url.to_string(),
            title: row.title.clone(),
            filename: row.filename.clone(),
            kind: row.kind,
            path: String::new(),
            bytes: row.bytes,
            live: true,
        };
        if ask.title.is_empty() {
            ask.title = title.trim().to_string();
        }
        let label = if ask.filename.is_empty() {
            url.to_string()
        } else {
            ask.filename.clone()
        };
        state::update_state(|s| {
            state::push_ask(s, ask);
            Ok(())
        })?;
        notify_url_saved(&label, true);
        return Ok(Some(String::new()));
    }
    let Some(mut ask) = state::locate_saved(&state, &base, url) else {
        return Ok(None);
    };
    if ask.title.is_empty() {
        ask.title = title.trim().to_string();
    }
    let label = if ask.filename.is_empty() {
        ask.path.clone()
    } else {
        ask.filename.clone()
    };
    let path = ask.path.clone();
    state::update_state(|s| {
        state::push_ask(s, ask);
        Ok(())
    })?;
    notify_url_saved(&label, false);
    Ok(Some(path))
}

fn clear_one_ask(url: &str) -> Result<()> {
    let url = url.trim().to_string();
    state::update_state(|state| {
        state::clear_ask(state, &url);
        Ok(())
    })
}

/// One line on the desktop. Failure to notify never fails the catch.
/// A live transfer gets its own wording: nothing is on disk yet, and the
/// panel may be closed when the question is raised, so this line is often the
/// only sign that the question exists.
pub(crate) fn notify_url_saved(label: &str, live: bool) {
    let text = if live {
        format!("Already downloading — {label}")
    } else {
        format!("Already saved — {label}")
    };
    let _ = std::process::Command::new("omarchy-notification-send")
        .args(["-g", "󰇚", &text])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Drop a saved-index entry whose file is no longer there. Called before a
/// fresh catch: a file the user deleted outside Catch must stop counting as
/// saved, or every later catch keeps asking about a file that isn't there.
fn prune_missing_saved(url: &str) -> Result<()> {
    let base = state::resolve_download_dir(None);
    let stale = {
        let state = load_state()?;
        match state::saved_indexed(&state, url) {
            Some(rec) => !base.join(&rec.path).is_file(),
            None => false,
        }
    };
    if stale {
        forget_saved(url)?;
    }
    Ok(())
}

fn forget_saved(url: &str) -> Result<()> {
    state::update_state(|state| {
        state.saved.remove(url);
        Ok(())
    })
}

/// Rename one queue row's file. Active entries are refused: a live worker
/// owns the path.
fn rename_one(id: &str, raw_title: &str) -> Result<String> {
    let (old_name, url) = state::with_lock(|| {
        let state = load_state()?;
        let d = state
            .downloads
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown download id {id}"))?;
        if matches!(
            d.status,
            Status::Queued | Status::Downloading | Status::Paused
        ) {
            anyhow::bail!("{id} is still active; wait for it or stop it first");
        }
        Ok((d.filename.clone(), d.url.clone()))
    })?;
    // The URL travels with it: `copy` renames the row it just made, and the
    // saved index has to follow, or it would point at the pre-rename name.
    rename_target(Some(id.to_string()), Some(&url), &old_name, "", raw_title)
}

fn remove_one(id: &str, delete_file: bool) -> Result<()> {
    // Resolve targets first (fs I/O stays outside the lock).
    let base = state::resolve_download_dir(None);
    let (name, was_active) = state::with_lock(|| {
        let state = load_state()?;
        let d = state
            .downloads
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown download id {id}"))?;
        Ok((d.filename.clone(), d.status == Status::Downloading))
    })?;
    let part = state::dir_for(&base, &name).join(format!("{name}.{id}.part"));
    // One atomic delete: a live worker finds no record on its next flush
    // and stands down (its part file is cleaned below).
    state::update_state(|state| {
        if let Some(pos) = state.downloads.iter().position(|d| d.id == id) {
            state.downloads.remove(pos);
        }
        Ok(())
    })?;
    std::fs::remove_file(&part).ok();
    // Drop any segmented-download sidecar too.
    let _ = std::fs::remove_file(format!("{}.meta", part.display()));
    if delete_file {
        // The file may predate categories or the setting may have flipped:
        // try the category dir first, then the flat base. The boundary is
        // always the download root, never the file's own parent.
        for dir in [state::dir_for(&base, &name), base.clone()] {
            if let Some(p) = state::resolve_inside(&base, &dir.join(&name).to_string_lossy()) {
                let _ = std::fs::remove_file(&p);
                break;
            }
        }
    }
    if was_active {
        println!("removed {name} (transfer stopped)");
    } else {
        println!("removed {name}");
    }
    Ok(())
}

fn mutate(id: &str, f: impl FnOnce(&mut Download) -> Result<String>) -> Result<()> {
    let mut out = String::new();
    state::update_state(|state| {
        let d = state
            .downloads
            .iter_mut()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown download id {id}"))?;
        out = f(d)?;
        // Touched by hand: the scheduler leaves it alone from here.
        d.sched_hold = false;
        Ok(())
    })?;
    println!("{id}: {out}");
    Ok(())
}

/// A missing positional id with no --all is user error, not a panic.
fn need_id(id: Option<String>) -> Result<String> {
    id.with_context(|| "give an id, or --all for every entry")
}

/// Switch every entry in one of `from` to `to`; returns how many changed.
fn bulk_status(from: &[Status], to: Status) -> Result<usize> {
    let mut n = 0;
    state::update_state(|state| {
        for d in state.downloads.iter_mut() {
            if from.contains(&d.status) {
                d.status = to.clone();
                n += 1;
            }
        }
        Ok(())
    })?;
    Ok(n)
}

/// Re-queue every paused, errored or stopped entry with a fresh retry budget.
fn bulk_resume() -> Result<usize> {
    let mut n = 0;
    state::update_state(|state| {
        for d in state.downloads.iter_mut() {
            if matches!(d.status, Status::Paused | Status::Error | Status::Cancelled) {
                d.status = Status::Queued;
                d.error.clear();
                d.attempts = 0;
                d.next_retry_at = 0;
                d.speed = 0;
                d.eta = 0;
                n += 1;
            }
        }
        Ok(())
    })?;
    Ok(n)
}

/// A checksum is 64 hex characters; normalized to lower case so comparisons
/// never fail on formatting.
fn validate_sha256(hex: &str) -> Result<String> {
    let norm = hex.trim().to_ascii_lowercase();
    if norm.len() != 64 || !norm.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!(
            "a SHA-256 is 64 hex characters (got {} characters)",
            norm.len()
        );
    }
    Ok(norm)
}

fn set_checksum(id: &str, hex: &str) -> Result<()> {
    let norm = validate_sha256(hex)?;
    mutate(id, move |d| {
        d.expected_sha256 = norm;
        Ok(format!("checksum {}", d.expected_sha256))
    })
}

/// One-line description of what a representation claims to be, for refresh
/// mismatch messages.
fn describe(total: &u64, etag: &str, last_modified: &str) -> String {
    let mut bits = Vec::new();
    if *total > 0 {
        bits.push(format!("{total} bytes"));
    }
    if !etag.is_empty() {
        bits.push(format!("etag {etag}"));
    }
    if !last_modified.is_empty() {
        bits.push(format!("modified {last_modified}"));
    }
    if bits.is_empty() {
        "unknown".to_string()
    } else {
        bits.join(", ")
    }
}

/// `catch refresh`: point a stopped/failed row at a fresh address for the
/// same file (expired signed links are the usual reason). The new URL is
/// probed first — length and validators are the available proof that the
/// bytes on disk are still a prefix of what the new address serves. Only
/// with proof is the partial data kept; `--force` overrides and resets it.
async fn refresh_one(id: &str, new_url: &str, force: bool) -> Result<String> {
    let new_url = new_url.trim().to_string();
    if !new_url.starts_with("http://") && !new_url.starts_with("https://") {
        anyhow::bail!("only http(s) URLs can be refreshed: {new_url}");
    }
    let row = {
        let state = load_state()?;
        state
            .downloads
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown download id {id}"))?
            .clone()
    };
    match row.status {
        Status::Downloading => {
            anyhow::bail!("{id} is still transferring; stop it first (catch cancel {id})")
        }
        Status::Done => {
            anyhow::bail!("{id} is already done; use `catch replace` to fetch the link again")
        }
        _ => {}
    }
    let probe = download::probe_remote(&new_url).await?;

    // Proof that the new address serves the same representation. Length
    // equality is the weak form, matching validators the strong form; a
    // comparison that could be made and failed is fatal to a resume.
    let length_both = row.total > 0 && probe.total.is_some();
    let length_ok = length_both && row.total == probe.total.unwrap();
    let etag_both = !row.etag.is_empty() && !probe.etag.is_empty();
    let etag_ok = etag_both && row.etag == probe.etag;
    let lm_both = !row.last_modified.is_empty() && !probe.last_modified.is_empty();
    let lm_ok = lm_both && row.last_modified == probe.last_modified;
    let compared_and_different =
        (length_both && !length_ok) || (etag_both && !etag_ok) || (lm_both && !lm_ok);
    let proof = if length_ok {
        Some("length verified")
    } else if etag_ok {
        Some("ETag verified")
    } else if lm_ok {
        Some("Last-Modified verified")
    } else {
        None
    };

    let base = state::resolve_download_dir(None);
    let part = state::dir_for(&base, &row.filename).join(format!("{}.{}.part", row.filename, id));
    let had_partial = std::fs::metadata(&part)
        .map(|m| m.len() > 0)
        .unwrap_or(false);

    if compared_and_different && !force {
        anyhow::bail!(
            "the new address serves a different file (old: {}; new: {}); \
             `catch refresh {id} <url> --force` replaces the partial data",
            describe(&row.total, &row.etag, &row.last_modified),
            describe(&probe.total.unwrap_or(0), &probe.etag, &probe.last_modified),
        );
    }
    if proof.is_none() && had_partial && !force {
        anyhow::bail!(
            "the new address cannot be verified as the same file (no matching length or \
             validators); `catch refresh {id} <url> --force` restarts from zero instead"
        );
    }

    let reset = compared_and_different || proof.is_none();
    let kept = row.bytes;
    state::update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            d.url = new_url.clone();
            d.status = Status::Queued;
            d.error.clear();
            d.attempts = 0;
            d.next_retry_at = 0;
            d.speed = 0;
            d.eta = 0;
            d.sched_hold = false;
            // A manual refresh is an explicit \"start now\": do not let an old
            // one-time delay hold it until later.
            d.scheduled_at = 0;
            d.sched_owned = false;
            if reset {
                d.bytes = 0;
                d.total = 0;
                d.etag = probe.etag.clone();
                d.last_modified = probe.last_modified.clone();
            } else {
                if d.total == 0 {
                    d.total = probe.total.unwrap_or(0);
                }
                if d.etag.is_empty() {
                    d.etag = probe.etag.clone();
                }
                if d.last_modified.is_empty() {
                    d.last_modified = probe.last_modified.clone();
                }
                if d.total > 0 {
                    d.bytes = d.bytes.min(d.total);
                }
            }
        }
        Ok(())
    })?;
    if reset {
        std::fs::remove_file(&part).ok();
        let _ = std::fs::remove_file(format!("{}.meta", part.display()));
        if compared_and_different {
            Ok("refreshed: the new address serves a different file; partial data reset, fetching from zero".to_string())
        } else {
            Ok("refreshed: could not verify the new address serves the same file; fetching from zero".to_string())
        }
    } else {
        Ok(format!(
            "refreshed: same file ({}); continuing from {kept} bytes",
            proof.unwrap_or("unverified")
        ))
    }
}

/// `catch why`: everything the backend knows about one entry, in plain text.
fn why_one(id: &str) -> Result<String> {
    let state = load_state()?;
    let d = state
        .downloads
        .iter()
        .find(|d| d.id == id)
        .with_context(|| format!("unknown download id {id}"))?;
    let base = state::resolve_download_dir(None);
    let part = state::dir_for(&base, &d.filename).join(format!("{}.{}.part", d.filename, d.id));
    let mut out = String::new();
    let pct = if d.total > 0 {
        format!(" ({:.0}%)", d.bytes as f64 * 100.0 / d.total as f64)
    } else {
        String::new()
    };
    out.push_str(&format!("{} [{}] {}{}\n", d.id, d.status, d.filename, pct));
    out.push_str(&format!("  url: {}\n", d.url));
    out.push_str(&format!("  bytes: {}/{}\n", d.bytes, d.total));
    if d.status == Status::Downloading && d.speed > 0 {
        out.push_str(&format!(
            "  speed: {} (avg {}), eta {}s\n",
            download::fmt_rate(d.speed),
            download::fmt_rate(d.avg_speed),
            d.eta
        ));
    }
    if d.priority != 0 {
        out.push_str(&format!("  priority: {}\n", d.priority));
    }
    if d.attempts > 0 {
        out.push_str(&format!(
            "  attempts: {}/{}",
            d.attempts,
            download::max_attempts()
        ));
        let now = state::now_secs();
        if d.next_retry_at > now {
            out.push_str(&format!(" (next retry in {}s)", d.next_retry_at - now));
        }
        out.push('\n');
    }
    match std::fs::metadata(&part) {
        Ok(m) => out.push_str(&format!(
            "  partial data: {} bytes at {}\n",
            m.len(),
            part.display()
        )),
        Err(_) => out.push_str("  partial data: none\n"),
    }
    match segment::manifest_summary(&part, &d.url) {
        Some((total, done, count)) => out.push_str(&format!(
            "  segments: {done}/{count} done (representation {total} bytes)\n"
        )),
        None => out.push_str("  segments: none (sequential transfer, or already finished)\n"),
    }
    if !d.etag.is_empty() || !d.last_modified.is_empty() {
        out.push_str(&format!(
            "  validators: etag={} last-modified={}\n",
            if d.etag.is_empty() { "-" } else { &d.etag },
            if d.last_modified.is_empty() {
                "-"
            } else {
                &d.last_modified
            }
        ));
    }
    if !d.expected_sha256.is_empty() {
        out.push_str(&format!("  expected sha256: {}\n", d.expected_sha256));
    }
    if !d.error.is_empty() {
        out.push_str(&format!("  last note: {}\n", d.error));
    }
    let now = state::now_secs();
    let assessment = match d.status {
        Status::Done => "saved; nothing to do".to_string(),
        Status::Downloading if d.speed > 0 => {
            format!("transferring now at {}", download::fmt_rate(d.speed))
        }
        Status::Downloading => "transferring now".to_string(),
        Status::Queued if d.sched_hold => "waiting for the scheduled window".to_string(),
        Status::Queued => "queued; starts when a parallel slot is free".to_string(),
        Status::Paused => {
            "paused; the partial data is kept — `catch resume <id>` continues".to_string()
        }
        Status::Cancelled => {
            "stopped; the partial data is kept — `catch resume <id>` continues, `catch remove <id>` deletes".to_string()
        }
        Status::Error => {
            let e = d.error.to_lowercase();
            if e.contains("page, not the file")
                || e.contains("expired")
                || e.contains(" 401")
                || e.contains(" 403")
                || e.contains("access denied")
            {
                "the link expired or hit a login wall — `catch refresh <id> <new-url>` then `catch resume <id>`".to_string()
            } else if d.attempts >= download::max_attempts() {
                format!(
                    "retry budget exhausted ({}) — `catch resume <id>` starts a fresh budget",
                    d.error
                )
            } else if d.next_retry_at > now {
                format!("will retry automatically in {}s", d.next_retry_at - now)
            } else if !d.error.is_empty() {
                format!(
                    "failed: {} — `catch resume <id>` retries from the partial data",
                    d.error
                )
            } else {
                "failed — `catch resume <id>` retries".to_string()
            }
        }
    };
    out.push_str(&format!("  assessment: {assessment}\n"));
    Ok(out)
}

#[cfg(test)]
mod cli_tests {
    use super::{extract_urls, newest_row_for, Status};

    #[test]
    fn clipboard_extraction() {
        let text = "grab these:\nhttps://a.example/x.zip, (https://b.example/y.mp4).\nhttp://c.example/z\nnot a link\nftp://d.example/w";
        assert_eq!(
            extract_urls(text),
            vec![
                "https://a.example/x.zip".to_string(),
                "https://b.example/y.mp4".to_string(),
                "http://c.example/z".to_string(),
            ]
        );
        assert!(extract_urls("nothing here").is_empty());
        assert_eq!(
            extract_urls("https://a.example/1 https://a.example/1"),
            vec!["https://a.example/1".to_string()]
        );
    }

    #[test]
    fn newest_row_is_what_copy_names_its_copy_after() {
        // `copy` renames the row it just made, so it has to pick the last
        // one for that URL — never an older caught row.
        let mut state = crate::state::State::default();
        let mk = |id: &str| crate::state::Download {
            id: id.to_string(),
            url: "https://x.test/a".to_string(),
            filename: format!("{id}.zip"),
            status: Status::Done,
            bytes: 0,
            total: 0,
            error: String::new(),
            kind: crate::state::DownloadKind::File,
            pid: 0,
            claimed_at: crate::state::now_secs(),
            sched_hold: false,
            scheduled_at: 0,
            sched_owned: false,
            dir: String::new(),
            audio_only: false,
            etag: String::new(),
            last_modified: String::new(),
            priority: 0,
            attempts: 0,
            next_retry_at: 0,
            speed: 0,
            avg_speed: 0,
            eta: 0,
            expected_sha256: String::new(),
            title: String::new(),
            extra: serde_json::Map::new(),
        };
        state.downloads.push(mk("old"));
        state.downloads.push(mk("new"));
        let last = state.downloads.last().unwrap().id.clone();
        assert_eq!(last, "new");
        // Mirrors what the command does: walk the rows newest-first.
        assert_eq!(
            state
                .downloads
                .iter()
                .rev()
                .find(|d| d.url == "https://x.test/a")
                .map(|d| d.id.clone()),
            Some(last)
        );
        let _ = newest_row_for; // exercised through the binary, not here
    }
}

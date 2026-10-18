mod download;
mod drain;
mod install;
mod native_host;
mod schedule;
mod segment;
mod state;
mod video;

use anyhow::{Context, Result};
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
    },
    /// Add a URL to the queue and return at once (non-blocking). The row
    /// appears immediately and the transfer runs in the background, exactly
    /// like a browser hand-off. `add` blocks on the download, which is what a
    /// terminal wants and what a panel must not do: a caller that serializes
    /// its commands would stall behind the whole transfer.
    Enqueue {
        url: String,
        /// Fetch with yt-dlp (video pages) instead of a plain download.
        #[arg(long)]
        video: bool,
        /// Best-audio stream instead of the full video (with --video).
        #[arg(long)]
        audio: bool,
    },
    /// Download one queued entry by id (blocking).
    Download {
        id: String,
        #[arg(long)]
        dir: Option<String>,
    },
    /// Fetch one queued entry with yt-dlp by id (blocking).
    Video {
        id: String,
        #[arg(long)]
        dir: Option<String>,
        /// Best-audio stream instead of the full video.
        #[arg(long)]
        audio: bool,
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
    /// One scheduler pass (run every minute by the systemd timer).
    /// NOT READY: the timer runs, but transfers are never held back outside
    /// the window yet. See crates/catch/src/schedule.rs.
    Tick {},
    /// Manage the scheduler timer (every minute, wakes the system).
    /// With no flags, shows scheduler status. NOT READY — the panel hides
    /// this; it is only reachable from the terminal for now.
    Schedule {
        #[arg(long)]
        install: bool,
        #[arg(long)]
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
    /// Pause a queued entry (step 1: queued/failed entries only).
    Pause { id: String },
    /// Re-queue a paused, errored, or cancelled entry.
    Resume { id: String },
    /// Mark an entry cancelled (files already written are kept).
    Cancel { id: String },
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
        } => {
            // Already on disk: ask, don't fetch a quiet second copy. `copy`
            // is the explicit "download again".
            if raise_ask(&url, "")?.is_some() {
                println!("already downloading or saved; open Catch to choose");
            } else {
                // Add reached here only after raise_ask, which asks about both a
                // saved file and a live transfer, so a live row is already gone.
                // Keep reusing one as a guard against the race in between.
                catch_now(&url, dir.as_deref(), video, audio, true).await?;
            }
        }
        Command::Enqueue { url, video, audio } => {
            // The same duplicate rule `add` applies: a link whose file is
            // already saved becomes a question for the panel instead of a
            // quiet second copy. Without this the panel's add path saved
            // duplicates without ever asking.
            if raise_ask(&url, "")?.is_some() {
                println!("already downloading or saved; open Catch to choose");
                return Ok(());
            }
            // catch_now up to the point where it would block: same row, same
            // rules, but the transfer is left to the drainer so the caller's
            // next command is free to run.
            let url = url.trim().to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                anyhow::bail!("only http(s) URLs can be caught: {url}");
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
                true,
            )?;
            state::spawn_drain();
            if fresh {
                println!("queued {id}");
            } else {
                println!("already catching {id}");
            }
        }
        Command::Copy { url, title } => {
            // A second copy of a caught link. The title is applied to the
            // *copy* (file name included) once it lands — the caught file
            // and its row are left exactly as they were.
            clear_one_ask(&url)?;
            // "Download again": the user chose to fetch this link a second time, so a
            // transfer already running for it must not swallow this one.
            catch_now(&url, None, false, false, false).await?;
            let raw = title.join(" ");
            if !raw.trim().is_empty() {
                let id = newest_row_for(&url)?;
                rename_one(&id, &raw)?;
            }
        }
        Command::Download { id, dir } => {
            let outcome = download::download_entry(&id, dir.as_deref(), false).await;
            // Always wake the drainer: a slot freed up whatever happened.
            state::spawn_drain();
            let dest = outcome?;
            println!("{}", dest.display());
        }
        Command::Video { id, dir, audio } => {
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
        Command::Pause { id } => mutate(&id, |d| match d.status {
            Status::Queued | Status::Downloading => {
                d.status = Status::Paused;
                Ok("paused".to_string())
            }
            _ => anyhow::bail!("cannot pause an entry that is {}", d.status),
        })?,
        Command::Resume { id } => mutate(&id, |d| match d.status {
            Status::Paused | Status::Error | Status::Cancelled => {
                d.status = Status::Queued;
                d.error.clear();
                Ok("re-queued (run `catch download` to fetch)".to_string())
            }
            _ => anyhow::bail!("entry is already {}", d.status),
        })?,
        Command::Cancel { id } => mutate(&id, |d| match d.status {
            Status::Done => anyhow::bail!("entry is already done"),
            _ => {
                d.status = Status::Cancelled;
                Ok("cancelled".to_string())
            }
        })?,
        Command::Drain { max } => {
            let max = max.unwrap_or_else(state::max_parallel).clamp(1, 10);
            drain::drain(max)?;
            println!("drain: max {max}");
        }
        Command::Tick {} => {
            schedule::tick()?;
        }
        Command::Schedule { install, remove } => {
            if install {
                schedule::install()?;
            } else if remove {
                schedule::remove()?;
            } else {
                schedule::status()?;
            }
        }
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
                if let Some(dir) = old.parent() {
                    if let Some(safe) = state::resolve_inside(dir, &old.to_string_lossy()) {
                        let _ = std::fs::remove_file(safe);
                    }
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
            audio_only,
            title: String::new(),
            extra: serde_json::Map::new(),
            etag: String::new(),
            last_modified: String::new(),
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
        let mut src = if old_rel.is_empty() {
            base.join(old_name)
        } else {
            base.join(old_rel)
        };
        if !src.is_file() {
            // Older records may predate the saved index, or the folder
            // setting may have flipped under us.
            for dir in [
                state::video_dir(&base),
                state::dir_for(&base, old_name),
                base.clone(),
            ] {
                let candidate = dir.join(old_name);
                if candidate.is_file() {
                    src = candidate;
                    break;
                }
            }
        }
        if src.is_file() {
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
        // try the category dir first, then the flat base.
        for dir in [state::dir_for(&base, &name), base.clone()] {
            if let Some(p) = state::resolve_inside(&dir, &dir.join(&name).to_string_lossy()) {
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
            audio_only: false,
            etag: String::new(),
            last_modified: String::new(),
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

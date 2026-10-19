use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

/// How often a live transfer re-reads stop orders even when yt-dlp is
/// silent. A quiet line of output must not delay "stop".
const STOP_POLL: Duration = Duration::from_millis(250);

/// How long yt-dlp may produce no output at all before the run is treated
/// as hung. yt-dlp has its own socket timeouts, but a wedged fragment
/// downloader can sit silent for ever; the row must not stay "downloading"
/// with no process behind it. Overridable for tests via
/// CATCH_VIDEO_IDLE_SECS.
fn idle_timeout() -> Duration {
    if let Ok(raw) = std::env::var("CATCH_VIDEO_IDLE_SECS") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if (1..=7200).contains(&n) {
                return Duration::from_secs(n);
            }
        }
    }
    Duration::from_secs(600)
}

/// Longest line accepted from a child process. yt-dlp progress lines are
/// short; anything longer is noise (or a hostile site title) and must not be
/// allowed to grow memory without bound.
const MAX_LINE: usize = 8 * 1024;
/// How much stderr is kept for error reporting (the last lines win).
const STDERR_KEEP_LINES: usize = 8;
const STDERR_MAX_BYTES: usize = 16 * 1024;

use crate::state::{
    clean_label, load_state, mark_error, resolve_download_dir, resolve_inside, sanitize_filename,
    unique_destination, update_state, video_dir, Status,
};

/// yt-dlp's own naming: the video title plus the real extension.
const DEFAULT_TEMPLATE: &str = "%(title)s.%(ext)s";

/// What one yt-dlp run left behind.
struct Run {
    /// The file that ended up in the folder: either the one this run
    /// downloaded, or the pre-existing target yt-dlp decided to skip.
    dest: PathBuf,
    /// True when bytes actually moved. yt-dlp skips a target that is
    /// already on disk (no ` (1)` suffixing like a browser), so a skipped
    /// run reports *someone else's* file here.
    downloaded: bool,
    /// Title from `before_dl`, which yt-dlp prints even on a skip.
    /// (The size estimate it sends alongside is written straight to the
    /// row, so it does not need to travel back here.)
    title: Option<String>,
}

/// Fetch one queued entry with yt-dlp, streaming `--newline` progress into
/// the state file the same way the plain downloader does. Best-effort
/// desktop feedback (notification) never fails the download itself.
/// `overwrite` is the explicit "replace" action: refetch even though the
/// file exists. Otherwise a caught video is never touched and a re-catch
/// lands as another copy.
pub async fn download_video(
    id: &str,
    dir_override: Option<&str>,
    audio_only: bool,
    overwrite: bool,
) -> Result<PathBuf> {
    let snapshot = load_state()?;
    let entry = snapshot
        .downloads
        .iter()
        .find(|d| d.id == id)
        .with_context(|| format!("unknown download id {id}"))?;
    if matches!(entry.status, Status::Done | Status::Cancelled) {
        anyhow::bail!("download {id} is already {}", entry.status);
    }
    let url = entry.url.clone();
    // A scheduled row keeps the folder chosen at enqueue time; an explicit
    // --dir still wins.
    let entry_dir = if entry.dir.trim().is_empty() {
        None
    } else {
        Some(entry.dir.as_str())
    };
    let base = resolve_download_dir(dir_override.or(entry_dir));
    std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;
    // Videos sort into the video folder when categories are on.
    let dir = video_dir(&base);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // Same cross-process transfer lock as a plain download: two yt-dlp runs
    // for one entry would race over the same output template and part files.
    let Some(_claim) = crate::state::claim_worker(id)? else {
        anyhow::bail!("{id} is already being downloaded by another catch process");
    };
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            let own = std::process::id();
            if d.status == Status::Downloading
                && crate::state::worker_holds(d.pid, id)
                && d.pid != own
            {
                anyhow::bail!("{id} is already held by worker {}", d.pid);
            }
            d.status = Status::Downloading;
            d.error.clear();
            d.pid = own;
            // See download.rs: the live transfer lock is the proof now; a
            // dead worker must not hold a slot for CLAIM_GRACE.
            d.claimed_at = 0;
        }
        Ok(())
    })?;

    let Some(mut run) = fetch(id, &url, &dir, audio_only, DEFAULT_TEMPLATE, overwrite).await?
    else {
        return Ok(dir);
    };
    if !run.downloaded {
        // The link was already on disk and yt-dlp skipped the transfer.
        // Catching it again means another file, not a second row pointing
        // at the old one: fetch once more under a free name ("Clip (1).webm").
        let template = free_copy_template(&dir, &run, &format!("video-{id}"));
        let Some(second) = fetch(id, &url, &dir, audio_only, &template, true).await? else {
            return Ok(dir);
        };
        run = second;
    }
    let dest = run.dest;
    if !dest.is_file() {
        let msg = "yt-dlp reported a file that is not on disk";
        let _ = mark_error(id, msg);
        anyhow::bail!("{msg}");
    }
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("video-{id}"));
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            if d.status == Status::Downloading {
                d.status = Status::Done;
                d.claimed_at = 0;
                d.pid = 0;
                d.error.clear();
                d.filename = clean_label(&name, &format!("video-{id}"));
                if d.title.is_empty() {
                    // Metadata never arrived (exotic site, old yt-dlp):
                    // the on-disk name is still better than "watch".
                    let stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    d.title = clean_label(&stem, &format!("video-{id}"));
                }
                // yt-dlp skips the download when the destination already
                // exists, so no CATCH_PROG lines arrive and bytes stays 0
                // while total keeps the CATCH_META estimate. Even on a real
                // run the last flushed totals belong to the final stream
                // only. Set both counters from the file that actually
                // landed on disk so the row never rests at "0 B / size".
                let final_size = std::fs::metadata(&dest)
                    .map(|m| m.len())
                    .unwrap_or_else(|_| d.bytes.max(d.total));
                d.bytes = final_size;
                d.total = final_size;
            }
        }
        // On disk whatever happened to the row, so remember the link: a
        // later catch must still be recognised once the queue is cleared.
        crate::state::record_saved(state, &url, &base, &dest, crate::state::DownloadKind::Video);
        Ok(())
    })?;
    eprintln!("catch: done {id} -> {}", dest.display());
    crate::state::spawn_drain();
    Ok(dest)
}

/// Run yt-dlp once, naming the file from `template`. `force` lets the run
/// write over its own target — only ever used with a name nothing else
/// uses. `Ok(None)` means a pause: the partial files stay for resume.
async fn fetch(
    id: &str,
    url: &str,
    dir: &Path,
    audio_only: bool,
    template: &str,
    force: bool,
) -> Result<Option<Run>> {
    // Names already in the folder before the run: together with the
    // progress lines below, that tells a real transfer from a skip.
    let before = existing_names(dir);

    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args([
        "--no-playlist",
        "--no-simulate",
        "--quiet",
        "--no-warnings",
        "--no-exec",
        "--no-exec-before-download",
        // DASH and HLS streams arrive as many small fragments, and yt-dlp
        // fetches them one at a time unless told otherwise. That is why a
        // Catch video came down slower than the same video in a browser.
        // Four at a time is the usual sweet spot: well past a single stream's
        // throughput, still far from tripping a rate limit.
        "--concurrent-fragments",
        "4",
        "--socket-timeout",
        "30",
        "--progress",
        "--newline",
        "--progress-template",
        "CATCH_PROG\t%(progress.downloaded_bytes)s\t%(progress.total_bytes)s",
    ]);
    if audio_only {
        // Best audio stream, no re-encode: fast and format-preserving.
        cmd.args(["--format", "bestaudio"]);
    }
    if force {
        // Only ever set for an explicit replace, or for the copy retry
        // whose target name was verified free by unique_destination.
        cmd.args(["--force-overwrites"]);
    }
    cmd.args([
        // Title and size arrive from this same extraction, before any
        // bytes move: the row shows the real title almost at once
        // instead of the URL tail, with no second yt-dlp race.
        "--print",
        "before_dl:CATCH_META\t%(title)j\t%(filesize_approx)s",
        "--paths",
        dir.to_str().unwrap_or("."),
        "-o",
        template,
        "--print",
        "after_move:CATCH_FILE\t%(filepath)s",
        "--",
        url,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    // Its own process group: FFmpeg runs as a child of yt-dlp, and a stop
    // has to reach the whole group. Killing only yt-dlp used to leave the
    // merge running.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| "spawn yt-dlp (is it installed?)")?;

    // Drain stderr *concurrently*. It used to be read only after the child
    // exited, so a chatty yt-dlp (or FFmpeg's merge output) filled the 64 KiB
    // pipe buffer and blocked for ever: the transfer deadlocked with the
    // child waiting on a reader that was waiting on the child.
    let stderr = child.stderr.take().context("capture yt-dlp stderr")?;
    let stderr_task = tokio::spawn(drain_stderr(stderr));
    let stdout = child.stdout.take().context("capture yt-dlp stdout")?;
    let mut lines = LineReader::new(tokio::io::BufReader::new(stdout), MAX_LINE);

    let read = read_run(id, dir, &mut lines).await;
    let (outcome, filepath, title, saw_progress, last_err) = match read {
        Ok(v) => v,
        Err(e) => {
            // Whatever went wrong while reading, the child must not be left
            // running in the background.
            stop_child(&mut child).await;
            let _ = stderr_task.await;
            return Err(e);
        }
    };

    // Cancel/kill/close before waiting, so no subprocess survives an early
    // return. A clean EOF needs no signal.
    let status = match outcome {
        RunOutcome::Eof => child.wait().await.context("wait yt-dlp")?,
        _ => stop_child(&mut child)
            .await
            .context("wait yt-dlp after stop")?,
    };
    let stderr_tail = tokio::time::timeout(Duration::from_secs(2), stderr_task)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    if matches!(outcome, RunOutcome::Stopped) {
        // A pause is not a failure: yt-dlp keeps its partial files, and the
        // row stays Paused so a later resume continues instead of showing an
        // error. `fetch` documents Ok(None) for exactly this.
        eprintln!("catch: {id} paused");
        return Ok(None);
    }
    if let RunOutcome::Idle(limit) = outcome {
        let msg = format!(
            "yt-dlp produced no output for {}s; stopped it (resume to retry)",
            limit.as_secs()
        );
        let _ = mark_error(id, &msg);
        anyhow::bail!("{msg}");
    }
    if !status.success() || filepath.is_none() {
        let msg = if !stderr_tail.is_empty() {
            stderr_tail
        } else if !last_err.is_empty() {
            format!("yt-dlp exited {status}: {last_err}")
        } else {
            format!("yt-dlp exited {status} with no file")
        };
        let _ = mark_error(id, &msg);
        anyhow::bail!("{msg}");
    }
    let dest = filepath.expect("checked above");
    let name = dest.file_name().map(|s| s.to_string_lossy().into_owned());
    let downloaded = was_downloaded(saw_progress, &before, name.as_deref());
    Ok(Some(Run {
        dest,
        downloaded,
        title,
    }))
}

enum RunOutcome {
    /// yt-dlp closed stdout on its own.
    Eof,
    /// A stop/pause order ended the run.
    Stopped,
    /// No output for longer than the idle timeout.
    Idle(Duration),
}

/// Read stdout lines until EOF or an order arrives. Everything that is not a
/// protocol line is stderr-worthy noise and capped.
#[allow(clippy::type_complexity)]
async fn read_run(
    id: &str,
    dir: &Path,
    lines: &mut LineReader<impl tokio::io::AsyncBufRead + Unpin>,
) -> Result<(RunOutcome, Option<PathBuf>, Option<String>, bool, String)> {
    let mut last_flush = Instant::now();
    let mut last_check = Instant::now();
    let mut last_output = Instant::now();
    let mut filepath: Option<PathBuf> = None;
    let mut last_err = String::new();
    let mut title: Option<String> = None;
    let mut saw_progress = false;

    loop {
        // yt-dlp can sit silent for minutes while a stalled connection
        // times out, so the read is bounded: a tick sends us round the loop
        // to re-read stop orders. Without it "stop" only landed when the
        // next progress line happened to arrive.
        let tick = tokio::time::timeout(STOP_POLL, lines.next_line()).await;
        let line = match tick {
            Err(_) => None,
            Ok(result) => match result? {
                Some(l) => Some(l),
                None => break,
            },
        };
        // Honor stop orders about once a second: pause and resume keep
        // yt-dlp's partial files (it continues by default); a missing
        // record counts as cancelled.
        if last_check.elapsed() >= Duration::from_secs(1) || line.is_none() {
            last_check = Instant::now();
            let order = load_state()
                .ok()
                .and_then(|s| {
                    s.downloads
                        .iter()
                        .find(|d| d.id == id)
                        .map(|d| d.status.clone())
                })
                .unwrap_or(Status::Cancelled);
            if order == Status::Paused {
                eprintln!("catch: {id} paused; partial files kept for resume");
                return Ok((RunOutcome::Stopped, filepath, title, saw_progress, last_err));
            } else if order != Status::Downloading {
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
        }
        if line.is_none() {
            // A poll tick carried no output. Only a *silent child* trips the
            // inactivity deadline; the loop above keeps checking orders.
            if last_output.elapsed() >= idle_timeout() {
                return Ok((
                    RunOutcome::Idle(idle_timeout()),
                    filepath,
                    title,
                    saw_progress,
                    last_err,
                ));
            }
            continue;
        }
        let line = line.unwrap_or_default();
        last_output = Instant::now();
        if let Some(rest) = line.strip_prefix("CATCH_PROG\t") {
            let mut parts = rest.split('\t');
            let bytes = parts.next().and_then(|s| s.parse::<u64>().ok());
            let total = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            // A skipped download still emits one line, with NA (or 0) for
            // the downloaded counter and no bytes actually moving. Only a
            // real byte count marks a transfer.
            if bytes.is_some_and(|b| b > 0) {
                saw_progress = true;
            }
            let bytes = bytes.unwrap_or(0);
            if last_flush.elapsed() >= Duration::from_millis(250) {
                last_flush = Instant::now();
                let _ = update_state(|state| {
                    if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                        if d.status == Status::Downloading {
                            d.bytes = bytes;
                            d.total = total;
                        }
                    }
                    Ok(())
                });
                eprintln!("catch: {id} {bytes}/{total}");
            }
        } else if let Some(rest) = line.strip_prefix("CATCH_META\t") {
            // Garbage-free metadata: the title arrives JSON-quoted so
            // tabs and newlines inside it can never break the line.
            let mut parts = rest.split('\t');
            let reported = parts
                .next()
                .and_then(|t| serde_json::from_str::<String>(t.trim()).ok())
                .filter(|s| !s.trim().is_empty());
            let estimate = parts
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|n| *n > 0);
            if reported.is_some() || estimate.is_some() {
                let _ = update_state(|state| {
                    if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                        if let Some(t) = &reported {
                            d.title = clean_label(t, &d.filename);
                        }
                        if d.total == 0 {
                            if let Some(n) = estimate {
                                d.total = n;
                            }
                        }
                    }
                    Ok(())
                });
            }
            title = reported;
        } else if let Some(rest) = line.strip_prefix("CATCH_FILE\t") {
            if let Some(p) = resolve_inside(dir, rest.trim()) {
                filepath = Some(p);
            }
        } else if !line.trim().is_empty() {
            last_err = crate::state::truncate_bytes(line.trim(), 200).to_string();
        }
    }
    Ok((RunOutcome::Eof, filepath, title, saw_progress, last_err))
}

/// Stop a child and everything in its process group: SIGTERM first (yt-dlp
/// finalizes its part files for resume), SIGKILL if it ignores us.
async fn stop_child(child: &mut tokio::process::Child) -> Option<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        signal_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        signal_group(pid, libc::SIGKILL);
    }
    child.wait().await.ok()
}

/// Signal a process group (negative pid). Safe no-op when the group is gone.
fn signal_group(pid: u32, signal: i32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: kill(2) with a negative pid addresses the process group this
    // child was placed in; no memory is touched and a vanished group is
    // reported as ESRCH, which is ignored.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

/// A line reader with a hard per-line cap. `BufReader::lines` grows without
/// bound, so a child that prints a 1 GiB line (or a page title with no
/// newline) could exhaust memory.
struct LineReader<R> {
    inner: R,
    limit: usize,
}

impl<R: tokio::io::AsyncBufRead + Unpin> LineReader<R> {
    fn new(inner: R, limit: usize) -> Self {
        LineReader { inner, limit }
    }

    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut out: Vec<u8> = Vec::new();
        let mut any = false;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if any {
                    Ok(Some(finish_line(&out)))
                } else {
                    Ok(None)
                };
            }
            any = true;
            let newline_at = available.iter().position(|&b| b == b'\n');
            let content = match newline_at {
                Some(i) => i,
                None => available.len(),
            };
            let room = self.limit.saturating_sub(out.len());
            let take = content.min(room);
            out.extend_from_slice(&available[..take]);
            let consumed = newline_at.map(|i| i + 1).unwrap_or(available.len());
            self.inner.consume(consumed);
            if newline_at.is_some() {
                return Ok(Some(finish_line(&out)));
            }
        }
    }
}

fn finish_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\r')
        .to_string()
}

/// Read a child's stderr to EOF, keeping only the last few lines (bounded).
async fn drain_stderr<R: tokio::io::AsyncRead + Unpin>(stderr: R) -> String {
    let mut lines = LineReader::new(tokio::io::BufReader::new(stderr), MAX_LINE);
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut held: usize = 0;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = crate::state::truncate_bytes(line.trim(), 4096).to_string();
                if line.is_empty() {
                    continue;
                }
                held += line.len();
                kept.push_back(line);
                while kept.len() > STDERR_KEEP_LINES || held > STDERR_MAX_BYTES {
                    if let Some(old) = kept.pop_front() {
                        held = held.saturating_sub(old.len());
                    } else {
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    kept.into_iter().last().unwrap_or_default()
}

/// Did this run actually move bytes? A skipped target emits a progress line
/// with `NA`/0 downloaded and leaves a name that was already on disk, so
/// either signal alone is misleading: real bytes count, or a name that was
/// not there before the run.
fn was_downloaded(saw_bytes: bool, before: &[String], name: Option<&str>) -> bool {
    saw_bytes || !name.is_some_and(|n| before.iter().any(|b| b == n))
}

/// File names already present in `dir` (used to spot a skipped run).
fn existing_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// Output template for the copy pass: the title (or, failing that, the
/// stem of the file yt-dlp skipped to) under the next free name. `%(ext)s`
/// keeps the real extension, since the first run picked it.
fn free_copy_template(dir: &Path, run: &Run, fallback: &str) -> String {
    let ext = run
        .dest
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    let stem_of_skipped = run
        .dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = clean_label(run.title.as_deref().unwrap_or(&stem_of_skipped), fallback);
    let stem = sanitize_filename(&stem);
    let stem = if stem.is_empty() {
        fallback.to_string()
    } else {
        stem
    };
    let base = match &ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem.clone(),
    };
    let free = unique_destination(dir, &base);
    let free = free
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(base);
    let stem = match &ext {
        Some(e) => free
            .strip_suffix(&format!(".{e}"))
            .map(str::to_string)
            .unwrap_or(free),
        None => free,
    };
    // A percent sign opens a field reference in a yt-dlp template, so a
    // literal one ("100% Hits") has to be doubled.
    format!("{}.%(ext)s", stem.replace('%', "%%"))
}

#[cfg(test)]
mod copy_name_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn skipped_runs_are_told_apart_from_real_transfers() {
        let before = vec!["clip1.mp4".to_string()];
        // yt-dlp skips an existing target but still prints a progress line
        // ("NA" for downloaded), so bytes alone must not decide this.
        assert!(!was_downloaded(false, &before, Some("clip1.mp4")));
        // A fresh name means this run wrote it.
        assert!(was_downloaded(false, &before, Some("clip1 (1).mp4")));
        // Real byte counts always mean a transfer, whatever the name.
        assert!(was_downloaded(true, &before, Some("clip1.mp4")));
        assert!(was_downloaded(true, &before, Some("clip1 (1).mp4")));
        assert!(was_downloaded(true, &[], Some("clip1.mp4")));
        assert!(was_downloaded(false, &[], Some("clip1.mp4")));
    }

    fn scratch() -> PathBuf {
        // new_id() is millisecond-based, so tests running in the same tick
        // would share a folder and one would delete the other's files.
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("catch-copy-{}-{}", std::process::id(), n));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_at(dir: &Path, name: &str, title: &str) -> Run {
        Run {
            dest: dir.join(name),
            downloaded: false,
            title: Some(title.to_string()),
        }
    }

    #[test]
    fn copy_lands_under_the_next_free_name() {
        let dir = scratch();
        std::fs::write(dir.join("Clip.webm"), b"old").unwrap();

        let tpl = free_copy_template(&dir, &run_at(&dir, "Clip.webm", "Clip"), "video-1");
        assert_eq!(tpl, "Clip (1).%(ext)s");

        // Second copy of the same video goes one further.
        std::fs::write(dir.join("Clip (1).webm"), b"copy").unwrap();
        let tpl = free_copy_template(&dir, &run_at(&dir, "Clip.webm", "Clip"), "video-1");
        assert_eq!(tpl, "Clip (2).%(ext)s");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_chars_are_escaped() {
        let dir = scratch();
        std::fs::write(dir.join("100% Hits.webm"), b"old").unwrap();
        let tpl = free_copy_template(&dir, &run_at(&dir, "100% Hits.webm", "100% Hits"), "v");
        assert_eq!(tpl, "100%% Hits (1).%(ext)s");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_title_falls_back_to_the_skipped_stem() {
        let dir = scratch();
        std::fs::write(dir.join("raw-name.mp4"), b"old").unwrap();
        let mut run = run_at(&dir, "raw-name.mp4", "");
        run.title = None;
        assert_eq!(
            free_copy_template(&dir, &run, "video-9"),
            "raw-name (1).%(ext)s"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_unicode_titles_keep_the_extension() {
        let dir = scratch();
        let title = "汉".repeat(200);
        let tpl = free_copy_template(&dir, &run_at(&dir, "x.webm", &title), "v");
        assert!(tpl.ends_with(".%(ext)s"));
        assert!(tpl.len() < 300, "template not capped: {}", tpl.len());
        assert!(
            tpl.char_indices().all(|(i, _)| tpl.is_char_boundary(i)),
            "valid UTF-8"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod line_reader_tests {
    use super::*;

    fn reader(data: &'static str) -> LineReader<tokio::io::BufReader<&'static [u8]>> {
        LineReader::new(tokio::io::BufReader::new(data.as_bytes()), 16)
    }

    #[tokio::test]
    async fn reads_lines_and_stops_at_eof() {
        let mut r = reader("one\ntwo\r\nthree");
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("one"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("two"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("three"));
        assert_eq!(r.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn caps_runaway_lines_without_losing_the_next_line() {
        let data: &'static str = "0123456789abcdefghijKL\nshort\n";
        let mut r = reader(data);
        let first = r.next_line().await.unwrap().unwrap();
        assert_eq!(first.len(), 16, "line was not capped: {first:?}");
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("short"));
    }

    #[tokio::test]
    async fn empty_input_is_eof_not_an_empty_line() {
        let mut r = reader("");
        assert_eq!(r.next_line().await.unwrap(), None);
    }
}

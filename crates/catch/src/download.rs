use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::state::{
    dir_for, load_state, mark_error, resolve_download_dir, unique_destination, update_state, Status,
};

/// Give up on a connection with no data this long, then reconnect.
/// Overridable for tests via CATCH_STALL_SECS.
fn stall_timeout() -> Duration {
    if let Ok(raw) = std::env::var("CATCH_STALL_SECS") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if (1..=600).contains(&n) {
                return Duration::from_secs(n);
            }
        }
    }
    Duration::from_secs(60)
}
/// Fresh connections attempted after a stall or mid-transfer failure
/// before calling it failed.
const MAX_RECONNECTS: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// All promised bytes are in but the server won't hang up: wait this long
/// for a clean EOF, then take the win.
const COMPLETION_GRACE: Duration = Duration::from_secs(5);
/// How often a live transfer re-reads stop orders even when no data is
/// arriving. Without this a quiet or stalled connection only notices a
/// pause/remove once the next chunk lands — or after the stall timeout,
/// which is what made "stop" look like it did nothing.
const STOP_POLL: Duration = Duration::from_millis(250);
/// Ceiling on how often a busy transfer re-reads stop orders. Each read is a
/// full parse of the state file, so doing it per chunk starved the download;
/// this keeps the file out of the hot path while staying well under STOP_POLL,
/// so a stop still lands within a tenth of a second of the click.
const ORDER_POLL: Duration = Duration::from_millis(100);

enum Directive {
    Keep,
    StandDown,
    Abort,
}

enum StreamEnd {
    Finished,
    Stalled,
    StoodDown,
    Aborted,
}

/// True when the URL path ends with a file extension that is not itself
/// a page (used to refuse HTML answers to file links).
fn looks_like_file_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let tail = path.rsplit('/').next().unwrap_or("");
    match tail.rfind('.') {
        Some(i) => {
            let ext = tail[i + 1..].to_lowercase();
            !(ext.is_empty()
                || ext.len() > 5
                || ["html", "htm", "xhtml", "php", "asp", "aspx", "jsp"].contains(&ext.as_str()))
        }
        None => false,
    }
}

/// Read-only stop check (atomic renames make plain reads safe).
fn check_directive(id: &str) -> Directive {
    match load_state().ok().and_then(|s| {
        s.downloads
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.status)
    }) {
        Some(Status::Downloading) => Directive::Keep,
        Some(Status::Paused) => Directive::StandDown,
        _ => Directive::Abort,
    }
}

/// Download one entry inline, streaming to an id-suffixed part file with
/// resume. Dead connections are re-established automatically (up to three
/// reconnects); stop orders are honored throughout. With `overwrite` (the
/// explicit "replace" action) the saved file is refetched in place;
/// otherwise an existing file is never touched and the entry lands as
/// another copy under the next free name.
pub async fn download_entry(
    id: &str,
    dir_override: Option<&str>,
    overwrite: bool,
) -> Result<PathBuf> {
    // Snapshot for local decisions (paths, resume offset). All writes go
    // through the state lock so parallel workers never clobber each other.
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
    let recorded_name = entry.filename.clone();
    let recorded_bytes = entry.bytes;
    let base = resolve_download_dir(dir_override);
    std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;
    // Category folder from the recorded name (server names re-resolve
    // per attempt below).
    let dir = dir_for(&base, &recorded_name);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // Keep the recorded filename stable across retries; only pick a fresh
    // destination when this entry has never resolved one.
    let mut dest = dir.join(&recorded_name);
    dest = if dest.exists() && recorded_bytes == 0 && !overwrite {
        unique_destination(&dir, &recorded_name)
    } else {
        dest
    };
    let mut dest_name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(recorded_name);

    // The part file carries the entry id: parallel entries for the same
    // filename must never share one.
    let mut part = dir.join(format!("{dest_name}.{id}.part"));

    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            let own = std::process::id();
            // A live holder (drain reservation or a running worker) wins;
            // a second worker for one entry must never run. Drain-held
            // reservations are claimable; only a live *worker* turns us away.
            if d.status == Status::Downloading && crate::state::live_worker_holds(d.pid, own) {
                anyhow::bail!("{id} is already held by worker {}", d.pid);
            }
            d.status = Status::Downloading;
            d.error.clear();
            d.filename = dest_name.clone();
            d.pid = own;
            d.claimed_at = crate::state::now_secs();
        }
        Ok(())
    })?;

    let client = reqwest::Client::builder()
        .user_agent("catch/0.1 (omarchy download manager)")
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;

    // Resume offset: bytes already on disk win over the recorded counter.
    let mut progress = Progress {
        bytes: std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0),
        total: 0,
        last_flush: Instant::now(),
        last_pct: 101,
        last_error: String::new(),
    };
    let mut reconnects = 0u32;

    // Connection attempts: a fresh request (with Range from what we hold)
    // is what unsticks the dead connections users used to fix by hand.
    loop {
        match check_directive(id) {
            Directive::Keep => {}
            Directive::StandDown => {
                eprintln!("catch: {id} paused at {} bytes", progress.bytes);
                return Ok(part.clone());
            }
            Directive::Abort => {
                std::fs::remove_file(&part).ok();
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
        }

        let mut req = client.get(&url);
        if progress.bytes > 0 {
            req = req.header("Range", format!("bytes={}-", progress.bytes));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let _ = mark_error(id, &format!("request failed: {e:#}"));
                anyhow::bail!("GET {url}: {e:#}");
            }
        };
        let resumed = progress.bytes > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        if progress.bytes > 0 && resp.status() == reqwest::StatusCode::OK {
            // Server ignored Range: restart from zero rather than corrupting.
            std::fs::remove_file(&part).ok();
            progress.bytes = 0;
        }
        if progress.bytes > 0 && resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // We asked past the end because we already hold everything —
            // unless the part file is gone (orphan cleanup), in which case
            // loop around and fetch from zero.
            if part.exists() {
                return finish_from_part(
                    id,
                    &dir,
                    &dest_name,
                    &part,
                    progress.bytes,
                    progress.total,
                    overwrite,
                )
                .await;
            }
            progress.bytes = 0;
            continue;
        }
        // A file link that answers HTML is a viewer/login wall, not the
        // file (this once saved release pages as APKs). Refuse loudly.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.starts_with("text/html") && looks_like_file_url(&url) {
            let _ = mark_error(id, "server returned a page, not the file (login wall?)");
            anyhow::bail!("{url}: server returned a page, not the file");
        }
        // Adopt a server-advertised filename when ours is generic (query
        // URLs like /download?id=5). Only before any bytes are held, so a
        // resume never switches files mid-stream.
        if progress.bytes == 0 && (!dest_name.contains('.') || dest_name.starts_with("download-")) {
            if let Some(server_name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(crate::state::filename_from_disposition)
            {
                dest_name = server_name;
                dest = dir_for(&base, &dest_name).join(&dest_name);
                if dest.exists() && !overwrite {
                    let parent = dest
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| base.clone());
                    dest = unique_destination(&parent, &dest_name);
                    dest_name = dest
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or(dest_name);
                }
                // Part file sits beside dest so resume finds it.
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).ok();
                    part = parent.join(format!("{dest_name}.{id}.part"));
                }
            }
        }
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            let code = resp.status();
            mark_error(id, &format!("server replied {code}"))?;
            anyhow::bail!("server replied {code} for {url}");
        }
        let announced = resp.content_length().unwrap_or(0);
        if announced > 0 {
            progress.total = if resumed {
                announced + progress.bytes
            } else {
                announced
            };
        }
        update_state(|state| {
            if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                d.total = progress.total;
                d.bytes = progress.bytes;
                d.filename = dest_name.clone();
            }
            Ok(())
        })?;

        let mut raw = tokio::fs::OpenOptions::new()
            .create(true)
            .append(resumed)
            .write(true)
            .truncate(!resumed)
            .open(&part)
            .await
            .with_context(|| format!("open {}", part.display()))?;
        // Buffer the writes. tokio::fs::File sends every write through the
        // blocking pool, so one call per network chunk meant thousands of
        // round-trips per file — that overhead, not the network, is what made
        // Catch slower than a browser. The buffer coalesces many chunks into
        // one write. Resume reads the part file's size, so anything still
        // buffered when the transfer ends is re-fetched rather than skipped.
        let mut file = tokio::io::BufWriter::with_capacity(256 * 1024, &mut raw);
        let mut stream = resp.bytes_stream();
        let end =
            match stream_one_connection(id, &part, &mut file, &mut stream, &mut progress).await {
                Ok(end) => end,
                Err(e) => {
                    // A BufWriter does not flush on drop. Put what is buffered on
                    // disk before the error travels on, so the next run resumes
                    // from where the state file says it does.
                    let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                    return Err(e);
                }
            };
        tokio::io::AsyncWriteExt::shutdown(&mut file).await.ok();
        drop(file);
        drop(raw);
        match end {
            StreamEnd::Finished => break,
            StreamEnd::StoodDown => {
                // Paused: the part file stays, resume continues via Range.
                eprintln!("catch: {id} paused at {} bytes", progress.bytes);
                return Ok(part.clone());
            }
            StreamEnd::Aborted => {
                std::fs::remove_file(&part).ok();
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
            StreamEnd::Stalled => {
                reconnects += 1;
                if reconnects > MAX_RECONNECTS {
                    let msg = if progress.last_error.is_empty() {
                        "stalled: no data for 60s, three reconnects failed".to_string()
                    } else {
                        format!(
                            "transfer failed after three reconnects: {}",
                            progress.last_error
                        )
                    };
                    let _ = mark_error(id, &msg);
                    anyhow::bail!("{url}: {msg} (resume to retry)");
                }
                eprintln!(
                    "catch: {id} interrupted, reconnecting ({reconnects}/{MAX_RECONNECTS}) …"
                );
            }
        }
    }

    // Our bytes only ever lived in the id-suffixed part file, so an
    // existing dest belongs to someone else: never overwrite it silently.
    // Only an explicit replace may take that dest over.
    let dest = if dest.exists() && !overwrite {
        unique_destination(&dir, &dest_name)
    } else {
        dest
    };
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("move to {}", dest.display()))?;
    update_state(|state| {
        // A stop order may have landed with the final chunk: only take the
        // win if nobody cancelled or removed us mid-flight.
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            if d.status == Status::Downloading {
                d.status = Status::Done;
                d.bytes = progress.bytes;
                if d.total == 0 {
                    d.total = progress.bytes;
                }
            }
        }
        // The file is on disk whatever happened to the row above, so record
        // it: a link caught again must still be recognised as a duplicate
        // after the queue is cleared.
        if let Some(url) = state
            .downloads
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.url.clone())
        {
            crate::state::record_saved(state, &url, &base, &dest, crate::state::DownloadKind::File);
        }
        Ok(())
    })?;
    eprintln!("catch: done {} -> {}", id, dest.display());
    crate::state::spawn_drain();
    Ok(dest)
}

/// Finalize from an already-complete part file (416/range-past-end case).
async fn finish_from_part(
    id: &str,
    dir: &std::path::Path,
    dest_name: &str,
    part: &std::path::Path,
    bytes: u64,
    total: u64,
    overwrite: bool,
) -> Result<PathBuf> {
    let dest = dir.join(dest_name);
    let dest = if dest.exists() && !overwrite {
        unique_destination(dir, dest_name)
    } else {
        dest
    };
    tokio::fs::rename(part, &dest)
        .await
        .with_context(|| format!("move to {}", dest.display()))?;
    let total = if total == 0 { bytes } else { total.max(bytes) };
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            d.status = Status::Done;
            d.bytes = bytes;
            d.total = total;
        }
        Ok(())
    })?;
    eprintln!("catch: done {} -> {}", id, dest.display());
    crate::state::spawn_drain();
    Ok(dest)
}

/// Live counters for one transfer, shared across reconnects.
struct Progress {
    bytes: u64,
    total: u64,
    last_flush: Instant,
    last_pct: u64,
    last_error: String,
}

impl Progress {
    fn percent(&self) -> u64 {
        if self.total == 0 {
            return 101;
        }
        self.bytes
            .checked_mul(100)
            .and_then(|v| v.checked_div(self.total))
            .unwrap_or(100)
    }
}

/// Read one connection until it ends. Honors stop orders on every flush,
/// and on every poll tick even when the connection has gone quiet.
async fn stream_one_connection(
    id: &str,
    part: &std::path::Path,
    file: &mut tokio::io::BufWriter<&mut tokio::fs::File>,
    stream: &mut (impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    progress: &mut Progress,
) -> Result<StreamEnd> {
    use tokio::io::AsyncWriteExt;
    // Reading the stop order loads and parses the whole state file: every
    // row, the saved index, every pending question. Doing that once per chunk
    // turned a fast transfer into an I/O storm that competed with the
    // download itself, which is what made Catch look slow beside a browser.
    // Orders are read on a timer instead, and always on a poll tick, so a
    // connection that has gone quiet still stops on time.
    let mut last_order_read = Instant::now();
    let mut order_due = true;
    loop {
        // Stop orders first: a transfer whose server has gone quiet is
        // exactly when someone reaches for "stop", and the row may be paused
        // or removed from another process while no bytes are moving at all.
        if order_due || last_order_read.elapsed() >= ORDER_POLL {
            order_due = false;
            last_order_read = Instant::now();
            match check_directive(id) {
                Directive::Keep => {}
                Directive::StandDown => return Ok(StreamEnd::StoodDown),
                Directive::Abort => return Ok(StreamEnd::Aborted),
            }
        }
        // Past the promised total with the server still talking: it may be
        // over-delivering (keep reading) or never hanging up (take the win
        // after a short grace instead of sitting at 100% forever).
        let complete = progress.total > 0 && progress.bytes >= progress.total;
        let wait = if complete {
            COMPLETION_GRACE
        } else {
            stall_timeout()
        };
        // Wait for data, but never longer than the poll interval: the tick
        // sends us back round the loop to re-read stop orders instead of
        // sitting in a read until the stall timeout fires.
        let next = tokio::select! {
            biased;
            chunk = stream.next() => Some(chunk),
            () = tokio::time::sleep(STOP_POLL.min(wait)) => None,
        };
        let chunk = match next {
            // A poll tick: nothing to write, but this is the moment a quiet
            // connection must honour a stop order, so force the next read
            // instead of waiting for a chunk that may never arrive.
            None => {
                order_due = true;
                continue;
            }
            Some(Some(Ok(c))) => c,
            Some(Some(Err(e))) => {
                // Worth one reconnect like a stall: servers that cut a
                // connection mid-body usually answer a ranged retry cleanly.
                progress.last_error = format!("{e:#}");
                return Ok(StreamEnd::Stalled);
            }
            Some(None) => return Ok(StreamEnd::Finished),
        };
        file.write_all(&chunk)
            .await
            .with_context(|| format!("write {}", part.display()))?;
        progress.bytes += chunk.len() as u64;
        let pct = progress.percent();
        if progress.last_flush.elapsed() >= Duration::from_millis(250) || pct != progress.last_pct {
            progress.last_flush = Instant::now();
            progress.last_pct = pct;
            // Merge-only flush that also reads stop orders: another process
            // may have paused, cancelled, or removed this entry (panel, CLI).
            // A missing record counts as cancelled.
            let mut directive = Directive::Keep;
            update_state(|state| {
                directive = match state.downloads.iter_mut().find(|d| d.id == id) {
                    Some(d) if d.status == Status::Downloading => {
                        d.bytes = progress.bytes;
                        d.total = progress.total;
                        Directive::Keep
                    }
                    Some(d) if d.status == Status::Paused => Directive::StandDown,
                    _ => Directive::Abort,
                };
                Ok(())
            })
            .unwrap_or(());
            match directive {
                Directive::Keep => {
                    if progress.total > 0 {
                        eprintln!(
                            "catch: {} {}% ({}/{})",
                            id, pct, progress.bytes, progress.total
                        );
                    } else {
                        eprintln!("catch: {} {} bytes", id, progress.bytes);
                    }
                }
                Directive::StandDown => return Ok(StreamEnd::StoodDown),
                Directive::Abort => return Ok(StreamEnd::Aborted),
            }
        }
    }
}

#[cfg(test)]
mod stop_order_tests {
    use super::*;

    /// The stall timeout is what made "stop" look broken: with stop orders
    /// only read when a chunk arrived, a quiet connection kept downloading
    /// for up to a minute (and across reconnects) after the row was gone.
    #[test]
    fn poll_is_far_shorter_than_the_stall_timeout() {
        assert!(
            STOP_POLL < stall_timeout(),
            "a stop must not wait on a stall"
        );
        assert!(
            STOP_POLL >= Duration::from_millis(50),
            "but not spin the state file"
        );
    }

    /// Every status the panel can leave behind has to resolve to an order
    /// the stream loop acts on, and a missing row counts as cancelled.
    #[test]
    fn every_status_maps_to_an_order() {
        let order = |s: Option<Status>| match s {
            Some(Status::Downloading) => Directive::Keep,
            Some(Status::Paused) => Directive::StandDown,
            _ => Directive::Abort,
        };
        assert!(matches!(order(Some(Status::Downloading)), Directive::Keep));
        assert!(matches!(order(Some(Status::Paused)), Directive::StandDown));
        for gone in [Some(Status::Cancelled), Some(Status::Error), None] {
            assert!(
                matches!(order(gone), Directive::Abort),
                "a removed or cancelled entry must abort, not keep going"
            );
        }
    }
}

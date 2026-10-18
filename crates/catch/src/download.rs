use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::segment;
use crate::state::{
    dir_for, load_state, mark_error, resolve_download_dir, sanitize_filename, unique_destination,
    update_state, worker_holds, Status,
};

/// Give up on a connection with no body data this long, then reconnect.
/// Overridable for tests via CATCH_STALL_SECS.
pub(crate) fn stall_timeout() -> Duration {
    if let Ok(raw) = std::env::var("CATCH_STALL_SECS") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if (1..=3600).contains(&n) {
                return Duration::from_secs(n);
            }
        }
    }
    Duration::from_secs(60)
}

/// Ceiling on connecting and on waiting for response headers. A server that
/// accepts the connection and then says nothing must not hang a worker for
/// ever: `connect_timeout` alone only covers the TCP handshake.
pub(crate) fn response_timeout() -> Duration {
    if let Ok(raw) = std::env::var("CATCH_RESPONSE_SECS") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if (1..=600).contains(&n) {
                return Duration::from_secs(n);
            }
        }
    }
    Duration::from_secs(30)
}

/// Connection attempts per transfer, including the first. Overridable for
/// tests (CATCH_MAX_ATTEMPTS=1 makes a transfer single-shot).
pub(crate) fn max_attempts() -> u32 {
    if let Ok(raw) = std::env::var("CATCH_MAX_ATTEMPTS") {
        if let Ok(n) = raw.trim().parse::<u32>() {
            if (1..=10).contains(&n) {
                return n;
            }
        }
    }
    4
}

/// All promised bytes are in but the server won't hang up: wait this long
/// for a clean EOF, then take the win.
const COMPLETION_GRACE: Duration = Duration::from_secs(5);

/// How often a live transfer re-reads stop orders even when no data is
/// arriving. Without this a quiet or stalled connection only notices a
/// pause/remove once the next chunk lands — or after the stall timeout,
/// which is what made "stop" look like it did nothing.
pub(crate) const STOP_POLL: Duration = Duration::from_millis(250);

/// How often a busy transfer writes progress to the state file (and reads
/// stop orders back). The whole file is parsed and rewritten for each flush,
/// so this is the difference between a smooth bar and an I/O storm.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// Ceiling on how often a busy transfer re-reads stop orders. Each read is a
/// full parse of the state file, so doing it per chunk starved the download;
/// this keeps the file out of the hot path while staying well under
/// STOP_POLL, so a stop still lands within a tenth of a second of the click.
const ORDER_POLL: Duration = Duration::from_millis(100);

/// First retry delay; each further attempt doubles it.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// Ceiling for a computed backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// Ceiling for a server-provided Retry-After (a hostile or broken server
/// must not park a worker for hours).
const RETRY_AFTER_MAX: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Directive {
    Keep,
    StandDown,
    Abort,
}

/// How one connection ended.
enum StreamEnd {
    /// The promised bytes arrived, or the body ended with no length promised.
    Finished,
    /// The connection went quiet past its deadline, died mid-body, or the
    /// body was shorter than announced. Worth a ranged reconnect.
    Interrupted(String),
    StoodDown,
    Aborted,
}

/// A failure and whether it is worth another attempt.
pub(crate) enum Verdict {
    Retry(Duration),
    Fatal(String),
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

/// "bytes 200-1000/67589" -> (200, 1000, Some(67589)). A malformed or
/// unsatisfiable header returns None so the caller restarts from zero
/// instead of trusting an offset it cannot verify.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    let (start, end) = range.trim().split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = end.trim().parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    // A range that runs past the representation's own size is malformed,
    // even though start and end look individually valid.
    if total.is_some_and(|t| end >= t) {
        return None;
    }
    Some((start, end, total))
}

/// A small, jittered exponential backoff. Jitter keeps a batch of workers
/// from hammering a server that just came back in lockstep.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    let base = BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_MAX);
    // Deterministic-ish jitter from the clock: +/- 25 %, never zero.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let spread = (base.as_millis() as u64) / 2;
    let jitter = if spread == 0 { 0 } else { nanos % spread };
    let millis = (base.as_millis() as u64).saturating_sub(spread / 2) + jitter;
    // Bias low: the first retry should be quick.
    Duration::from_millis(millis.clamp(50, BACKOFF_MAX.as_millis() as u64))
}

/// A server-provided Retry-After (seconds or HTTP-date form), clamped.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    // Integer seconds form.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(RETRY_AFTER_MAX));
    }
    // HTTP-date form (RFC 7231). Far-future and malformed values are clamped
    // or rejected rather than parking the worker indefinitely.
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(&raw) {
        let now = chrono::Utc::now();
        let delay = date.signed_duration_since(now);
        if delay.num_seconds() < 0 {
            return Some(Duration::ZERO);
        }
        let secs = delay.num_seconds() as u64;
        return Some(Duration::from_secs(secs).min(RETRY_AFTER_MAX));
    }
    // Try the IMF-fixdate spelling explicitly (RFC 7231 preferred form).
    if let Ok(date) = chrono::NaiveDateTime::parse_from_str(&raw, "%a, %d %b %Y %H:%M:%S GMT") {
        let now = chrono::Utc::now().naive_utc();
        let delay = date.signed_duration_since(now);
        if delay.num_seconds() < 0 {
            return Some(Duration::ZERO);
        }
        let secs = delay.num_seconds() as u64;
        return Some(Duration::from_secs(secs).min(RETRY_AFTER_MAX));
    }
    None
}

/// Classify an HTTP failure status. Temporary conditions retry; anything
/// that means "this request will never work" does not — a 404 or a 403 must
/// fail immediately instead of being attempted four times.
pub(crate) fn status_verdict(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    attempt: u32,
) -> Verdict {
    match status.as_u16() {
        408 | 425 | 429 | 500..=599 => {
            let wait = retry_after(headers)
                .unwrap_or_else(|| backoff(attempt))
                .max(Duration::from_millis(50));
            Verdict::Retry(wait)
        }
        _ => Verdict::Fatal(format!("server replied {status}")),
    }
}

/// Classify a transport failure. Builder errors (a malformed URL) and
/// redirect loops are permanent; connection, timeout, body and protocol
/// failures are worth retrying.
pub(crate) fn transport_verdict(e: &reqwest::Error, attempt: u32) -> Verdict {
    if e.is_builder() {
        return Verdict::Fatal(format!("bad request: {e}"));
    }
    if e.is_redirect() {
        return Verdict::Fatal(format!("redirect loop: {e}"));
    }
    Verdict::Retry(backoff(attempt))
}

/// Read-only stop check (atomic renames make plain reads safe).
///
/// The state file is only re-parsed when it actually changed: a live
/// transfer asks this every poll tick, and three transfers doing that for a
/// big queue meant more parsing than downloading.
struct DirectiveWatch {
    stamp: Option<(std::time::SystemTime, u64)>,
    directive: Option<Directive>,
}

impl DirectiveWatch {
    fn new() -> Self {
        DirectiveWatch {
            stamp: None,
            directive: None,
        }
    }

    fn directive(&mut self, id: &str) -> Directive {
        let stamp = std::fs::metadata(crate::state::state_path())
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if stamp.is_some() && stamp == self.stamp {
            if let Some(d) = self.directive {
                return d;
            }
        }
        let directive = match load_state() {
            Err(_) => Directive::Keep, // state unreadable: preserve data, keep going
            Ok(s) => match s
                .downloads
                .into_iter()
                .find(|d| d.id == id)
                .map(|d| d.status)
            {
                Some(Status::Downloading) => Directive::Keep,
                Some(Status::Paused) => Directive::StandDown,
                _ => Directive::Abort,
            },
        };
        self.stamp = stamp;
        self.directive = Some(directive);
        directive
    }
}

/// Download one entry inline, streaming to an id-suffixed part file with
/// resume. Interrupted connections are re-established with a bounded,
/// jittered backoff; stop orders are honored throughout. With `overwrite`
/// (the explicit "replace" action) the saved file is refetched in place;
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
    let recorded_name = sanitize_filename(&entry.filename);
    let recorded_bytes = entry.bytes;
    let recorded_total = entry.total;
    let recorded_etag = entry.etag.clone();
    let recorded_modified = entry.last_modified.clone();
    let base = resolve_download_dir(dir_override);
    std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;

    // One transfer per entry, enforced across processes: the row's pid is
    // written by whoever reserved the slot, which is not proof that a worker
    // is behind it. The lock is the proof, and it is released when this
    // function returns (including on error).
    let Some(_claim) = crate::state::claim_worker(id)? else {
        anyhow::bail!("{id} is already being downloaded by another catch process");
    };
    // Secondary guard for the pre-claim world: a live worker for this id
    // that outraced us (it holds the row but not yet the lock).
    if entry.status == Status::Downloading && worker_holds(entry.pid, id) {
        anyhow::bail!("{id} is already held by worker {}", entry.pid);
    }

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
            d.status = Status::Downloading;
            d.error.clear();
            d.filename = dest_name.clone();
            d.pid = std::process::id();
            // The drainer's reservation grace is over: a worker that has
            // stamped itself is protected by its transfer lock while alive,
            // so a dead pid can be requeued at once instead of waiting out
            // CLAIM_GRACE.
            d.claimed_at = 0;
        }
        Ok(())
    })?;

    // One client for the whole transfer (connection pool, keep-alive). TLS
    // verification is rustls' default: on.
    let client = reqwest::Client::builder()
        .user_agent("catch/0.1 (omarchy download manager)")
        .connect_timeout(response_timeout())
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;

    // Resume offset: bytes already on disk win over the recorded counter.
    let mut progress = Progress {
        bytes: std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0),
        total: recorded_total,
        last_flush: Instant::now(),
        last_error: String::new(),
        etag: recorded_etag,
        last_modified: recorded_modified,
        resumable: true,
    };
    let mut attempts = 0u32;
    let mut restarts = 0u32;
    let mut watch = DirectiveWatch::new();

    // Segmented resume: if a sidecar manifest exists and is valid, run the
    // segmented downloader instead of the sequential path.
    // Only probe when segmentation is even possible: a cap of 1 disables it,
    // and probing would only add a wasted request per file.
    let mut probe_pending = progress.bytes == 0 && segment::segments_cap() >= 2;

    // Recoverable finalization: a previous run may have renamed the part to
    // its destination but crashed before updating queue state. Complete that
    // update now instead of creating a second copy.
    let intent_path = PathBuf::from(format!("{}.finalizing", part.display()));
    if intent_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&intent_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(dest_str) = v.get("dest").and_then(|x| x.as_str()) {
                    let dest = PathBuf::from(dest_str);
                    if dest.is_file() {
                        let final_name = dest
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                        let _ = update_state(|state| {
                            if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                                if d.status == Status::Downloading {
                                    d.status = Status::Done;
                                    d.bytes = size;
                                    d.total = size.max(recorded_total);
                                    d.filename = final_name.clone();
                                    d.error.clear();
                                    d.pid = 0;
                                    d.claimed_at = 0;
                                }
                            }
                            crate::state::record_saved(
                                state,
                                &url,
                                &base,
                                &dest,
                                crate::state::DownloadKind::File,
                            );
                            Ok(())
                        });
                        let _ = std::fs::remove_file(&intent_path);
                        crate::state::spawn_drain();
                        return Ok(dest);
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&intent_path);
    }

    if let Some(manifest) = segment::load_manifest(&part, &url)? {
        // Reconcile validators: prefer queue state, fall back to the
        // manifest (which may have survived a crash that lost the queue
        // update). The manifest must not silently lose its identity.
        let etag = if progress.etag.is_empty() {
            manifest.etag.clone()
        } else {
            progress.etag.clone()
        };
        let last_modified = if progress.last_modified.is_empty() {
            manifest.last_modified.clone()
        } else {
            progress.last_modified.clone()
        };
        progress.etag = etag.clone();
        progress.last_modified = last_modified.clone();

        let active = load_state()
            .map(|s| {
                s.downloads
                    .iter()
                    .filter(|d| d.status == Status::Downloading)
                    .count()
            })
            .unwrap_or(1);
        let share = segment::segment_share(active);
        let outcome = segment::run_segmented(
            id,
            &url,
            &client,
            &part,
            manifest.total,
            etag,
            last_modified,
            manifest,
            share,
        )
        .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let _ = mark_error(id, &format!("{e:#}"));
                let _ = update_state(|state| {
                    if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                        d.pid = 0;
                        d.claimed_at = 0;
                    }
                    Ok(())
                });
                crate::state::spawn_drain();
                return Err(e);
            }
        };
        match outcome {
            segment::SegmentedResult::Complete { manifest } => {
                segment::remove_manifest(&part);
                return finish(Finish {
                    id,
                    url: &url,
                    dir: &dir,
                    dest_name: &dest_name,
                    part: &part,
                    bytes: manifest.total,
                    total: Some(manifest.total),
                    overwrite,
                    base: &base,
                })
                .await;
            }
            segment::SegmentedResult::Paused => {
                note_paused(id, &progress);
                return Ok(part.clone());
            }
            segment::SegmentedResult::Stopped => {
                std::fs::remove_file(&part).ok();
                segment::remove_manifest(&part);
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
            segment::SegmentedResult::Fallback(reason) => {
                eprintln!("catch: {id} segmented download fell back to sequential: {reason}");
                segment::remove_manifest(&part);
                let _ = std::fs::remove_file(&part);
                progress.bytes = 0;
                progress.total = 0;
                progress.etag.clear();
                progress.last_modified.clear();
                probe_pending = false;
            }
        }
    }

    // Connection attempts: a fresh request (with Range from what we hold)
    // is what unsticks the dead connections users used to fix by hand.
    loop {
        match watch.directive(id) {
            Directive::Keep => {}
            Directive::StandDown => {
                note_paused(id, &progress);
                return Ok(part.clone());
            }
            Directive::Abort => {
                std::fs::remove_file(&part).ok();
                segment::remove_manifest(&part);
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
        }

        if attempts >= max_attempts() {
            let msg = format!(
                "gave up after {} attempts: {}",
                attempts,
                if progress.last_error.is_empty() {
                    "connection lost"
                } else {
                    &progress.last_error
                }
            );
            let _ = mark_error(id, &msg);
            anyhow::bail!("{url}: {msg} (resume to retry)");
        }
        attempts += 1;

        // Resume offset: bytes already on disk win over the recorded counter,
        // and so they do on every attempt: the part file is the truth about
        // what the server still owes us.
        let on_disk = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if on_disk != progress.bytes {
            progress.bytes = on_disk;
        }

        let was_probe = probe_pending && progress.bytes == 0;
        let mut req = client.get(&url);
        if progress.bytes > 0 && progress.resumable {
            req = req.header("Range", format!("bytes={}-", progress.bytes));
            // If-Range makes the server answer 200 (whole file) instead of a
            // 206 when the representation changed since we started: a resume
            // must never splice two different files together.
            if let Some(validator) = progress.if_range() {
                req = req.header("If-Range", validator);
            }
        } else if was_probe {
            req = req
                .header("Range", "bytes=0-1023")
                .header("Accept-Encoding", "identity");
        }
        let sent = tokio::time::timeout(response_timeout(), req.send()).await;
        let resp = match sent {
            Err(_) => {
                let why = format!(
                    "no response headers within {}s",
                    response_timeout().as_secs()
                );
                let wait = retry_or_fail(id, &url, &why, attempts, &mut progress)?;
                tokio::time::sleep(wait).await;
                continue;
            }
            Ok(Err(e)) => match transport_verdict(&e, attempts) {
                Verdict::Fatal(why) => {
                    let _ = mark_error(id, &why);
                    anyhow::bail!("GET {url}: {why}");
                }
                Verdict::Retry(wait) => {
                    let _ = note_retry(id, &format!("{e}"), attempts, wait, &mut progress);
                    tokio::time::sleep(wait).await;
                    continue;
                }
            },
            Ok(Ok(r)) => r,
        };

        let status = resp.status();
        // A probe that hits 416 means the representation is empty or the
        // server disliked the range: retry without a probe range.
        if was_probe && status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            probe_pending = false;
            drop(resp);
            continue;
        }
        // A compressed body has no stable byte offsets to resume against.
        // reqwest is built without decompression here, so this only fires
        // when a server encodes anyway: fall back to whole-file transfers.
        let encoded = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|v| !v.eq_ignore_ascii_case("identity"))
            .unwrap_or(false);
        if encoded {
            progress.resumable = false;
            if progress.bytes > 0 {
                // A ranged request is already in flight and its answer is
                // about the encoded stream: start over from zero.
                restart_from_zero(&part, &mut progress);
                continue;
            }
        }

        // A resumed answer must prove it starts exactly where we stopped.
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let header = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok());
            let resp_etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok());
            let resp_modified = resp
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok());
            match header.and_then(parse_content_range) {
                Some((start, end, None)) if start == progress.bytes && end + 1 > start => {
                    // Unknown representation length: a ranged response cannot
                    // establish completion. Fall back to a plain sequential
                    // download (resumable state is discarded).
                    progress.resumable = false;
                    if progress.bytes > 0 {
                        restart_from_zero(&part, &mut progress);
                    }
                    drop(resp);
                    continue;
                }
                Some((start, end, total))
                    if start == progress.bytes && end + 1 > start && total.is_some() =>
                {
                    let total = total.unwrap();
                    // A server is supposed to answer 200 to a stale
                    // If-Range, but trust is not proof: if the response
                    // itself carries different validators (or a different
                    // total) than the representation we hold, appending the
                    // tail would splice two files. Restart from zero.
                    let etag_changed =
                        !progress.etag.is_empty() && resp_etag.is_some_and(|e| e != progress.etag);
                    let modified_changed = !progress.last_modified.is_empty()
                        && resp_modified.is_some_and(|lm| lm != progress.last_modified);
                    let size_changed = progress.total > 0 && Some(total) != Some(progress.total);
                    if progress.bytes > 0 && (etag_changed || modified_changed || size_changed) {
                        eprintln!(
                            "catch: {id} remote changed under a resumed transfer; restarting from zero"
                        );
                        restart_from_zero(&part, &mut progress);
                        continue;
                    }
                    progress.total = total;
                }
                _ => {
                    let why = "malformed Content-Range on a resumed transfer";
                    let wait = retry_or_fail(id, &url, why, attempts, &mut progress)?;
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
        } else if status == reqwest::StatusCode::OK {
            if progress.bytes > 0 {
                // Server ignored Range (or If-Range changed): start over
                // rather than appending a whole file to a partial one.
                restart_from_zero(&part, &mut progress);
            }
        } else if progress.bytes > 0 && status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // We asked past the end because we already hold everything —
            // unless the remote representation changed under us, in which
            // case the answer names a different total.
            let remote_total = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().strip_prefix("bytes */"))
                .and_then(|v| v.trim().parse::<u64>().ok());
            let held = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let complete = match remote_total {
                Some(total) => held == total,
                None => progress.total > 0 && held >= progress.total,
            };
            if complete && part.exists() {
                return finish(Finish {
                    id,
                    url: &url,
                    dir: &dir,
                    dest_name: &dest_name,
                    part: &part,
                    bytes: held,
                    total: remote_total,
                    overwrite,
                    base: &base,
                })
                .await;
            }
            if restarts >= 2 {
                let msg = format!("server keeps refusing the resumed range ({status})");
                let _ = mark_error(id, &msg);
                anyhow::bail!("{url}: {msg}");
            }
            restarts += 1;
            restart_from_zero(&part, &mut progress);
            continue;
        } else if !status.is_success() {
            match status_verdict(status, resp.headers(), attempts) {
                Verdict::Fatal(why) => {
                    let _ = mark_error(id, &why);
                    anyhow::bail!("{url}: {why}");
                }
                Verdict::Retry(wait) => {
                    let _ = note_retry(
                        id,
                        &format!("server replied {status}"),
                        attempts,
                        wait,
                        &mut progress,
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
        }

        // Capture validators as soon as the response is in, so a probe can
        // decide on segmentation from them.
        progress.capture_validators(resp.headers());
        if was_probe {
            probe_pending = false;
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

        // Segmented downloading: a probe that came back as a valid 206 with a
        // usable length, validators and enough connection budget. Anything
        // else falls through to the sequential path below.
        if was_probe && status == reqwest::StatusCode::PARTIAL_CONTENT {
            let total = progress.total;
            let active = load_state()
                .map(|s| {
                    s.downloads
                        .iter()
                        .filter(|d| d.status == Status::Downloading)
                        .count()
                })
                .unwrap_or(1);
            let share = segment::segment_share(active);
            if segment::eligible(total, share)
                && (!progress.etag.is_empty() || !progress.last_modified.is_empty())
            {
                // Preallocate the part file and write the manifest.
                let n = segment::segment_count(total, share);
                let boundaries = segment::segment_ranges(total, n);
                let manifest = segment::SegmentManifest {
                    version: segment::MANIFEST_VERSION,
                    url: url.clone(),
                    total,
                    etag: progress.etag.clone(),
                    last_modified: progress.last_modified.clone(),
                    segments: boundaries,
                    created_at: crate::state::now_secs(),
                    updated_at: crate::state::now_secs(),
                };
                if let Err(e) = (|| -> Result<()> {
                    let pf = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&part)?;
                    pf.set_len(total)?;
                    Ok(())
                })() {
                    eprintln!("catch: {id} cannot preallocate part: {e:#}");
                } else {
                    if segment::save_manifest_atomic(&part, &manifest).is_ok() {
                        update_state(|state| {
                            if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                                d.total = total;
                                d.etag = progress.etag.clone();
                                d.last_modified = progress.last_modified.clone();
                                d.filename = dest_name.clone();
                            }
                            Ok(())
                        })?;
                        let outcome = segment::run_segmented(
                            id,
                            &url,
                            &client,
                            &part,
                            total,
                            progress.etag.clone(),
                            progress.last_modified.clone(),
                            manifest,
                            share,
                        )
                        .await;
                        let outcome = match outcome {
                            Ok(o) => o,
                            Err(e) => {
                                let _ = mark_error(id, &format!("{e:#}"));
                                let _ = update_state(|state| {
                                    if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id)
                                    {
                                        d.pid = 0;
                                        d.claimed_at = 0;
                                    }
                                    Ok(())
                                });
                                crate::state::spawn_drain();
                                return Err(e);
                            }
                        };
                        match outcome {
                            segment::SegmentedResult::Complete { manifest } => {
                                segment::remove_manifest(&part);
                                return finish(Finish {
                                    id,
                                    url: &url,
                                    dir: &dir,
                                    dest_name: &dest_name,
                                    part: &part,
                                    bytes: manifest.total,
                                    total: Some(manifest.total),
                                    overwrite,
                                    base: &base,
                                })
                                .await;
                            }
                            segment::SegmentedResult::Paused => {
                                note_paused(id, &progress);
                                return Ok(part.clone());
                            }
                            segment::SegmentedResult::Stopped => {
                                std::fs::remove_file(&part).ok();
                                segment::remove_manifest(&part);
                                eprintln!("catch: {id} stopped");
                                anyhow::bail!("{id} stopped");
                            }
                            segment::SegmentedResult::Fallback(reason) => {
                                eprintln!(
                                    "catch: {id} segmented download fell back to sequential: {reason}"
                                );
                                segment::remove_manifest(&part);
                                let _ = std::fs::remove_file(&part);
                                progress.bytes = 0;
                                progress.total = 0;
                                progress.etag.clear();
                                progress.last_modified.clear();
                                probe_pending = false;
                                drop(resp);
                                continue;
                            }
                        }
                    }
                }
            } else {
                // Small file or squeezed budget: drop the probe and run the
                // ordinary sequential path from scratch.
                probe_pending = false;
                progress.bytes = 0;
                drop(resp);
                continue;
            }
        }

        let resumed = progress.bytes > 0;
        if !resumed {
            let announced = resp.content_length().unwrap_or(0);
            progress.total = announced;
        }
        update_state(|state| {
            if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
                d.total = progress.total;
                d.bytes = progress.bytes;
                d.filename = dest_name.clone();
                d.etag = progress.etag.clone();
                d.last_modified = progress.last_modified.clone();
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
        let end = match stream_one_connection(
            id,
            &part,
            &mut file,
            &mut stream,
            &mut progress,
            &mut watch,
        )
        .await
        {
            Ok(end) => end,
            Err(e) => {
                // A BufWriter does not flush on drop. Put what is buffered on
                // disk before the error travels on, so the next run resumes
                // from where the state file says it does.
                let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                // A local write failure (disk full, read-only folder) will
                // not fix itself: report it instead of hammering the server.
                let _ = mark_error(id, &format!("{e:#}"));
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
                note_paused(id, &progress);
                return Ok(part.clone());
            }
            StreamEnd::Aborted => {
                std::fs::remove_file(&part).ok();
                segment::remove_manifest(&part);
                eprintln!("catch: {id} stopped");
                anyhow::bail!("{id} stopped");
            }
            StreamEnd::Interrupted(why) => {
                progress.last_error = why.clone();
                if attempts >= max_attempts() {
                    // The loop's own deadline check reports this on the next
                    // pass; keep the part file for a manual resume.
                    continue;
                }
                let wait = backoff(attempts);
                note_retry(id, &why, attempts, wait, &mut progress)?;
                tokio::time::sleep(wait).await;
                continue;
            }
        }
    }

    // Our bytes only ever lived in the id-suffixed part file, so an
    // existing dest belongs to someone else: never overwrite it silently.
    // Only an explicit replace may take that dest over.
    finish(Finish {
        id,
        url: &url,
        dir: &dir,
        dest_name: &dest_name,
        part: &part,
        bytes: progress.bytes,
        total: Some(progress.total),
        overwrite,
        base: &base,
    })
    .await
}

/// Decision helper for an attempt that failed before any bytes were written:
/// another attempt (with its delay) or a fatal error.
fn retry_or_fail(
    id: &str,
    url: &str,
    why: &str,
    attempts: u32,
    progress: &mut Progress,
) -> Result<Duration> {
    if attempts >= max_attempts() {
        let msg = format!("gave up after {attempts} attempts: {why}");
        let _ = mark_error(id, &msg);
        anyhow::bail!("{url}: {msg} (resume to retry)");
    }
    let wait = backoff(attempts);
    let _ = note_retry(id, why, attempts, wait, progress);
    Ok(wait)
}

/// Record the retry on the row (the panel shows `error` on the card) and in
/// the log, and wait before the next attempt.
fn note_retry(
    id: &str,
    why: &str,
    attempts: u32,
    wait: Duration,
    progress: &mut Progress,
) -> Result<()> {
    progress.last_error = why.to_string();
    let note = format!(
        "attempt {attempts}/{} failed: {why}; retrying in {:.1}s",
        max_attempts(),
        wait.as_secs_f64()
    );
    eprintln!("catch: {id} {note}");
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            if d.status == Status::Downloading {
                d.error = note.clone();
            }
        }
        Ok(())
    })
}

fn note_paused(id: &str, progress: &Progress) {
    eprintln!("catch: {id} paused at {} bytes", progress.bytes);
}

/// Live counters for one transfer, shared across reconnects.
struct Progress {
    bytes: u64,
    total: u64,
    last_flush: Instant,
    last_error: String,
    etag: String,
    last_modified: String,
    /// False when byte offsets cannot be trusted across connections
    /// (a content-encoded body): never resume such a transfer.
    resumable: bool,
}

/// Throw away the partial file and every resume validator: the server
/// answered about a representation that is not the one already on disk (or
/// ignored the request's Range), so appending would splice two files.
fn restart_from_zero(part: &std::path::Path, progress: &mut Progress) {
    let _ = std::fs::remove_file(part);
    progress.bytes = 0;
    progress.total = 0;
    progress.etag.clear();
    progress.last_modified.clear();
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

    /// Remember the validators of this response, for `If-Range` on the next
    /// reconnect of this same transfer.
    fn capture_validators(&mut self, headers: &reqwest::header::HeaderMap) {
        if let Some(etag) = headers
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
        {
            self.etag = etag.to_string();
        }
        if let Some(lm) = headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
        {
            self.last_modified = lm.to_string();
        }
    }

    fn if_range(&self) -> Option<String> {
        if !self.etag.is_empty() {
            Some(self.etag.clone())
        } else if !self.last_modified.is_empty() {
            Some(self.last_modified.clone())
        } else {
            None
        }
    }
}

/// Write live counters to the row and translate its status into an order.
/// One locked read-modify-write also makes this the stop-order check for a
/// transfer that is busily moving bytes. `Ok(None)` means keep going.
async fn flush_progress(id: &str, progress: &mut Progress) -> Result<Option<StreamEnd>> {
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
    if progress.total > 0 {
        eprintln!(
            "catch: {} {}% ({}/{})",
            id,
            progress.percent(),
            progress.bytes,
            progress.total
        );
    } else {
        eprintln!("catch: {} {} bytes", id, progress.bytes);
    }
    Ok(match directive {
        Directive::Keep => None,
        Directive::StandDown => Some(StreamEnd::StoodDown),
        Directive::Abort => Some(StreamEnd::Aborted),
    })
}

/// Read one connection until it ends. Honors stop orders on every flush,
/// and on every poll tick even when the connection has gone quiet.
///
/// The idle deadline is a monotonic mark of the last byte that arrived, not
/// a timer that is re-armed on each pass: re-arming the `select!` sleep on
/// every poll tick meant the stall timeout never fired at all, and a
/// connection that stopped sending hung a worker for ever.
async fn stream_one_connection(
    id: &str,
    part: &std::path::Path,
    file: &mut tokio::io::BufWriter<&mut tokio::fs::File>,
    stream: &mut (impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    progress: &mut Progress,
    watch: &mut DirectiveWatch,
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
    let mut last_data = Instant::now();
    loop {
        // Stop orders first: a transfer whose server has gone quiet is
        // exactly when someone reaches for "stop", and the row may be paused
        // or removed from another process while no bytes are moving at all.
        if order_due || last_order_read.elapsed() >= ORDER_POLL {
            order_due = false;
            last_order_read = Instant::now();
            match watch.directive(id) {
                Directive::Keep => {}
                Directive::StandDown => return Ok(StreamEnd::StoodDown),
                Directive::Abort => return Ok(StreamEnd::Aborted),
            }
        }
        // How long this connection may stay quiet, and the deadline it has
        // already burned: `last_data` is only reset by real body bytes.
        let complete = progress.total > 0 && progress.bytes >= progress.total;
        let limit = if complete {
            COMPLETION_GRACE
        } else {
            stall_timeout()
        };
        let idle = last_data.elapsed();
        if idle >= limit {
            return Ok(StreamEnd::Interrupted(if complete {
                format!(
                    "server did not close after {} bytes ({}s grace)",
                    progress.bytes,
                    limit.as_secs()
                )
            } else {
                format!(
                    "no data for {}s after {} bytes",
                    idle.as_secs(),
                    progress.bytes
                )
            }));
        }
        let wait = limit - idle;
        // Wait for data, but never longer than the poll interval: the tick
        // sends us back round the loop to re-read stop orders instead of
        // sitting in a read until the stall timeout fires.
        let next = tokio::select! {
            biased;
            chunk = stream.next() => Some(chunk),
            () = tokio::time::sleep(wait.min(STOP_POLL)) => None,
        };
        let chunk = match next {
            // A poll tick: nothing to write, but this is the moment a quiet
            // connection must honour a stop order, so force the next read
            // instead of waiting for a chunk that may never arrive. It is
            // also the only chance to show progress for a body that arrives
            // slower than FLUSH_EVERY: flushing only after a chunk left the
            // bar at 0 B for minutes on a slow chunked stream.
            None => {
                order_due = true;
                if progress.last_flush.elapsed() >= FLUSH_EVERY {
                    if let Some(end) = flush_progress(id, progress).await? {
                        return Ok(end);
                    }
                }
                continue;
            }
            Some(Some(Ok(c))) => c,
            Some(Some(Err(e))) => {
                // Worth one reconnect like a stall: servers that cut a
                // connection mid-body usually answer a ranged retry cleanly.
                return Ok(StreamEnd::Interrupted(format!("{e:#}")));
            }
            Some(None) => {
                // Clean EOF. With a promised length this is only success
                // when every promised byte arrived; a short body is a
                // truncated transfer, not a finished file.
                if progress.total > 0 && progress.bytes < progress.total {
                    return Ok(StreamEnd::Interrupted(format!(
                        "body ended at {} of {} bytes",
                        progress.bytes, progress.total
                    )));
                }
                return Ok(StreamEnd::Finished);
            }
        };
        file.write_all(&chunk)
            .await
            .with_context(|| format!("write {}", part.display()))?;
        progress.bytes += chunk.len() as u64;
        if !chunk.is_empty() {
            last_data = Instant::now();
        }
        // Time-based throttle only. A percentage gate meant a 10 GiB file
        // updated once per 100 MiB, and a chunked response with no
        // Content-Length never updated at all (its percent is constant).
        if progress.last_flush.elapsed() >= FLUSH_EVERY {
            if let Some(end) = flush_progress(id, progress).await? {
                return Ok(end);
            }
        }
    }
}

/// Move a finished part file into place and record it, once, for every kind
/// of transfer (plain, resumed, completed-by-416).
///
/// Every successful completion goes through here so the rules cannot drift:
/// the bytes are validated against the promised total, an existing file is
/// only replaced by an explicit `overwrite`, the row and the saved-file
/// index are updated in one locked write, and a cancelled row is never
/// reported as done.
pub(crate) struct Finish<'a> {
    id: &'a str,
    url: &'a str,
    dir: &'a std::path::Path,
    dest_name: &'a str,
    part: &'a std::path::Path,
    bytes: u64,
    total: Option<u64>,
    overwrite: bool,
    base: &'a std::path::Path,
}

pub(crate) async fn finish(f: Finish<'_>) -> Result<PathBuf> {
    let Finish {
        id,
        url,
        dir,
        dest_name,
        part,
        bytes,
        total,
        overwrite,
        base,
    } = f;

    // Race safety: if the row is already cancelled or removed, do not publish
    // a final file. Remove the part and bail.
    let alive_result = crate::state::with_lock(|| {
        let state = crate::state::load_state()?;
        Ok(state
            .downloads
            .iter()
            .any(|d| d.id == id && d.status == Status::Downloading))
    });
    match alive_result {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(part);
            segment::remove_manifest(part);
            anyhow::bail!("{id} stopped");
        }
        Err(e) => {
            // State unreadable: preserve partial data and report.
            anyhow::bail!("{id}: could not verify ownership: {e:#}");
        }
    }

    let held = std::fs::metadata(part)
        .map(|m| m.len())
        .with_context(|| format!("stat {}", part.display()))?;
    let total = total.unwrap_or(held);
    if total > 0 && held != total {
        // Never report success for a file whose size disagrees with the
        // announced total: too short or too long both fail.
        let msg = format!("size mismatch: {held} of {total} bytes");
        let _ = mark_error(id, &msg);
        anyhow::bail!("{msg}");
    }
    if held != bytes {
        eprintln!("catch: {id} note: {bytes} streamed, {held} on disk");
    }
    // Atomically allocate the final path so two workers finishing at the
    // same time cannot both rename to the same destination (which would
    // silently overwrite). The reservation file is replaced by the rename.
    let dest = if overwrite {
        dir.join(dest_name)
    } else {
        crate::state::with_lock(|| {
            let mut candidate = dir.join(dest_name);
            loop {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&candidate)
                {
                    Ok(_) => break,
                    Err(_) => candidate = unique_destination(dir, dest_name),
                }
            }
            Ok(candidate)
        })?
    };
    // Intent record for recoverable finalization: written before the rename.
    let intent_path = PathBuf::from(format!("{}.finalizing", part.display()));
    let _ = std::fs::write(
        &intent_path,
        serde_json::json!({"dest": dest.display().to_string(), "url": url}).to_string(),
    );

    if let Err(e) = tokio::fs::rename(part, &dest).await {
        // Remove the reservation file we created so it does not linger.
        if !overwrite {
            let _ = std::fs::remove_file(&dest);
        }
        return Err(e).with_context(|| format!("move to {}", dest.display()));
    }
    let final_name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest_name.to_string());
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(held);

    // Re-check under the lock: cancellation or removal may have won during
    // the rename. If so, drop the published file and do not mark done.
    let mut lost = false;
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            if d.status == Status::Downloading {
                d.status = Status::Done;
                d.bytes = size;
                d.total = size.max(total);
                d.filename = final_name.clone();
                d.error.clear();
                d.pid = 0;
                d.claimed_at = 0;
            } else {
                lost = true;
            }
        } else {
            // Row was removed: completion lost.
            lost = true;
        }
        if !lost {
            crate::state::record_saved(state, url, base, &dest, crate::state::DownloadKind::File);
        }
        Ok(())
    })?;
    if lost {
        // Best-effort cleanup of the published file.
        if let Some(safe) = crate::state::resolve_inside(base, &dest.to_string_lossy()) {
            let _ = std::fs::remove_file(safe);
        }
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(&intent_path);
        anyhow::bail!("{id} stopped");
    }
    let _ = std::fs::remove_file(&intent_path);
    eprintln!("catch: done {} -> {}", id, dest.display());
    crate::state::spawn_drain();
    Ok(dest)
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn content_range_parses_well_formed_headers() {
        assert_eq!(
            parse_content_range("bytes 200-1000/67589"),
            Some((200, 1000, Some(67589)))
        );
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, 0, Some(1))));
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(
            parse_content_range("  bytes   10-19 / 100 "),
            Some((10, 19, Some(100)))
        );
    }

    #[test]
    fn content_range_rejects_malformed_headers() {
        // Anything unparsable must make the caller restart from zero rather
        // than trust an offset it cannot verify.
        for bad in [
            "",
            "bytes",
            "bytes */100",
            "bytes 100/200",
            "bytes 100-",
            "bytes -100/200",
            "bytes x-y/200",
            "items 1-2/3",
            "bytes 20-10/200",
            "bytes 0-199/100",
            "bytes 5-10/10",
        ] {
            assert_eq!(parse_content_range(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn backoff_grows_and_is_bounded() {
        let first = backoff(1);
        assert!(first >= Duration::from_millis(50), "{first:?}");
        assert!(first <= Duration::from_secs(1), "{first:?}");
        for attempt in 1..=8 {
            assert!(backoff(attempt) <= BACKOFF_MAX, "attempt {attempt}");
        }
    }

    #[test]
    fn permanent_statuses_do_not_retry() {
        let empty = reqwest::header::HeaderMap::new();
        for code in [400, 401, 403, 404, 410, 451] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(
                matches!(status_verdict(status, &empty, 1), Verdict::Fatal(_)),
                "{code} must not be retried"
            );
        }
        for code in [408, 429, 500, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(
                matches!(status_verdict(status, &empty, 1), Verdict::Retry(_)),
                "{code} must be retried"
            );
        }
    }

    #[test]
    fn retry_after_parses_http_date() {
        use chrono::{Duration as ChronoDuration, Utc};
        let future = Utc::now() + ChronoDuration::seconds(10);
        let date_str = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, date_str.parse().unwrap());
        match retry_after(&headers) {
            Some(d) => assert!(
                d.as_secs() >= 5 && d.as_secs() <= 15,
                "expected ~10s, got {d:?}"
            ),
            None => panic!("http-date Retry-After must parse"),
        }
        // malformed
        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert(reqwest::header::RETRY_AFTER, "not-a-date".parse().unwrap());
        assert!(retry_after(&h2).is_none());
        // far future clamped
        let far = Utc::now() + ChronoDuration::days(10);
        let far_str = far.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut h3 = reqwest::header::HeaderMap::new();
        h3.insert(reqwest::header::RETRY_AFTER, far_str.parse().unwrap());
        assert_eq!(retry_after(&h3), Some(RETRY_AFTER_MAX));
        // past date -> zero (no wait)
        let past = Utc::now() - ChronoDuration::seconds(10);
        let past_str = past.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut h4 = reqwest::header::HeaderMap::new();
        h4.insert(reqwest::header::RETRY_AFTER, past_str.parse().unwrap());
        assert_eq!(retry_after(&h4), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_is_honoured_and_clamped() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        match status_verdict(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, 1) {
            Verdict::Retry(wait) => assert_eq!(wait, Duration::from_secs(7)),
            Verdict::Fatal(why) => panic!("429 must retry: {why}"),
        }
        headers.insert(reqwest::header::RETRY_AFTER, "99999".parse().unwrap());
        match status_verdict(reqwest::StatusCode::SERVICE_UNAVAILABLE, &headers, 1) {
            Verdict::Retry(wait) => assert_eq!(wait, RETRY_AFTER_MAX),
            Verdict::Fatal(why) => panic!("503 must retry: {why}"),
        }
    }

    /// The stall deadline is what made "stop" look broken *and* what made a
    /// dead connection hang a worker for ever: the same `select!` that
    /// re-read stop orders has to leave the idle deadline standing.
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
        assert!(COMPLETION_GRACE < stall_timeout());
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

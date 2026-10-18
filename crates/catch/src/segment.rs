//! Segmented (range) downloading for eligible large files.
//!
//! One worker process still owns one queue entry. When the server proves it
//! supports real byte ranges, that worker splits the representation into
//! contiguous ranges and fetches them concurrently, writing each range into
//! its own slice of the shared `.part` file with a separate file handle (no
//! shared seek position). A small sidecar manifest (`.part.meta`) records
//! which ranges are durably committed, so a crash or a restart resumes only
//! the missing ones.
//!
//! Everything here that can fail safely degrades to the caller's sequential
//! downloader: a server that answers 200, sends a mismatched Content-Range,
//! changes validators between requests, or rejects a range makes us restart
//! from zero through the ordinary path.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::download::{
    backoff, max_attempts, parse_content_range, response_timeout, stall_timeout, status_verdict,
    transport_verdict,
};
use crate::state::Status;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentRange {
    pub start: u64,
    pub end: u64, // inclusive
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub version: u32,
    pub url: String,
    pub total: u64,
    #[serde(default)]
    pub etag: String,
    #[serde(default)]
    pub last_modified: String,
    pub segments: Vec<SegmentRange>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

pub const MANIFEST_VERSION: u32 = 1;

pub fn manifest_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_os_string();
    s.push(".meta");
    PathBuf::from(s)
}

pub fn remove_manifest(part: &Path) {
    let _ = std::fs::remove_file(manifest_path(part));
}

fn now_secs() -> u64 {
    crate::state::now_secs()
}

fn remove_both(part: &Path, meta: &Path) {
    let _ = std::fs::remove_file(meta);
    let _ = std::fs::remove_file(part);
}

pub fn load_manifest(part: &Path, url: &str) -> Result<Option<SegmentManifest>> {
    let meta = manifest_path(part);
    if !meta.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&meta) {
        Ok(t) => t,
        Err(_) => {
            remove_both(part, &meta);
            return Ok(None);
        }
    };
    let m: SegmentManifest = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(_) => {
            remove_both(part, &meta);
            return Ok(None);
        }
    };
    if m.version != MANIFEST_VERSION || m.url != url || m.total == 0 || m.segments.is_empty() {
        remove_both(part, &meta);
        return Ok(None);
    }
    if !validate_coverage(&m.segments, m.total) {
        remove_both(part, &meta);
        return Ok(None);
    }
    if !part.is_file() {
        let _ = std::fs::remove_file(&meta);
        return Ok(None);
    }
    let plen = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if plen != m.total {
        remove_both(part, &meta);
        return Ok(None);
    }
    Ok(Some(m))
}

fn validate_coverage(segs: &[SegmentRange], total: u64) -> bool {
    if segs.is_empty() {
        return false;
    }
    if segs[0].start != 0 {
        return false;
    }
    let mut prev_end: Option<u64> = None;
    for s in segs {
        if s.end < s.start {
            return false;
        }
        if let Some(pe) = prev_end {
            if pe + 1 != s.start {
                return false;
            }
        }
        prev_end = Some(s.end);
    }
    prev_end.unwrap() == total.saturating_sub(1)
}

pub fn save_manifest_atomic(part: &Path, m: &SegmentManifest) -> Result<()> {
    let meta = manifest_path(part);
    let tmp = meta.with_extension("meta.tmp");
    let text = serde_json::to_string_pretty(m)?;
    std::fs::write(&tmp, &text).with_context(|| format!("write {}", tmp.display()))?;
    let f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
    let _ = f.sync_data();
    drop(f);
    std::fs::rename(&tmp, &meta).with_context(|| format!("rename to {}", meta.display()))?;
    Ok(())
}

// ---------- eligibility / budget ----------

pub fn min_file_bytes() -> u64 {
    if let Ok(raw) = std::env::var("CATCH_SEGMENT_MIN_BYTES") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if n >= 1024 {
                return n;
            }
        }
    }
    32 * 1024 * 1024
}

pub fn min_piece_bytes() -> u64 {
    if let Ok(raw) = std::env::var("CATCH_SEGMENT_MIN_PIECE") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            if n >= 256 {
                return n;
            }
        }
    }
    8 * 1024 * 1024
}

pub fn connection_budget() -> usize {
    if let Ok(raw) = std::env::var("CATCH_MAX_CONNECTIONS") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if (1..=256).contains(&n) {
                return n;
            }
        }
    }
    8
}

pub fn segments_cap() -> usize {
    crate::state::segments_setting()
}

pub fn segment_share(active: usize) -> usize {
    let cap = segments_cap();
    let budget = connection_budget();
    let active = active.max(1);
    (budget / active).clamp(1, cap)
}

pub fn segment_count(total: u64, share: usize) -> usize {
    let cap = share.max(1);
    let by_size = total.div_ceil(min_piece_bytes()).max(1) as usize;
    cap.min(by_size).max(1)
}

pub fn eligible(total: u64, share: usize) -> bool {
    if total < min_file_bytes() {
        return false;
    }
    if share < 2 {
        return false;
    }
    let n = segment_count(total, share);
    n >= 2
}

pub fn segment_ranges(total: u64, n: usize) -> Vec<SegmentRange> {
    let n = n.max(1);
    let base = total / n as u64;
    let rem = (total % n as u64) as usize;
    let mut out = Vec::with_capacity(n);
    let mut pos = 0u64;
    for i in 0..n {
        let len = base + if i < rem { 1 } else { 0 };
        if len == 0 {
            break;
        }
        let end = pos + len - 1;
        out.push(SegmentRange {
            start: pos,
            end,
            done: false,
        });
        pos = end + 1;
    }
    out
}

// ---------- outcome ----------

#[derive(Debug)]
pub enum SegmentedResult {
    Complete { manifest: SegmentManifest },
    Paused,
    Stopped,
    Fallback(String),
}

// ---------- runner ----------

enum SegError {
    Fatal(String),
    Fallback(String),
    Interrupted(String),
}

#[derive(Clone, Copy)]
enum Directive {
    Keep,
    StandDown,
    Abort,
}

fn read_directive(id: &str) -> Directive {
    match crate::state::load_state().ok().and_then(|s| {
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

#[allow(clippy::too_many_arguments)]
pub async fn run_segmented(
    id: &str,
    url: &str,
    client: &reqwest::Client,
    part: &Path,
    total: u64,
    etag: String,
    last_modified: String,
    manifest: SegmentManifest,
    share: usize,
) -> Result<SegmentedResult> {
    let n = manifest.segments.len();
    let written: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());
    let done_flags: Arc<Vec<AtomicBool>> = Arc::new(
        (0..n)
            .map(|i| AtomicBool::new(manifest.segments[i].done))
            .collect(),
    );
    // seed written for already-done segments (informational; they are done)
    let manifest = Arc::new(Mutex::new(manifest));
    let cancel = Arc::new(AtomicU8::new(0)); // 0 run, 1 pause, 2 abort/fail

    let mut pending: std::collections::VecDeque<usize> = (0..n)
        .filter(|&i| !done_flags[i].load(Ordering::Relaxed))
        .collect();

    let mut joinset: tokio::task::JoinSet<Result<(), SegError>> = tokio::task::JoinSet::new();

    let mut first_fatal: Option<String> = None;
    let mut first_fallback: Option<String> = None;

    let mut last_flush = Instant::now();
    let mut last_order = Instant::now();

    loop {
        // spawn as many as the budget allows
        while joinset.len() < share && !pending.is_empty() && cancel.load(Ordering::Relaxed) == 0 {
            let i = pending.front().copied().unwrap();
            let seg = {
                let m = manifest.lock().unwrap();
                m.segments[i].clone()
            };
            let seg_len = seg.end - seg.start + 1;
            // reset per-segment written counter for a fresh attempt (resume handled inside task via in_seg)
            let task_written = Arc::clone(&written);
            let task_done = Arc::clone(&done_flags);
            let task_manifest = Arc::clone(&manifest);
            let task_cancel = Arc::clone(&cancel);
            let url = url.to_string();
            let client = client.clone();
            let part = part.to_path_buf();
            let etag = etag.clone();
            let last_modified = last_modified.clone();
            joinset.spawn(run_one_segment(
                i,
                seg,
                seg_len,
                url,
                client,
                part,
                total,
                etag,
                last_modified,
                task_manifest,
                task_cancel,
                task_written,
                task_done,
            ));
            pending.pop_front();
        }

        if pending.is_empty() && joinset.is_empty() {
            break;
        }

        // wait for an event or a tick
        let mut got = false;
        tokio::select! {
            biased;
            res = joinset.join_next(), if !joinset.is_empty() => {
                got = true;
                match res {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(SegError::Fallback(why)))) => {
                        if first_fallback.is_none() { first_fallback = Some(why); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    Some(Ok(Err(SegError::Fatal(why)))) => {
                        if first_fatal.is_none() { first_fatal = Some(why); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    Some(Ok(Err(SegError::Interrupted(why)))) => {
                        // A segment's connection died and exhausted... actually Interrupted is per-attempt; run_one_segment retries internally.
                        // If we see it here, attempts exhausted → treat as fatal.
                        if first_fatal.is_none() { first_fatal = Some(why); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    Some(Err(e)) => {
                        if first_fatal.is_none() { first_fatal = Some(e.to_string()); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    None => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        let _ = got;

        // directive check
        if last_order.elapsed() >= Duration::from_millis(100) {
            last_order = Instant::now();
            match read_directive(id) {
                Directive::StandDown => cancel.store(1, Ordering::Relaxed),
                Directive::Abort => cancel.store(2, Ordering::Relaxed),
                Directive::Keep => {}
            }
        }

        // cancel handling
        if cancel.load(Ordering::Relaxed) != 0 {
            joinset.abort_all();
            while joinset.join_next().await.is_some() {}
            flush_progress(id, total, &manifest, &done_flags, &written).await;
            let c = cancel.load(Ordering::Relaxed);
            if c == 1 {
                return Ok(SegmentedResult::Paused);
            }
            if let Some(why) = first_fallback.take() {
                return Ok(SegmentedResult::Fallback(why));
            }
            if let Some(why) = first_fatal.take() {
                return Err(anyhow::anyhow!(why));
            }
            return Ok(SegmentedResult::Stopped);
        }

        // periodic flush
        if last_flush.elapsed() >= crate::download::FLUSH_EVERY {
            last_flush = Instant::now();
            flush_progress(id, total, &manifest, &done_flags, &written).await;
        }
    }

    // all done: verify
    for i in 0..n {
        if !done_flags[i].load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("segment {} did not complete", i));
        }
    }
    flush_progress(id, total, &manifest, &done_flags, &written).await;
    let final_manifest = {
        let m = manifest.lock().unwrap();
        m.clone()
    };
    Ok(SegmentedResult::Complete {
        manifest: final_manifest,
    })
}

async fn flush_progress(
    id: &str,
    total: u64,
    manifest: &Arc<Mutex<SegmentManifest>>,
    done_flags: &Arc<Vec<AtomicBool>>,
    written: &Arc<Vec<AtomicU64>>,
) {
    let n = done_flags.len();
    let mut done_bytes: u64 = 0;
    for i in 0..n {
        if done_flags[i].load(Ordering::Relaxed) {
            let m = manifest.lock().unwrap();
            done_bytes += m.segments[i].end - m.segments[i].start + 1;
        }
    }
    let mut live: u64 = 0;
    for i in 0..n {
        if !done_flags[i].load(Ordering::Relaxed) {
            live += written[i].load(Ordering::Relaxed);
        }
    }
    let bytes = done_bytes + live;
    let done_count = done_flags
        .iter()
        .filter(|f| f.load(Ordering::Relaxed))
        .count();
    let _ = crate::state::update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            if d.status == Status::Downloading {
                d.bytes = bytes;
                d.total = total;
                let mut obj = serde_json::Map::new();
                obj.insert("total".into(), serde_json::json!(n));
                obj.insert("done".into(), serde_json::json!(done_count));
                d.extra
                    .insert("segments".into(), serde_json::Value::Object(obj));
            }
        }
        Ok(())
    });
}

// ---------- one segment ----------

#[allow(clippy::too_many_arguments)]
async fn run_one_segment(
    i: usize,
    seg: SegmentRange,
    seg_len: u64,
    url: String,
    client: reqwest::Client,
    part: PathBuf,
    total: u64,
    etag: String,
    last_modified: String,
    manifest: Arc<Mutex<SegmentManifest>>,
    cancel: Arc<AtomicU8>,
    written: Arc<Vec<AtomicU64>>,
    done_flags: Arc<Vec<AtomicBool>>,
) -> Result<(), SegError> {
    use reqwest::header;
    use tokio::io::AsyncWriteExt;

    let mut in_seg: u64 = 0;
    let mut attempts = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) != 0 {
            return Ok(());
        }
        if in_seg >= seg_len {
            break;
        }
        if attempts >= max_attempts() {
            return Err(SegError::Fatal(format!(
                "segment {i} gave up after {attempts} attempts"
            )));
        }
        attempts += 1;

        let want_start = seg.start + in_seg;
        let want_end = seg.end;

        let mut req = client.get(&url).header(header::ACCEPT_ENCODING, "identity");
        req = req.header(header::RANGE, format!("bytes={}-{}", want_start, want_end));
        let has_validator = !etag.is_empty() || !last_modified.is_empty();
        if has_validator {
            let v = if !etag.is_empty() {
                etag.clone()
            } else {
                last_modified.clone()
            };
            req = req.header(header::IF_RANGE, v);
        }

        let sent = tokio::time::timeout(response_timeout(), req.send()).await;
        let resp = match sent {
            Err(_) => {
                let why = format!(
                    "no response headers within {}s",
                    response_timeout().as_secs()
                );
                let wait = backoff(attempts);
                eprintln!(
                    "catch: segment {i}: {why}; retry in {:.1}s",
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            Ok(Err(e)) => match transport_verdict(&e, attempts) {
                crate::download::Verdict::Fatal(why) => return Err(SegError::Fatal(why)),
                crate::download::Verdict::Retry(wait) => {
                    eprintln!(
                        "catch: segment {i}: {e}; retry in {:.1}s",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            },
            Ok(Ok(r)) => r,
        };

        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let cr = resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok());
            let parsed = cr.and_then(parse_content_range);
            let (rs, re, rt) = match parsed {
                Some(t) => t,
                None => {
                    let wait = backoff(attempts);
                    eprintln!(
                        "catch: segment {i}: malformed Content-Range; retry in {:.1}s",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };
            if rs != want_start || re != want_end || rt != Some(total) {
                if rt.is_some() && rt != Some(total) {
                    return Err(SegError::Fallback(format!(
                        "segment {i}: representation changed (total {} vs {})",
                        rt.unwrap_or(0),
                        total
                    )));
                }
                let wait = backoff(attempts);
                eprintln!(
                    "catch: segment {i}: unexpected Content-Range {}-{} (want {}-{}); retry in {:.1}s",
                    rs, re, want_start, want_end, wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            // validator checks
            let resp_etag = resp
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok());
            let resp_modified = resp
                .headers()
                .get(header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok());
            if !etag.is_empty() && resp_etag.is_some_and(|e| e != etag) {
                return Err(SegError::Fallback(format!(
                    "segment {i}: representation changed (etag)"
                )));
            }
            if !last_modified.is_empty() && resp_modified.is_some_and(|lm| lm != last_modified) {
                return Err(SegError::Fallback(format!(
                    "segment {i}: representation changed (last-modified)"
                )));
            }
            // content encoding
            let encoded = resp
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .map(|v| !v.eq_ignore_ascii_case("identity"))
                .unwrap_or(false);
            if encoded {
                return Err(SegError::Fallback(format!(
                    "segment {i}: non-identity content encoding"
                )));
            }
            let cl = resp.content_length();
            let expected_cl = seg_len - in_seg;
            if cl.is_some() && cl != Some(expected_cl) {
                let wait = backoff(attempts);
                eprintln!(
                    "catch: segment {i}: Content-Length mismatch ({} vs {}); retry in {:.1}s",
                    cl.unwrap_or(0),
                    expected_cl,
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                continue;
            }

            // stream body, bounded to seg_len - in_seg
            let mut file = match tokio::fs::OpenOptions::new().write(true).open(&part).await {
                Ok(f) => f,
                Err(e) => return Err(SegError::Fatal(format!("open {}: {e}", part.display()))),
            };
            use tokio::io::AsyncSeekExt;
            if file
                .seek(std::io::SeekFrom::Start(want_start))
                .await
                .is_err()
            {
                return Err(SegError::Fatal(format!("seek {}", part.display())));
            }
            let mut buf = tokio::io::BufWriter::with_capacity(256 * 1024, &mut file);
            let mut stream = resp.bytes_stream();
            let mut last_data = Instant::now();
            let mut attempt_bytes: u64 = 0;
            let remaining = seg_len - in_seg;
            let mut oversized = false;
            loop {
                if cancel.load(Ordering::Relaxed) != 0 {
                    let _ = buf.flush().await;
                    return Ok(());
                }
                if attempt_bytes >= remaining {
                    break;
                }
                let limit = stall_timeout();
                let idle = last_data.elapsed();
                if idle >= limit {
                    let _ = buf.flush().await;
                    return Err(SegError::Interrupted(format!(
                        "segment {i}: no data for {}s",
                        idle.as_secs()
                    )));
                }
                let wait = limit - idle;
                let next = tokio::select! {
                    biased;
                    chunk = stream.next() => Some(chunk),
                    () = tokio::time::sleep(wait.min(crate::download::STOP_POLL)) => None,
                };
                let chunk = match next {
                    None => continue,
                    Some(Some(Ok(c))) => c,
                    Some(Some(Err(e))) => {
                        let _ = buf.flush().await;
                        return Err(SegError::Interrupted(format!("segment {i}: {e:#}")));
                    }
                    Some(None) => {
                        let _ = buf.flush().await;
                        if attempt_bytes < remaining {
                            return Err(SegError::Interrupted(format!(
                                "segment {i}: body ended at {} of {} bytes",
                                attempt_bytes, remaining
                            )));
                        }
                        break;
                    }
                };
                if chunk.is_empty() {
                    continue;
                }
                let space = remaining - attempt_bytes;
                if (chunk.len() as u64) > space {
                    // oversized body: protocol violation
                    let to_write = space as usize;
                    if to_write > 0 {
                        if buf.write_all(&chunk[..to_write]).await.is_err() {
                            let _ = buf.flush().await;
                            return Err(SegError::Fatal(format!("write {}", part.display())));
                        }
                        attempt_bytes += to_write as u64;
                    }
                    let _ = buf.flush().await;
                    oversized = true;
                    break;
                }
                if buf.write_all(&chunk).await.is_err() {
                    let _ = buf.flush().await;
                    return Err(SegError::Fatal(format!("write {}", part.display())));
                }
                attempt_bytes += chunk.len() as u64;
                written[i].store(in_seg + attempt_bytes, Ordering::Relaxed);
                if !chunk.is_empty() {
                    last_data = Instant::now();
                }
            }
            let _ = buf.shutdown().await;
            drop(buf);
            drop(file);

            if oversized {
                return Err(SegError::Interrupted(format!(
                    "segment {i}: server sent extra bytes"
                )));
            }
            if attempt_bytes < remaining {
                // shouldn't reach here except via break with full remaining
            }
            in_seg += attempt_bytes;
            written[i].store(in_seg, Ordering::Relaxed);
            // if we got all bytes for this segment, done
            if in_seg >= seg_len {
                break;
            }
            // otherwise retry the rest
            continue;
        } else if status == reqwest::StatusCode::OK {
            return Err(SegError::Fallback(format!(
                "segment {i}: server answered 200 to a ranged request"
            )));
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(SegError::Fallback(format!(
                "segment {i}: server refused the range (416)"
            )));
        } else if !status.is_success() {
            match status_verdict(status, resp.headers(), attempts) {
                crate::download::Verdict::Fatal(why) => return Err(SegError::Fatal(why)),
                crate::download::Verdict::Retry(wait) => {
                    let wait = wait.max(backoff(attempts));
                    eprintln!(
                        "catch: segment {i}: server replied {status}; retry in {:.1}s",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
        } else {
            let wait = backoff(attempts);
            tokio::time::sleep(wait).await;
            continue;
        }
    }

    // durably commit: sync the file, then the manifest
    let f = std::fs::OpenOptions::new().write(true).open(&part);
    if let Ok(f) = f {
        let _ = f.sync_data();
    }
    {
        let mut m = manifest.lock().unwrap();
        if i < m.segments.len() {
            m.segments[i].done = true;
        }
        m.updated_at = now_secs();
        if let Err(e) = save_manifest_atomic(&part, &m) {
            return Err(SegError::Fatal(format!("manifest write: {e:#}")));
        }
    }
    done_flags[i].store(true, Ordering::Relaxed);
    written[i].store(seg_len, Ordering::Relaxed);
    Ok(())
}

// ---------- helpers ----------

// FLUSH_EVERY re-export for convenience
// (crate::download::FLUSH_EVERY is private; use a local constant)
// We reference crate::download::... only public(crate) items; FLUSH_EVERY is not exposed.
// So define locally:
// (not used; runner uses its own 250ms tick)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_exactly() {
        for (total, n) in [
            (100u64, 1usize),
            (100, 2),
            (100, 3),
            (100, 4),
            (10, 3),
            (1, 1),
            (7, 8),
            (1000, 7),
        ] {
            let r = segment_ranges(total, n);
            if total == 0 {
                continue;
            }
            assert_eq!(r.first().unwrap().start, 0);
            assert_eq!(r.last().unwrap().end, total - 1);
            for w in r.windows(2) {
                assert_eq!(w[0].end + 1, w[1].start);
            }
            let sum: u64 = r.iter().map(|s| s.end - s.start + 1).sum();
            assert_eq!(sum, total, "total {} n {}", total, n);
        }
    }

    #[test]
    fn ranges_handle_remainder() {
        // 10 bytes / 3 segments: 4,3,3
        let r = segment_ranges(10, 3);
        assert_eq!(
            r[0],
            SegmentRange {
                start: 0,
                end: 3,
                done: false
            }
        );
        assert_eq!(
            r[1],
            SegmentRange {
                start: 4,
                end: 6,
                done: false
            }
        );
        assert_eq!(
            r[2],
            SegmentRange {
                start: 7,
                end: 9,
                done: false
            }
        );
    }

    #[test]
    fn validate_coverage_rejects_gaps() {
        let good = vec![
            SegmentRange {
                start: 0,
                end: 4,
                done: false,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
            },
        ];
        assert!(validate_coverage(&good, 10));
        let gap = vec![
            SegmentRange {
                start: 0,
                end: 3,
                done: false,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
            },
        ];
        assert!(!validate_coverage(&gap, 10));
        let overlap = vec![
            SegmentRange {
                start: 0,
                end: 5,
                done: false,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
            },
        ];
        assert!(!validate_coverage(&overlap, 10));
        let wrong_total = vec![SegmentRange {
            start: 0,
            end: 9,
            done: false,
        }];
        assert!(!validate_coverage(&wrong_total, 11));
    }

    #[test]
    fn manifest_round_trips() {
        let dir = std::env::temp_dir().join(format!("catch-seg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("file.bin.1.part");
        std::fs::write(&part, vec![0u8; 100]).unwrap();
        let m = SegmentManifest {
            version: MANIFEST_VERSION,
            url: "https://x.test/f".into(),
            total: 100,
            etag: "\"v1\"".into(),
            last_modified: String::new(),
            segments: segment_ranges(100, 4),
            created_at: 1,
            updated_at: 1,
        };
        save_manifest_atomic(&part, &m).unwrap();
        let loaded = load_manifest(&part, "https://x.test/f").unwrap().unwrap();
        assert_eq!(loaded.total, 100);
        assert_eq!(loaded.segments.len(), 4);
        assert!(manifest_path(&part).exists());
        // corrupt url → recovered (both deleted)
        std::fs::write(&part, vec![0u8; 100]).unwrap();
        // reload with wrong url
        assert!(load_manifest(&part, "https://other/").unwrap().is_none());
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eligible_respects_thresholds() {
        // defaults 32MiB file, 8MiB piece, share 4
        assert!(!eligible(1024 * 1024, 4)); // too small
                                            // use env to lower for this test? env is process-global; just check share gating
        assert!(!eligible(40 * 1024 * 1024, 1)); // share<2
    }
}

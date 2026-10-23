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
    /// Bytes at the start of this range that are already on disk. For an
    /// unfinished segment this is the resume point; writes are sequential
    /// from `start`, so the coverage is always a prefix. Kept separate from
    /// `done` so a pause does not throw away a partially downloaded range.
    #[serde(default)]
    pub have: u64,
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

/// Size of each segment task's userspace write buffer. A checkpoint may only
/// advance `have` past bytes that reached the file, so an active task's
/// frontier is held back by at most this much.
const SEGMENT_WRITE_BUFFER: u64 = 256 * 1024;

/// How often partial segment progress is fsynced and written to the
/// manifest. This is deliberately slower than the UI progress flush: each
/// checkpoint must sync the part file before the manifest can name a resume
/// offset, and doing that four times a second would be wasteful.
const SEGMENT_CHECKPOINT_EVERY: Duration = Duration::from_secs(5);

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

pub fn load_manifest(part: &Path, url: &str) -> Result<Option<SegmentManifest>> {
    let meta = manifest_path(part);
    if !meta.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&meta) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // A transient read error must not read as "no manifest": the
            // part is preallocated to full size, and treating it as a plain
            // partial file once published a file of holes as complete.
            // Fail the transfer instead; both files stay for the next try.
            anyhow::bail!("cannot read manifest {}: {e}", meta.display());
        }
    };
    let m: SegmentManifest = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "catch: corrupt manifest {}: {e}; quarantining part",
                meta.display()
            );
            let _ = std::fs::remove_file(&meta);
            quarantine_part(part);
            return Ok(None);
        }
    };
    if m.version != MANIFEST_VERSION
        || m.url != url
        || m.total == 0
        || m.segments.is_empty()
        || !validate_coverage(&m.segments, m.total)
    {
        eprintln!(
            "catch: incompatible manifest {}; quarantining part",
            meta.display()
        );
        let _ = std::fs::remove_file(&meta);
        quarantine_part(part);
        return Ok(None);
    }
    if !part.is_file() {
        let _ = std::fs::remove_file(&meta);
        return Ok(None);
    }
    let plen = match std::fs::metadata(part) {
        Ok(md) => md.len(),
        Err(e) => {
            // The part existed a moment ago (`is_file()` above); a stat
            // error now is transient I/O trouble, not a missing part.
            anyhow::bail!("cannot stat part {}: {e}", part.display());
        }
    };
    if plen != m.total {
        eprintln!(
            "catch: part size mismatch ({} vs {}); quarantining part",
            plen, m.total
        );
        let _ = std::fs::remove_file(&meta);
        quarantine_part(part);
        return Ok(None);
    }
    Ok(Some(m))
}

/// (total bytes, done segments, segment count) for `catch why`; `None` when
/// no usable manifest exists (sequential transfer, finished download, or a
/// quarantined/corrupt manifest).
pub fn manifest_summary(part: &Path, url: &str) -> Option<(u64, usize, usize)> {
    let m = load_manifest(part, url).ok().flatten()?;
    let done = m.segments.iter().filter(|s| s.done).count();
    Some((m.total, done, m.segments.len()))
}

fn quarantine_part(part: &Path) {
    if !part.is_file() {
        return;
    }
    let q = part.with_extension(format!("part.quarantined-{}", crate::state::now_secs()));
    let _ = std::fs::rename(part, &q);
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
        if s.have > s.end - s.start + 1 {
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
    f.sync_data()
        .with_context(|| format!("sync {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, &meta).with_context(|| format!("rename to {}", meta.display()))?;
    // Best-effort directory sync for crash durability of the rename.
    if let Some(parent) = meta.parent() {
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
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
    // One place decides the global connection budget: the cross-process
    // permit registry (environment override, then the traffic mode).
    crate::conns::budget()
}

pub fn segments_cap() -> usize {
    crate::state::segments_setting()
}

pub fn segment_share(active: usize) -> usize {
    let cap = segments_cap();
    let active = active.max(1);
    let share = (connection_budget() / active).clamp(1, cap);
    // A bandwidth cap can only feed so many full-rate connections; beyond
    // that the byte limiter would just serialize them.
    match crate::state::cap_slices() {
        Some(slices) => share.min((slices / active).max(1)),
        None => share,
    }
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
            have: 0,
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

/// One contiguous piece of one segment: the unit of work the runner
/// schedules. Every work range ends at its segment's end, so a task that
/// covers its whole work has covered its whole segment tail on top of
/// everything earlier works of that segment wrote.
#[derive(Clone, Copy, Debug)]
struct Work {
    seg: usize,
    start: u64,
    end: u64,
}

impl Work {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// What a segment task reports back. `covered` is its own coverage from
/// `work.start`; less than `work.len()` on `Finished` means it stopped on
/// purpose so an idle slot can pick up the remainder.
enum TaskEnd {
    Finished {
        work: Work,
        covered: u64,
    },
    GaveUp {
        work: Work,
        covered: u64,
        why: String,
    },
}

#[derive(Clone, Copy)]
enum Directive {
    Keep,
    StandDown,
    Abort,
    /// State could not be read; preserve data and keep going.
    Unknown,
}

fn read_directive(id: &str) -> Directive {
    let state = match crate::state::load_state() {
        Ok(s) => s,
        Err(_) => return Directive::Unknown,
    };
    match state
        .downloads
        .into_iter()
        .find(|d| d.id == id)
        .map(|d| d.status)
    {
        Some(Status::Downloading) => Directive::Keep,
        Some(Status::Paused) => Directive::StandDown,
        Some(_) => Directive::Abort,
        None => {
            // Row gone: could be removed (Abort) or state unreadable. We
            // already read state successfully, so treat missing row as
            // cancelled/removed.
            Directive::Abort
        }
    }
}

/// Normalize the manifest into the in-memory resume state. A segment whose
/// prefix covers its whole range is complete even if a crash happened after
/// the data checkpoint but before the `done` flag was written.
fn resume_state(manifest: &mut SegmentManifest) -> (Vec<bool>, Vec<u64>) {
    manifest
        .segments
        .iter_mut()
        .map(|s| {
            let len = s.end - s.start + 1;
            let have = if s.done { len } else { s.have.min(len) };
            if have == len {
                s.done = true;
            }
            s.have = have;
            (s.done, have)
        })
        .unzip()
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
    mut manifest: SegmentManifest,
    share: usize,
    limiter: Option<Arc<crate::throttle::RateLimiter>>,
) -> Result<SegmentedResult> {
    let n = manifest.segments.len();
    let (done, have) = resume_state(&mut manifest);
    let written: Arc<Vec<AtomicU64>> = Arc::new(have.into_iter().map(AtomicU64::new).collect());
    let done_flags: Arc<Vec<AtomicBool>> =
        Arc::new(done.into_iter().map(AtomicBool::new).collect());
    // A split instruction per segment: the absolute offset a running task
    // should stop at so its tail becomes fresh work. u64::MAX = no split.
    let split_at: Arc<Vec<AtomicU64>> =
        Arc::new((0..n).map(|_| AtomicU64::new(u64::MAX)).collect());
    let seg_starts: Vec<u64> = manifest.segments.iter().map(|s| s.start).collect();
    let manifest = Arc::new(Mutex::new(manifest));
    let cancel = Arc::new(AtomicU8::new(0)); // 0 run, 1 pause, 2 abort/fail

    let mut pending: std::collections::VecDeque<Work> = manifest
        .lock()
        .unwrap()
        .segments
        .iter()
        .enumerate()
        .filter(|(i, _)| !done_flags[*i].load(Ordering::Relaxed))
        .map(|(i, s)| Work {
            seg: i,
            start: s.start + written[i].load(Ordering::Relaxed),
            end: s.end,
        })
        .collect();
    // At most one work item per segment runs at a time, so the per-segment
    // `written` frontier stays meaningful.
    let mut running: std::collections::HashMap<usize, Work> = std::collections::HashMap::new();
    // One re-queue per segment: after the second exhaustion, fail.
    let mut requeues = vec![0u8; n];

    let mut joinset: tokio::task::JoinSet<Result<TaskEnd, SegError>> = tokio::task::JoinSet::new();

    let mut first_fatal: Option<String> = None;
    let mut first_fallback: Option<String> = None;

    let mut last_flush = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut last_order = Instant::now();

    loop {
        // spawn as many as the budget allows
        while joinset.len() < share && !pending.is_empty() && cancel.load(Ordering::Relaxed) == 0 {
            let work = pending.pop_front().unwrap();
            running.insert(work.seg, work);
            split_at[work.seg].store(u64::MAX, Ordering::Relaxed);
            let task_written = Arc::clone(&written);
            let task_done = Arc::clone(&done_flags);
            let task_manifest = Arc::clone(&manifest);
            let task_cancel = Arc::clone(&cancel);
            let task_split = Arc::clone(&split_at);
            let task_limiter = limiter.clone();
            let url = url.to_string();
            let client = client.clone();
            let part = part.to_path_buf();
            let etag = etag.clone();
            let last_modified = last_modified.clone();
            joinset.spawn(run_one_segment(
                work,
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
                task_split,
                task_limiter,
            ));
        }

        // Idle capacity with nothing pending: split the slowest running range
        // so the free slot helps the straggler instead of waiting for it.
        // The split is only an instruction — the victim reports its exact
        // frontier as it stops, and that frontier becomes the tail work.
        if pending.is_empty()
            && running.len() < share
            && share >= 2
            && cancel.load(Ordering::Relaxed) == 0
        {
            let mut best: Option<(usize, u64, u64)> = None; // seg, remaining, claim
            for (&seg, w) in &running {
                if split_at[seg].load(Ordering::Relaxed) != u64::MAX {
                    continue;
                }
                let claim = seg_starts[seg] + written[seg].load(Ordering::Relaxed);
                if claim <= w.start {
                    // No progress on this work yet: splitting it would only
                    // re-queue the same range.
                    continue;
                }
                let remaining = w.end + 1 - claim;
                if remaining <= min_piece_bytes() {
                    continue;
                }
                if best.map(|(_, r, _)| remaining > r).unwrap_or(true) {
                    best = Some((seg, remaining, claim));
                }
            }
            if let Some((seg, _, claim)) = best {
                split_at[seg].store(claim, Ordering::Relaxed);
            }
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
                    Some(Ok(Ok(TaskEnd::Finished { work, covered }))) => {
                        running.remove(&work.seg);
                        split_at[work.seg].store(u64::MAX, Ordering::Relaxed);
                        if covered < work.len() && cancel.load(Ordering::Relaxed) == 0 {
                            // Stopped for a handoff: the remainder becomes
                            // fresh work and the next iteration spawns it
                            // into the slot this task just freed.
                            pending.push_back(Work {
                                seg: work.seg,
                                start: work.start + covered,
                                end: work.end,
                            });
                        }
                    }
                    Some(Ok(Ok(TaskEnd::GaveUp { work, covered, why }))) => {
                        running.remove(&work.seg);
                        split_at[work.seg].store(u64::MAX, Ordering::Relaxed);
                        let start = (work.start + covered).min(work.end);
                        if cancel.load(Ordering::Relaxed) == 0 && requeues[work.seg] == 0 {
                            // Keep every byte already written and try the
                            // remainder once more on a fresh connection
                            // instead of failing the transfer and making
                            // the user resume it by hand.
                            requeues[work.seg] = 1;
                            eprintln!(
                                "catch: {id} segment {} exhausted its attempts ({why}); re-queued the remainder",
                                work.seg
                            );
                            pending.push_back(Work {
                                seg: work.seg,
                                start,
                                end: work.end,
                            });
                        } else if first_fatal.is_none() {
                            first_fatal = Some(why);
                            cancel.store(2, Ordering::Relaxed);
                        }
                    }
                    Some(Ok(Err(SegError::Fallback(why)))) => {
                        if first_fallback.is_none() { first_fallback = Some(why); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    Some(Ok(Err(SegError::Fatal(why)))) => {
                        if first_fatal.is_none() { first_fatal = Some(why); }
                        cancel.store(2, Ordering::Relaxed);
                    }
                    Some(Ok(Err(SegError::Interrupted(why)))) => {
                        // A segment exhausted its retry budget on body-stream
                        // failures (inactivity, premature EOF, oversized body).
                        // Treat as fatal: stop the other segments and fail.
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
                Directive::Keep | Directive::Unknown => {}
            }
        }

        // cancel handling
        if cancel.load(Ordering::Relaxed) != 0 {
            // Let tasks notice the cancel and flush their BufWriter before
            // recording the frontier. Aborting immediately loses the last
            // buffered bytes while `written` has already counted them.
            drain_segment_tasks(&mut joinset).await;
            checkpoint_progress(part, &manifest, &done_flags, &written).await;
            flush_progress(
                id,
                total,
                &manifest,
                &done_flags,
                &written,
                limiter.as_deref(),
            )
            .await;
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
            flush_progress(
                id,
                total,
                &manifest,
                &done_flags,
                &written,
                limiter.as_deref(),
            )
            .await;
        }

        // periodic durable checkpoint
        if last_checkpoint.elapsed() >= SEGMENT_CHECKPOINT_EVERY {
            last_checkpoint = Instant::now();
            checkpoint_progress(part, &manifest, &done_flags, &written).await;
        }
    }

    // all done: verify
    for i in 0..n {
        if !done_flags[i].load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("segment {} did not complete", i));
        }
    }
    flush_progress(
        id,
        total,
        &manifest,
        &done_flags,
        &written,
        limiter.as_deref(),
    )
    .await;
    let final_manifest = {
        let m = manifest.lock().unwrap();
        m.clone()
    };
    Ok(SegmentedResult::Complete {
        manifest: final_manifest,
    })
}

async fn drain_segment_tasks(joinset: &mut tokio::task::JoinSet<Result<TaskEnd, SegError>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !joinset.is_empty() {
        match tokio::time::timeout_at(deadline, joinset.join_next()).await {
            // Tasks that finish while stopping still flush their buffer in
            // `run_one_segment`; their result is intentionally discarded.
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                joinset.abort_all();
                while joinset.join_next().await.is_some() {}
                return;
            }
        }
    }
}

/// Make the part file durable, then record each segment's resume point in
/// the manifest. The order matters: a resume point must never name bytes
/// that are still only in a userspace buffer or in the page cache.
///
/// The recorded frontier is held back by one write buffer. A task counts
/// bytes in `written` as soon as they enter its BufWriter, and a checkpoint
/// has no way to force that buffer into the file without touching the task
/// itself. Holding back the last buffer is cheap (at most 256 KiB per
/// segment per pause) and keeps the manifest from naming bytes that a crash
/// could still lose.
async fn checkpoint_progress(
    part: &Path,
    manifest: &Arc<Mutex<SegmentManifest>>,
    done_flags: &Arc<Vec<AtomicBool>>,
    written: &Arc<Vec<AtomicU64>>,
) {
    let n = done_flags.len();
    let mut frontiers = Vec::with_capacity(n);
    {
        let m = manifest.lock().unwrap();
        for (i, s) in m.segments.iter().enumerate() {
            let len = s.end - s.start + 1;
            if done_flags[i].load(Ordering::Relaxed) {
                frontiers.push(len);
            } else {
                let frontier = written[i].load(Ordering::Relaxed).min(len);
                frontiers.push(frontier.saturating_sub(SEGMENT_WRITE_BUFFER));
            }
        }
    }

    match std::fs::OpenOptions::new().write(true).open(part) {
        Ok(f) => {
            if let Err(e) = f.sync_data() {
                eprintln!("catch: checkpoint {}: {e}", part.display());
                return;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("catch: checkpoint {}: {e}", part.display());
            return;
        }
    }

    let mut m = manifest.lock().unwrap();
    let mut next = m.clone();
    let mut changed = false;
    for (i, s) in next.segments.iter_mut().enumerate() {
        let len = s.end - s.start + 1;
        if done_flags[i].load(Ordering::Relaxed) {
            if !s.done {
                s.done = true;
                changed = true;
            }
            if s.have != len {
                s.have = len;
                changed = true;
            }
        } else {
            let frontier = frontiers[i].max(s.have).min(len);
            if s.have != frontier {
                s.have = frontier;
                changed = true;
            }
        }
    }
    if changed {
        next.updated_at = now_secs();
        // Publish the new resume points only if the manifest write succeeded;
        // otherwise keep the old, still-valid frontier in memory.
        match save_manifest_atomic(part, &next) {
            Ok(()) => *m = next,
            Err(e) => eprintln!("catch: checkpoint manifest {}: {e:#}", part.display()),
        }
    }
}

async fn flush_progress(
    id: &str,
    total: u64,
    manifest: &Arc<Mutex<SegmentManifest>>,
    done_flags: &Arc<Vec<AtomicBool>>,
    written: &Arc<Vec<AtomicU64>>,
    limiter: Option<&crate::throttle::RateLimiter>,
) {
    let n = done_flags.len();
    let m = manifest.lock().unwrap();
    let mut done_bytes: u64 = 0;
    for i in 0..n {
        if done_flags[i].load(Ordering::Relaxed) {
            done_bytes += m.segments[i].end - m.segments[i].start + 1;
        }
    }
    drop(m);
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
    let mut active = 1usize;
    let _ = crate::state::update_state(|state| {
        active = state
            .downloads
            .iter()
            .filter(|d| d.status == Status::Downloading)
            .count()
            .max(1);
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            // A pause changes the row before the worker gets its final
            // flush. The worker still owns the row and its size is the
            // truthful one, so update a paused row too instead of leaving
            // the last 250 ms of progress (or a whole restarted attempt)
            // on screen.
            if d.pid == std::process::id()
                && matches!(d.status, Status::Downloading | Status::Paused)
            {
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
    // Global cap: this worker's share follows the number of transfers
    // running right now, refreshed at every flush.
    if let (Some(l), Some(share)) = (limiter, crate::throttle::worker_limit(active)) {
        l.set_rate(share);
    }
}

// ---------- one segment ----------

#[allow(clippy::too_many_arguments)]
async fn stream_one_segment_body<S>(
    stream: &mut S,
    buf: &mut tokio::io::BufWriter<&mut tokio::fs::File>,
    cancel: &AtomicU8,
    remaining: u64,
    written_shared: &AtomicU64,
    base_written: u64,
    i: usize,
    part: &Path,
    work_start: u64,
    split_at: &AtomicU64,
    limiter: Option<&crate::throttle::RateLimiter>,
) -> Result<u64, SegError>
where
    S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let mut last_data = Instant::now();
    let mut attempt_bytes: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) != 0 {
            let _ = buf.flush().await;
            return Ok(0);
        }
        if attempt_bytes >= remaining {
            break;
        }
        // A handoff split can arrive mid-body: stop at the instructed
        // frontier and let the runner re-queue the tail on a fresh slot.
        let stop = split_at.load(Ordering::Relaxed);
        if stop != u64::MAX && work_start + attempt_bytes >= stop {
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
            let to_write = space as usize;
            if to_write > 0 && buf.write_all(&chunk[..to_write]).await.is_err() {
                let _ = buf.flush().await;
                return Err(SegError::Fatal(format!("write {}", part.display())));
            }
            let _ = buf.flush().await;
            return Err(SegError::Interrupted(format!(
                "segment {i}: server sent extra bytes"
            )));
        }
        if let Some(l) = limiter {
            // Throttle before the write: the wait is the backpressure.
            l.acquire(chunk.len() as u64).await;
        }
        if buf.write_all(&chunk).await.is_err() {
            let _ = buf.flush().await;
            return Err(SegError::Fatal(format!("write {}", part.display())));
        }
        attempt_bytes += chunk.len() as u64;
        // fetch_max: after a split, head and tail tasks update the same
        // per-segment frontier, and the union coverage is the max — never a
        // backwards step.
        written_shared.fetch_max(base_written + attempt_bytes, Ordering::Relaxed);
        if !chunk.is_empty() {
            last_data = Instant::now();
        }
    }
    if let Err(e) = buf.flush().await {
        return Err(SegError::Fatal(format!("flush {}: {e}", part.display())));
    }
    Ok(attempt_bytes)
}

#[allow(clippy::too_many_arguments)]
async fn run_one_segment(
    work: Work,
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
    split_at: Arc<Vec<AtomicU64>>,
    limiter: Option<Arc<crate::throttle::RateLimiter>>,
) -> Result<TaskEnd, SegError> {
    use reqwest::header;
    use tokio::io::AsyncWriteExt;

    let i = work.seg;
    let seg = {
        let m = manifest.lock().unwrap();
        m.segments[i].clone()
    };
    let seg_len = work.len();
    // Segment-relative offset of this work's first byte; `written` is a
    // segment-relative frontier shared by every work item of this segment.
    let base_off = work.start - seg.start;

    let mut in_seg: u64 = 0; // bytes covered from work.start
    let mut attempts = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) != 0 {
            return Ok(TaskEnd::Finished {
                work,
                covered: in_seg,
            });
        }
        if in_seg >= seg_len {
            break;
        }
        // A split was requested (or this task is already at the frontier):
        // report progress and let the runner hand the tail to a fresh slot.
        let stop = split_at[i].load(Ordering::Relaxed);
        if stop != u64::MAX && work.start + in_seg >= stop {
            return Ok(TaskEnd::Finished {
                work,
                covered: in_seg,
            });
        }
        if attempts >= max_attempts() {
            return Ok(TaskEnd::GaveUp {
                work,
                covered: in_seg,
                why: format!("segment {i}: gave up after {attempts} attempts"),
            });
        }
        attempts += 1;

        let want_start = work.start + in_seg;
        let want_end = work.end;

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
            let mut buf =
                tokio::io::BufWriter::with_capacity(SEGMENT_WRITE_BUFFER as usize, &mut file);
            let mut stream = resp.bytes_stream();
            let remaining = seg_len - in_seg;
            let body_result = stream_one_segment_body(
                &mut stream,
                &mut buf,
                &cancel,
                remaining,
                &written[i],
                base_off + in_seg,
                i,
                &part,
                work.start,
                &split_at[i],
                limiter.as_deref(),
            )
            .await;
            let _ = buf.shutdown().await;
            drop(buf);
            drop(file);

            match body_result {
                Ok(attempt_bytes) => {
                    in_seg += attempt_bytes;
                    written[i].fetch_max(base_off + in_seg, Ordering::Relaxed);
                    // Stopped for the runner's split: report now so the tail
                    // can go to a fresh connection. Only while a tail is
                    // left, though — an attempt that already completed the
                    // work must fall through to the commit below. Reporting
                    // it as a handoff would look like "finished, no
                    // remainder" with the segment still unmarked, and the
                    // transfer would end in "segment N did not complete"
                    // with every byte of it on disk.
                    if in_seg < seg_len {
                        let stop = split_at[i].load(Ordering::Relaxed);
                        if stop != u64::MAX && work.start + in_seg >= stop {
                            return Ok(TaskEnd::Finished {
                                work,
                                covered: in_seg,
                            });
                        }
                    }
                }
                Err(SegError::Interrupted(why)) => {
                    if cancel.load(Ordering::Relaxed) != 0 {
                        return Ok(TaskEnd::Finished {
                            work,
                            covered: in_seg,
                        });
                    }
                    let wait = backoff(attempts);
                    eprintln!(
                        "catch: segment {i}: {why}; retry in {:.1}s",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(other) => return Err(other),
            }
            if in_seg < seg_len {
                // partial result: retry the remaining bytes
                continue;
            }
            // segment complete
            break;
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
    match f {
        Ok(f) => {
            if let Err(e) = f.sync_data() {
                return Err(SegError::Fatal(format!("sync {}: {e}", part.display())));
            }
        }
        Err(e) => {
            return Err(SegError::Fatal(format!("open {}: {e}", part.display())));
        }
    }
    {
        let mut m = manifest.lock().unwrap();
        if i < m.segments.len() {
            m.segments[i].done = true;
            m.segments[i].have = seg.end - seg.start + 1;
        }
        m.updated_at = now_secs();
        if let Err(e) = save_manifest_atomic(&part, &m) {
            return Err(SegError::Fatal(format!("manifest write: {e:#}")));
        }
    }
    done_flags[i].store(true, Ordering::Relaxed);
    written[i].fetch_max(seg.end - seg.start + 1, Ordering::Relaxed);
    Ok(TaskEnd::Finished {
        work,
        covered: in_seg,
    })
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
                done: false,
                have: 0
            }
        );
        assert_eq!(
            r[1],
            SegmentRange {
                start: 4,
                end: 6,
                done: false,
                have: 0
            }
        );
        assert_eq!(
            r[2],
            SegmentRange {
                start: 7,
                end: 9,
                done: false,
                have: 0
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
                have: 0,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
                have: 0,
            },
        ];
        assert!(validate_coverage(&good, 10));
        let gap = vec![
            SegmentRange {
                start: 0,
                end: 3,
                done: false,
                have: 0,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
                have: 0,
            },
        ];
        assert!(!validate_coverage(&gap, 10));
        let overlap = vec![
            SegmentRange {
                start: 0,
                end: 5,
                done: false,
                have: 0,
            },
            SegmentRange {
                start: 5,
                end: 9,
                done: false,
                have: 0,
            },
        ];
        assert!(!validate_coverage(&overlap, 10));
        let mut oversized = good.clone();
        oversized[0].have = 6; // range is 5 bytes
        assert!(!validate_coverage(&oversized, 10));
        let wrong_total = vec![SegmentRange {
            start: 0,
            end: 9,
            done: false,
            have: 0,
        }];
        assert!(!validate_coverage(&wrong_total, 11));
    }

    #[test]
    fn resume_state_keeps_partial_and_completed_prefixes() {
        let mut m = SegmentManifest {
            version: MANIFEST_VERSION,
            url: "https://x.test/f".into(),
            total: 100,
            etag: String::new(),
            last_modified: String::new(),
            segments: segment_ranges(100, 4),
            created_at: 1,
            updated_at: 1,
        };
        m.segments[0].have = 10;
        m.segments[1].done = true;
        m.segments[2].have = 25; // all bytes durable, commit flag lost
        let (done, have) = resume_state(&mut m);
        assert_eq!(have[0], 10);
        assert!(!done[0]);
        assert_eq!(have[1], 25);
        assert!(done[1]);
        assert_eq!(have[2], 25);
        assert!(done[2], "a full durable prefix is a complete segment");
        assert_eq!(have[3], 0);
        assert!(!done[3]);
        assert!(m.segments[2].done);
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
            segments: {
                let mut s = segment_ranges(100, 4);
                s[1].have = 7;
                s
            },
            created_at: 1,
            updated_at: 1,
        };
        save_manifest_atomic(&part, &m).unwrap();
        let loaded = load_manifest(&part, "https://x.test/f").unwrap().unwrap();
        assert_eq!(loaded.total, 100);
        assert_eq!(loaded.segments.len(), 4);
        assert_eq!(loaded.segments[1].have, 7);
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

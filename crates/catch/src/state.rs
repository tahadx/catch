use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Done,
    Error,
    Cancelled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Downloading => "downloading",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DownloadKind {
    #[default]
    File,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub status: Status,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub kind: DownloadKind,
    /// OS pid of the worker holding this entry (0 = none).
    #[serde(default)]
    pub pid: u32,
    /// Unix seconds of the drainer's reservation. Reaping respects a grace
    /// window around a *non-zero* value so a reservation whose worker has not
    /// started yet is not mistaken for an orphan. A worker clears it once it
    /// has stamped its own pid: from then on the transfer lock is the proof
    /// of ownership and a dead worker can be requeued immediately.
    #[serde(default)]
    pub claimed_at: u64,
    /// Parked by the scheduler (not by the user): the scheduler may resume it.
    #[serde(default)]
    pub sched_hold: bool,
    /// yt-dlp bestaudio extraction for video entries.
    #[serde(default)]
    pub audio_only: bool,
    /// Display title (video title when known); filename stays on disk name.
    #[serde(default)]
    pub title: String,
    /// HTTP validators seen for this representation. Sent back as `If-Range`
    /// on a resume: a server whose file changed answers 200 (or a different
    /// ETag) instead of a 206, which restarts the transfer from zero instead
    /// of splicing two different files together.
    #[serde(default)]
    pub etag: String,
    #[serde(default)]
    pub last_modified: String,
    /// User-chosen queue order: higher runs first, ties keep queue order.
    /// Never a reason to drop anything; drain only sorts by it.
    #[serde(default)]
    pub priority: i64,
    /// Connection attempts the last worker used. Persisted so a restart does
    /// not silently reset the retry budget: `resume` starts a *new* budget
    /// on purpose, a crashed worker does not.
    #[serde(default)]
    pub attempts: u32,
    /// Unix seconds when the next automatic retry is due (0 = none). Written
    /// with each `retrying in Ns` note so `catch why` can say when.
    #[serde(default)]
    pub next_retry_at: u64,
    /// Live rate in bytes/s as measured by the worker on a monotonic clock
    /// (0 when unknown or idle). The panel may prefer this over its own
    /// sampling: it survives panel restarts because it is persisted.
    #[serde(default)]
    pub speed: u64,
    /// Average rate over the whole transfer, bytes/s.
    #[serde(default)]
    pub avg_speed: u64,
    /// Seconds remaining at the current rate (0 = unknown).
    #[serde(default)]
    pub eta: u64,
    /// Optional lowercase hex SHA-256 the user pinned for this download.
    /// Verified over the part file before it is published; a mismatch is
    /// reported as an integrity error and never silently published.
    #[serde(default)]
    pub expected_sha256: String,
    /// Fields written by other versions (older or newer) pass through
    /// untouched: a stale backend rewriting state must never strip a fresh
    /// writer's data — this exact clobber erased `title` on every drain.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A file Catch saved for a link. Kept after the queue row is cleared or
/// removed, because the file is still on disk: without this, catching a
/// link again after `catch clear` looked brand new and silently saved a
/// second copy instead of asking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedFile {
    /// Path relative to the download dir: "clip.webm", "video/clip.webm".
    pub path: String,
    #[serde(default)]
    pub kind: DownloadKind,
    #[serde(default)]
    pub bytes: u64,
    /// Unix seconds, so the panel can say when it was saved.
    #[serde(default)]
    pub at: u64,
}

/// A link the user just sent that is already a file in the download folder.
/// The panel asks what to do. Nothing is fetched until they choose.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ask {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub kind: DownloadKind,
    /// Path relative to the download dir ("programs/app.apk").
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    /// True when this is not a saved file but a transfer still running. The
    /// user sent the same link again while it was in flight, so the panel
    /// offers "download it again" rather than "open the file you have".
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub downloads: Vec<Download>,
    /// Link -> what was saved for it. Survives clearing the queue on
    /// purpose; that is the whole point of it.
    #[serde(default)]
    pub saved: std::collections::BTreeMap<String, SavedFile>,
    /// Links waiting on the "already in your folder" question. Newest first.
    #[serde(default)]
    pub asks: Vec<Ask>,
    /// Same pass-through contract as `Download::extra`.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            downloads: Vec::new(),
            saved: std::collections::BTreeMap::new(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        dir.join("omarchy").join("catch")
    } else {
        home_dir()
            .join(".local")
            .join("state")
            .join("omarchy")
            .join("catch")
    }
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

pub fn default_download_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR").map(PathBuf::from) {
        dir.join("catch")
    } else {
        home_dir().join("Downloads").join("catch")
    }
}

/// File-extension → category folder (always lowercase). Unknown
/// extensions return None and stay flat in the download dir.
pub fn category_for(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit('.').next()?.to_lowercase();
    if !filename.contains('.') || ext.is_empty() {
        return None;
    }
    Some(match ext.as_str() {
        "mp4" | "mkv" | "avi" | "webm" | "mov" | "flv" | "m4v" => "video",
        "mp3" | "flac" | "wav" | "m4a" | "ogg" | "aac" => "music",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "odt" => "documents",
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => "compressed",
        "appimage" | "deb" | "rpm" | "sh" | "bin" | "exe" => "programs",
        _ => return None,
    })
}

/// Type folders are off: everything lands flat in the download dir, so a file
/// is where the panel says it is and nothing is filed away into documents/ or
/// video/ behind the user's back. `CATCH_CATEGORIES=on` brings the folders
/// back for anyone who wants them sorted; `dir_for` and `video_dir` keep the
/// same shape either way, so nothing else has to know which mode is on.
pub fn categories_on() -> bool {
    categories_from(std::env::var("CATCH_CATEGORIES").ok().as_deref())
}

/// The whole decision, without touching the process environment.
fn categories_from(env: Option<&str>) -> bool {
    match env {
        Some(raw) => raw.trim().eq_ignore_ascii_case("on"),
        None => false,
    }
}

/// Download dir for a given filename: base dir plus the category folder
/// when the extension matches one. Created on use.
pub fn dir_for(base: &std::path::Path, filename: &str) -> PathBuf {
    if categories_on() {
        if let Some(cat) = category_for(filename) {
            return base.join(cat);
        }
    }
    base.to_path_buf()
}

/// Video download dir: the video folder when categories are on.
pub fn video_dir(base: &std::path::Path) -> PathBuf {
    if categories_on() {
        base.join("video")
    } else {
        base.to_path_buf()
    }
}

/// True for watch-style video pages (YouTube long + Shorts, Vimeo):
/// fetching these as plain files would only save HTML.
pub fn looks_like_video_page(url: &str) -> bool {
    let lower = url.to_lowercase();
    let Some((host, path)) = lower.split_once("://").map(|(_, rest)| {
        let mut parts = rest.splitn(2, '/');
        (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
    }) else {
        return false;
    };
    let host = host.trim_start_matches("www.").trim_start_matches("m.");
    match host {
        "youtube.com" | "music.youtube.com" => {
            path.starts_with("watch") || path.starts_with("shorts/") || path.starts_with("live/")
        }
        "youtu.be" => !path.is_empty(),
        "vimeo.com" | "player.vimeo.com" => !path.is_empty(),
        _ => false,
    }
}

/// `~` and `~/…` are the home directory. `~someone` is left as typed.
fn expand_home_in(path: &str, home: &std::path::Path) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

fn expand_home(path: &str) -> PathBuf {
    expand_home_in(path, &home_dir())
}

pub fn resolve_download_dir(explicit: Option<&str>) -> PathBuf {
    match explicit {
        Some(s) if !s.trim().is_empty() => expand_home(s),
        _ => shell_download_dir().unwrap_or_else(default_download_dir),
    }
}

/// The panel's download folder setting from shell.json (same value `o`
/// opens). Empty/absent means the default.
fn shell_download_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
    let text = std::fs::read_to_string(&shell).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        let entries = match layout.get(section).and_then(|s| s.as_array()) {
            Some(e) => e,
            None => continue,
        };
        for e in entries {
            if e.get("id")?.as_str()? != "io.github.tahadx.catch" {
                continue;
            }
            let dir = e.get("downloadDir")?.as_str()?.trim();
            if !dir.is_empty() {
                return Some(expand_home(dir));
            }
        }
    }
    None
}

pub fn load_state() -> Result<State> {
    let path = state_path();
    if !path.exists() {
        return Ok(State::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(State::default());
    }
    match serde_json::from_str::<State>(&text) {
        Ok(mut state) => {
            state.version = STATE_VERSION;
            Ok(state)
        }
        Err(e) => {
            // A truncated or edited state file used to brick every command
            // that touches the queue. Move the unreadable file aside (it is
            // the user's only record of the queue, so keep it) and start
            // clean: Catch heals itself and says so.
            let quarantine = state_dir().join(format!("state.json.bad-{}", now_secs()));
            let moved = std::fs::rename(&path, &quarantine).is_ok();
            eprintln!(
                "catch: state file unreadable ({e}); {}",
                if moved {
                    format!("kept it at {}", quarantine.display())
                } else {
                    "could not move it aside; starting from an empty queue".to_string()
                }
            );
            Ok(State::default())
        }
    }
}

/// Atomic write beside the target, then rename. Never modifies user files.
///
/// The temp file is fsynced before the rename and the directory after it, so
/// a power loss cannot leave a state file that is newer by name but empty (or
/// half-written) by content — queue state is the user's only record of what
/// is in flight. The rename itself keeps readers crash-proof: they see the
/// old file or the new one, never a mixture.
pub fn save_state(state: &State) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = state_path();
    let tmp = dir.join("state.json.tmp");
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, &text).with_context(|| format!("write {}", tmp.display()))?;
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp) {
        let _ = f.sync_data();
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn new_id() -> String {
    // Millisecond + pid keeps ids readable and roughly sortable; the counter
    // makes them unique inside one process. A batch enqueue (a page with
    // hundreds of links, a clipboard paste) adds many rows in the same
    // millisecond, and two rows sharing an id share a part file: one worker's
    // bytes landed in the other's transfer.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = format!("{}-{}", millis, std::process::id() % 100000);
    if seq == 0 {
        // First id in this process keeps the historical shape.
        base
    } else {
        format!("{base}-{seq}")
    }
}

/// Cap a string at `max` bytes **without splitting a UTF-8 character**.
/// `String::truncate` panics on a non-boundary offset, which is how a long
/// CJK or emoji filename took the whole backend down mid-enqueue.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A filename that is safe to join onto the download directory, from an
/// untrusted source (URL tail, Content-Disposition, browser hint).
///
/// Only the final path segment survives, control characters and NUL are
/// dropped, leading dots are stripped (no `..`, no hidden files), and the
/// result is capped at 128 bytes on a character boundary with the extension
/// kept when there is room for it. Empty means "use the caller's fallback".
pub fn sanitize_filename(raw: &str) -> String {
    let flat = raw.replace('\\', "/");
    let tail = flat.rsplit('/').next().unwrap_or("");
    let cleaned: String = tail
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return String::new();
    }
    // Keep the extension: dropping it would file the download in the wrong
    // category folder and change how it opens.
    if cleaned.len() > 128 {
        let (stem, ext) = match cleaned.rfind('.') {
            Some(i) if i > 0 && cleaned.len() - i <= 12 => (&cleaned[..i], &cleaned[i..]),
            _ => (cleaned, ""),
        };
        let stem = truncate_bytes(stem, 128usize.saturating_sub(ext.len()));
        let stem = stem.trim_end_matches(['.', ' ']);
        let stem = if stem.is_empty() { "download" } else { stem };
        return format!("{stem}{ext}");
    }
    cleaned.to_string()
}

/// Derive a safe filename from a URL. Falls back to `download-<id>`.
pub fn filename_for_url(url: &str, id: &str) -> String {
    let tail = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim();
    // Percent-decode minimally: '+' and %XX for the common cases.
    let decoded = percent_decode(tail);
    let name = sanitize_filename(&decoded);
    if name.is_empty() {
        format!("download-{}", id)
    } else {
        name
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Path of `dest` relative to the download dir, for the saved index, so the
/// record survives the user moving their download folder.
pub fn relative_to_base(base: &Path, dest: &Path) -> String {
    dest.strip_prefix(base)
        .unwrap_or(dest)
        .to_string_lossy()
        .into_owned()
}

/// Remember that `url` landed at `dest`. Called where a transfer finishes,
/// inside the same locked write that marks the row done.
pub fn record_saved(state: &mut State, url: &str, base: &Path, dest: &Path, kind: DownloadKind) {
    let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    state.saved.insert(
        url.to_string(),
        SavedFile {
            path: relative_to_base(base, dest),
            kind,
            bytes,
            at,
        },
    );
}

/// What Catch saved for this link. The index answers it, and it outlives the
/// queue rows. Links caught before the index existed fall back to their
/// finished row, which carries no recorded path — callers then work the file
/// out from the row's name.
pub fn saved_match(state: &State, url: &str) -> Option<SavedFile> {
    if let Some(rec) = state.saved.get(url) {
        return Some(rec.clone());
    }
    state
        .downloads
        .iter()
        .find(|d| d.url == url && d.status == Status::Done)
        .map(|d| SavedFile {
            path: String::new(),
            kind: d.kind,
            bytes: d.bytes,
            at: 0,
        })
}

/// The index entry for this link, ignoring the pre-index fallback. The one
/// caller that cares is the staleness prune, which must only ever drop real
/// entries.
pub fn saved_indexed<'a>(state: &'a State, url: &str) -> Option<&'a SavedFile> {
    state.saved.get(url)
}

/// The index says this link was saved, and that file is no longer in the
/// download folder. Callers forget the record and treat the link as new.
pub fn indexed_file_missing(state: &State, base: &Path, url: &str) -> bool {
    let Some(rec) = state.saved.get(url) else {
        return false;
    };
    resolve_inside(base, &base.join(&rec.path).to_string_lossy()).is_none()
}

/// The file Catch already has for this link, when it is still in the
/// download folder. An index entry whose file was deleted is not a match
/// (the caller drops it). A finished row from before the index existed
/// matches when its file is still where the downloader would have put it.
pub fn locate_saved(state: &State, base: &Path, url: &str) -> Option<Ask> {
    if let Some(rec) = state.saved.get(url) {
        let abs = base.join(&rec.path);
        resolve_inside(base, &abs.to_string_lossy())?;
        let filename = rec.path.rsplit('/').next().unwrap_or(&rec.path).to_string();
        let title = state
            .downloads
            .iter()
            .find(|d| d.url == url)
            .map(|d| d.title.clone())
            .unwrap_or_default();
        let bytes = std::fs::metadata(&abs)
            .map(|m| m.len())
            .unwrap_or(rec.bytes);
        return Some(Ask {
            url: url.to_string(),
            title,
            filename,
            kind: rec.kind,
            path: rec.path.clone(),
            bytes,
            live: false,
        });
    }
    let row = state
        .downloads
        .iter()
        .find(|d| d.url == url && d.status == Status::Done)?;
    let mut candidates = vec![
        dir_for(base, &row.filename).join(&row.filename),
        base.join(&row.filename),
    ];
    if row.kind == DownloadKind::Video {
        candidates.push(video_dir(base).join(&row.filename));
    }
    for candidate in candidates {
        if resolve_inside(base, &candidate.to_string_lossy()).is_none() {
            continue;
        }
        let bytes = std::fs::metadata(&candidate)
            .map(|m| m.len())
            .unwrap_or(row.bytes);
        return Some(Ask {
            url: url.to_string(),
            title: row.title.clone(),
            filename: row.filename.clone(),
            kind: row.kind,
            path: relative_to_base(base, &candidate),
            bytes,
            live: false,
        });
    }
    None
}

/// Remember a question, newest first. The same link replaces its older ask.
pub fn push_ask(state: &mut State, ask: Ask) {
    state.asks.retain(|a| a.url != ask.url);
    state.asks.insert(0, ask);
}

pub fn clear_ask(state: &mut State, url: &str) {
    state.asks.retain(|a| a.url != url);
}

/// The row already working on this link, if any. A second catch of a link
/// that is queued or transferring reuses that row (retry floods and double
/// clicks used to stack duplicates); a link Catch already *saved* gets a
/// fresh row instead, so it lands as another file with a new name.
pub fn live_row_for<'a>(state: &'a State, url: &str) -> Option<&'a Download> {
    state
        .downloads
        .iter()
        .find(|d| d.url == url && matches!(d.status, Status::Queued | Status::Downloading))
}

/// Server-advertised filename from a Content-Disposition header value.
/// Handles `filename="app.apk"` and RFC 5987 `filename*=UTF-8''app.apk`.
/// Returns None for missing, empty, or unsafe names.
pub fn filename_from_disposition(header: &str) -> Option<String> {
    let mut name: Option<String> = None;
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // RFC 5987: charset'lang'percent-encoded
            let encoded = rest.split('\'').next_back().unwrap_or("").trim();
            let decoded = percent_decode(encoded.trim_matches('"'));
            if !decoded.trim().is_empty() {
                name = Some(decoded);
                break;
            }
        } else if name.is_none() {
            if let Some(rest) = part.strip_prefix("filename=") {
                let v = rest.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    name = Some(v.to_string());
                }
            }
        }
    }
    let raw = name?;
    // Never trust a path: keep the final segment only.
    let tail = raw
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let cleaned = clean_label(&tail, "");
    if cleaned.is_empty() {
        return None;
    }
    // Reject extensionless blobs: they add nothing over the URL name.
    if !cleaned.contains('.') {
        return None;
    }
    Some(cleaned)
}

/// A printed path is only usable if it is a regular file inside `dir`.
/// Guards against control-char tricks and directory escapes.
pub fn resolve_inside(dir: &Path, candidate: &str) -> Option<PathBuf> {
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains(['\n', '\r', '\t']) {
        return None;
    }
    let candidate = PathBuf::from(candidate);
    if !candidate.is_file() {
        return None;
    }
    let file_real = candidate.canonicalize().ok()?;
    let dir_real = dir.canonicalize().ok()?;
    let s = file_real.to_string_lossy();
    if s.contains(['\n', '\r', '\t']) {
        return None;
    }
    if file_real.starts_with(&dir_real) {
        Some(file_real)
    } else {
        None
    }
}

/// Keep only what a person would read; drop control chars and leading dashes.
/// Capped at 128 bytes on a character boundary (byte truncation used to panic
/// on the first CJK title over the limit).
pub fn clean_label(s: &str, fallback: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    out = out.trim().trim_matches('.').to_string();
    if out.len() > 128 {
        out = truncate_bytes(&out, 128).to_string();
    }
    if out.is_empty() || out.starts_with('-') {
        return fallback.to_string();
    }
    out
}

/// The (row title, on-disk name) a rename would produce: path separators
/// and control characters are dropped, the original extension is kept.
/// None when nothing usable remains as a title.
pub fn rename_plan(original: &str, raw: &str) -> Option<(String, String)> {
    let stem: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let stem = stem.trim().trim_matches('.').to_string();
    if stem.is_empty() {
        return None;
    }
    let stem: String = stem.chars().take(120).collect();
    let ext = match original.rfind('.') {
        Some(i) if i > 0 => &original[i..],
        _ => "",
    };
    // People type the whole file name ("ddr35.apk"), not just a title. Adding
    // the extension to that used to land "ddr35.apk.apk" on disk, so a title
    // that already ends in the file's own extension is the complete name.
    // Any other trailing dot stays part of the title: "dots..trim" is a
    // title, and guessing at extensions cannot tell it from "clip.mkv".
    if !ext.is_empty()
        && stem
            .to_ascii_lowercase()
            .ends_with(&ext.to_ascii_lowercase())
    {
        return Some((stem.clone(), stem));
    }
    Some((stem.clone(), format!("{stem}{ext}")))
}

pub fn mark_error(id: &str, msg: &str) -> Result<()> {
    update_state(|state| {
        if let Some(d) = state.downloads.iter_mut().find(|d| d.id == id) {
            d.status = Status::Error;
            d.error = msg.to_string();
            // The worker is gone once it reports an error; leaving its pid
            // behind made a dead transfer look live to the panel.
            d.pid = 0;
            d.claimed_at = 0;
        }
        Ok(())
    })
}

/// Run `f` under the state lock so concurrent processes (browser
/// handoffs, workers, panel actions) never interleave load-modify-save
/// cycles and lose entries. Stale locks (holder dead >2 min) are reclaimed.
///
/// The lock is released by a guard, not by the happy path: a panic inside
/// `f` (a bad filename used to panic here) unwound straight past the
/// remove, and every later command failed with "state is locked" for the
/// full two minutes of staleness.
pub fn with_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let lock = dir.join("state.lock");
    let _held = take_state_lock(&lock)?;
    f()
}

/// Removes the lock directory when it goes out of scope, including on panic.
struct LockGuard(std::path::PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Unix pid from /proc. An empty cmdline is a zombie or kernel thread, an
/// unreadable/missing file means the process is gone.
pub(crate) fn pid_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    proc_argv(pid).is_some_and(|argv| !argv.is_empty())
}

/// Write the holder into a lock directory so a later process can tell a
/// crashed holder from one that is merely slow.
fn stamp_lock_owner(lock: &std::path::Path) {
    let _ = std::fs::write(lock.join("owner"), format!("{}\n", std::process::id()));
}

/// A state lock is abandoned when its owner process is gone. The mtime is
/// only a second opinion: a lock whose owner is alive is never stolen, and
/// a lock with no owner file gets a short grace before it is treated as
/// abandoned (it is created immediately before the pid is written).
fn lock_abandoned(lock: &std::path::Path) -> bool {
    let age = std::fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::ZERO);
    if age < Duration::from_secs(2) {
        return false;
    }
    let owner = std::fs::read_to_string(lock.join("owner"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    match owner {
        Some(pid) => !pid_live(pid),
        None => age > Duration::from_secs(30),
    }
}

/// Reclaim a lock directory that was abandoned (a crash, a SIGKILL) instead
/// of failing forever. `rename` makes the reclaim exclusive: two processes
/// that both decide "stale" cannot both succeed, so one of them creates the
/// fresh lock while the other loses the race and retries the normal way.
fn reclaim_stale_lock(lock: &std::path::Path) -> bool {
    let graveyard = lock.with_extension(format!("stale-{}", std::process::id()));
    if std::fs::rename(lock, &graveyard).is_err() {
        return false;
    }
    let _ = std::fs::remove_dir_all(&graveyard);
    std::fs::create_dir(lock).is_ok()
}

fn take_state_lock(lock: &std::path::Path) -> Result<LockGuard> {
    if std::fs::create_dir(lock).is_ok() {
        stamp_lock_owner(lock);
        return Ok(LockGuard(lock.to_path_buf()));
    }
    if lock_abandoned(lock) && reclaim_stale_lock(lock) {
        stamp_lock_owner(lock);
        return Ok(LockGuard(lock.to_path_buf()));
    }
    // Brief contention: wait for the holder instead of failing.
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if std::fs::create_dir(lock).is_ok() {
            stamp_lock_owner(lock);
            return Ok(LockGuard(lock.to_path_buf()));
        }
        if lock_abandoned(lock) && reclaim_stale_lock(lock) {
            stamp_lock_owner(lock);
            return Ok(LockGuard(lock.to_path_buf()));
        }
    }
    anyhow::bail!("state is locked by another catch process")
}

/// Load-modify-save under the state lock.
pub fn update_state(f: impl FnOnce(&mut State) -> Result<()>) -> Result<()> {
    with_lock(|| {
        let mut state = load_state()?;
        f(&mut state)?;
        save_state(&state)
    })
}
/// Ensure a checkout-installed backend is configured: when this binary
/// lives at <plugin>/bin/dist/catch (the shipped layout), run the
/// plugin's bin/configure so backend links and browser hosts exist.
/// Best-effort and idempotent; never fails the caller.
///
/// The four things configure does are checked here first: spawning `bash`
/// on every invocation (including every `catch drain` the panel fires) cost
/// two processes per wake-up just to learn that nothing had changed.
pub fn self_configure() {
    // bin/configure invokes this binary (version check, install-hosts) while
    // the links/hosts it is about to create are still missing. Without this
    // guard each of those children would spawn configure again, recursively,
    // until the process limit happened to break the chain.
    if std::env::var_os("CATCH_NO_SELF_CONFIGURE").is_some() {
        return;
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_default();
    let Some(dist) = exe.parent() else { return };
    if dist.file_name().and_then(|s| s.to_str()) != Some("dist") {
        return;
    }
    let Some(bin) = dist.parent() else { return };
    let Some(plugin) = bin.parent() else { return };
    let script = plugin.join("bin").join("configure");
    if !script.is_file() {
        return;
    }
    if !needs_configure() {
        return;
    }
    let _ = std::process::Command::new("bash")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The same "is anything missing?" test bin/configure opens with.
fn needs_configure() -> bool {
    let home = home_dir();
    let bin_dir = home.join(".local/bin");
    let executable = |p: std::path::PathBuf| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if !executable(bin_dir.join("catch")) || !executable(bin_dir.join("catch-host")) {
        return true;
    }
    let host = format!("{}.json", "io.github.tahadx.catch");
    !home
        .join(".config/chromium/NativeMessagingHosts")
        .join(&host)
        .is_file()
        || !home
            .join(".mozilla/native-messaging-hosts")
            .join(&host)
            .is_file()
}

/// The drain marker names its holder, so a drainer killed between create and
/// release can be replaced at once instead of blocking the queue for the old
/// two-minute mtime window. A marker created but not yet stamped gets the same
/// short grace as the state lock, because another process may be between
/// `create_new` and the pid write.
fn drain_marker_abandoned(marker: &std::path::Path) -> bool {
    let age = std::fs::metadata(marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::ZERO);
    let owner = std::fs::read_to_string(marker)
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<u32>().ok()));
    match owner {
        // A stamped, dead pid is conclusive at any age: the holder either
        // finished (and its Drop failed) or was killed. The grace only
        // protects the instant between create_new and the pid write.
        Some(pid) => !pid_live(pid),
        None => age > Duration::from_secs(30),
    }
}

/// Take an abandoned marker out of the way. `rename` makes the reclaim
/// exclusive, so two drainers that both see the same dead pid cannot both
/// create a marker: the loser retries and sees the winner's live marker.
fn reclaim_drain_marker(marker: &std::path::Path) -> bool {
    let graveyard = marker.with_extension(format!("stale-{}", std::process::id()));
    if std::fs::rename(marker, &graveyard).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&graveyard);
    true
}

/// True when this process may act as the single drainer, taking the marker.
/// False when one is already running. A marker whose holder died is reclaimed
/// immediately; the mtime is only a second opinion for an unstamped marker.
pub fn claim_drain_slot() -> bool {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let marker = dir.join("drain.lock");
    let take = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .is_ok()
    };
    if take() {
        let _ = std::fs::write(&marker, format!("{}\n{}\n", std::process::id(), now_secs()));
        return true;
    }
    if !drain_marker_abandoned(&marker) || !reclaim_drain_marker(&marker) {
        return false;
    }
    if take() {
        let _ = std::fs::write(&marker, format!("{}\n{}\n", std::process::id(), now_secs()));
        return true;
    }
    false
}

/// True when some live drainer already holds the marker. An abandoned one is
/// cleared so a later spawn can take it.
pub fn drain_marker_held() -> bool {
    let marker = state_dir().join("drain.lock");
    if !marker.exists() {
        return false;
    }
    if drain_marker_abandoned(&marker) {
        let _ = reclaim_drain_marker(&marker);
        return false;
    }
    true
}

/// Release the drain marker when a drainer finishes. Only our own marker is
/// removed: a drainer that somehow lost its slot must not delete its
/// successor's marker on the way out.
pub fn release_drain_slot() {
    let marker = state_dir().join("drain.lock");
    let owner = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<u32>().ok()));
    if owner == Some(std::process::id()) {
        let _ = std::fs::remove_file(marker);
    }
}

/// Fire-and-forget: wake the queue drainer so queued entries start when a
/// slot frees up. Never fails the caller.
///
/// Only one drainer may run at a time. Without this the chain
/// drain -> worker -> spawn_drain -> drain had no bound at all: transfers
/// that finish or fail quickly re-forked it faster than the work died, and
/// the machine ended up with thousands of `catch` processes. The marker is
/// created exclusively and reclaimed if its holder died or went stale.
pub fn spawn_drain() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    // A live drainer already holds the marker, so this wake-up is redundant:
    // that drainer will pick up whatever woke us.
    if drain_marker_held() {
        return;
    }
    let _ = std::process::Command::new(exe)
        .arg("drain")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The argv of a live process. `None` when the process is gone, is a zombie,
/// or is not readable — none of which is a worker holding a row.
///
/// `/proc/PID/cmdline` is NUL-separated, so matching it against
/// space-separated strings can never succeed on the first argument and can
/// match by accident on any later one ("catch download" split across argv[0]
/// and argv[1]). Parse the arguments instead of searching the raw bytes.
fn proc_argv(pid: u32) -> Option<Vec<String>> {
    if pid == 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        // Kernel thread, or a zombie whose parent has not reaped it yet.
        return None;
    }
    let argv = parse_cmdline(&raw);
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Split `/proc/PID/cmdline` bytes into arguments. NUL separates arguments;
/// trailing NULs produce no empty argument.
fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// True when `argv` is a catch transfer worker (not a drain, not the panel's
/// short-lived `catch pause`/`catch remove`, and not an unrelated process
/// that happens to have "catch" somewhere in its command line).
fn is_transfer_worker(argv: &[String]) -> bool {
    let prog = argv.first().map(String::as_str).unwrap_or("");
    let base = prog.rsplit('/').next().unwrap_or(prog);
    if base != "catch" && base != "catch-host" {
        return false;
    }
    matches!(argv.get(1).map(String::as_str), Some("download" | "video"))
        && argv.get(2).is_some_and(|id| !id.is_empty())
}

/// True when `pid` is a live transfer worker *for this entry*: a `catch
/// download <id>` / `catch video <id>` (or the browser host running it).
/// PID reuse by any other process — including another catch worker — counts
/// as gone, so a row can never be held by an unrelated process for good.
pub fn worker_holds(pid: u32, id: &str) -> bool {
    let Some(argv) = proc_argv(pid) else {
        return false;
    };
    is_transfer_worker(&argv) && argv.get(2).map(String::as_str) == Some(id)
}

/// True when `pid` is a live catch process (any subcommand): the binary's own
/// basename is checked, so a recycled pid belonging to anything else — even
/// something with "catch" in its arguments — never counts as a holder.
fn holder_process_alive(pid: u32) -> bool {
    let Some(argv) = proc_argv(pid) else {
        return false;
    };
    let prog = argv.first().map(String::as_str).unwrap_or("");
    let base = prog.rsplit('/').next().unwrap_or(prog);
    base == "catch" || base == "catch-host"
}

/// The pid of the worker holding `id`'s transfer lock, if any and still alive.
fn live_claim_holder(id: &str) -> Option<u32> {
    let path = state_dir().join("workers").join(format!("{id}.lock"));
    let pid: u32 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())?;
    if pid == std::process::id() || !holder_process_alive(pid) {
        return None;
    }
    Some(pid)
}

/// True when some live worker process is transferring this entry, whichever
/// command started it (`download`, `video`, or a blocking `add`/`copy`/
/// `replace`). This is the authority for "do not start a second transfer":
/// the queue row's pid is only a hint written before the worker existed.
pub fn entry_claimed(id: &str) -> bool {
    live_claim_holder(id).is_some()
}

/// True while the pid names any live catch transfer worker. Kept for
/// diagnostics from tests once an id is not known.
#[allow(dead_code)]
fn worker_alive(pid: u32) -> bool {
    proc_argv(pid)
        .map(|argv| is_transfer_worker(&argv))
        .unwrap_or(false)
}

/// Exclusive ownership of one entry's `.part` file.
///
/// The row's `pid` column is only a hint: it is written by the drainer
/// before the worker exists, and a worker that is slow to start leaves a
/// window in which a second `catch download <id>` would happily open the
/// same part file and interleave writes. This is a real cross-process lock
/// beside the part file, freed on drop.
pub struct WorkerClaim {
    path: std::path::PathBuf,
    id: String,
}

/// Transfers held by *this* process. A lock file naming our pid is only a
/// duplicate when this set still holds the id; otherwise the pid was reused
/// by a new catch process after its predecessor died.
fn held_claims() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static HELD: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    HELD.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

impl Drop for WorkerClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Ok(mut held) = held_claims().lock() {
            held.remove(&self.id);
        }
    }
}

/// Take the transfer lock for `id`. `Ok(None)` means a live worker for this
/// id already holds it; the caller must not start a second transfer.
///
/// The lock records the holder's pid. A lock whose holder is gone (crash,
/// SIGKILL, power loss) or is not a worker for this entry (PID reuse) is
/// reclaimed. Reclaim goes through a rename so two rivals cannot both win.
pub fn claim_worker(id: &str) -> Result<Option<WorkerClaim>> {
    let dir = state_dir().join("workers");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{id}.lock"));
    let own = std::process::id();
    static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for _ in 0..3 {
        // Write the pid to a private temp file and hard-link it into place:
        // the lock appears atomically with its holder already recorded, so a
        // rival can never observe an empty lock and steal it mid-write.
        let tmp = dir.join(format!(
            ".{id}.lock.tmp-{own}-{}",
            TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&tmp, format!("{own}\n"))
            .with_context(|| format!("write {}", tmp.display()))?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                if let Ok(mut held) = held_claims().lock() {
                    held.insert(id.to_string());
                }
                return Ok(Some(WorkerClaim {
                    path,
                    id: id.to_string(),
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder: u32 = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                if holder == own {
                    let held = held_claims()
                        .lock()
                        .map(|h| h.contains(id))
                        .unwrap_or(false);
                    if held {
                        return Ok(None);
                    }
                    // Our pid, but no transfer in this process holds it: a
                    // new process inherited a recycled pid. Reclaim.
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if holder_process_alive(holder) {
                    // Any live catch process on that lock owns the transfer:
                    // `catch add`, `catch copy` and `catch replace` download
                    // inline, and stealing from them corrupts the part file.
                    return Ok(None);
                }
                // Dead holder: reclaim through a rename so two rivals cannot
                // both believe they won.
                let graveyard = path.with_extension(format!("stale-{own}"));
                if std::fs::rename(&path, &graveyard).is_err() {
                    // Someone else reclaimed it first; try again from the top.
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let _ = std::fs::remove_file(&graveyard);
            }
            Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
        }
    }
    anyhow::bail!("could not take the transfer lock for {id}")
}

/// How long a freshly claimed entry is left alone before a missing worker
/// counts as dead.
///
/// A drainer reserves a slot by stamping the row Downloading, spawns the
/// worker, and then exits. Between that exit and the worker stamping its own
/// pid there is a window where the row says Downloading and points at a pid
/// that is gone. Reaping inside that window handed the row straight back,
/// another drainer claimed it, and the chain drain -> worker -> drain could
/// re-fork without end. The claim time is what makes that window visible.
const CLAIM_GRACE: Duration = Duration::from_secs(60);

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Requeue entries whose worker died holding them (stuck at 100% or any
/// percent with no process behind them). Returns how many were freed.
///
/// A row claimed within `CLAIM_GRACE` is left alone even when its pid is
/// gone: that is a drain reservation whose worker has not started yet, and
/// treating it as an orphan is what produced thousands of forked processes.
/// A row whose transfer lock is held by a live process is likewise left
/// alone, whichever command started that transfer.
pub fn reap_orphans(state: &mut State) -> usize {
    let now = now_secs();
    let mut freed = 0;
    for d in state.downloads.iter_mut() {
        if d.status != Status::Downloading || worker_holds(d.pid, &d.id) {
            continue;
        }
        if d.claimed_at != 0 && now.saturating_sub(d.claimed_at) < CLAIM_GRACE.as_secs() {
            continue;
        }
        if entry_claimed(&d.id) {
            continue;
        }
        d.status = Status::Queued;
        d.pid = 0;
        d.claimed_at = 0;
        freed += 1;
    }
    freed
}
/// Max parallel downloads. Single source of truth: the widget settings in
/// shell.json (editable in the Omarchy Settings UI). Falls back to 3;
/// CATCH_MAX_PARALLEL wins over everything for headless use.
pub fn max_parallel() -> usize {
    if let Ok(raw) = std::env::var("CATCH_MAX_PARALLEL") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if (1..=10).contains(&n) {
                return n;
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
        if let Ok(text) = std::fs::read_to_string(&shell) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(n) = find_widget_max(&v) {
                    return n;
                }
            }
        }
    }
    3
}

fn find_widget_max(v: &serde_json::Value) -> Option<usize> {
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        let entries = match layout.get(section).and_then(|s| s.as_array()) {
            Some(e) => e,
            None => continue,
        };
        for e in entries {
            if e.get("id")?.as_str()? != "io.github.tahadx.catch" {
                continue;
            }
            let n = e.get("maxParallel")?.as_u64()? as usize;
            if [1, 3, 5, 10].contains(&n) {
                return Some(n);
            }
        }
    }
    None
}

/// Segmented-download setting from the widget (1 disables segmentation).
/// Env CATCH_SEGMENTS overrides. Valid values: 1, 2, 4, 8.
pub fn segments_setting() -> usize {
    if let Ok(raw) = std::env::var("CATCH_SEGMENTS") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if [1, 2, 4, 8].contains(&n) {
                return n;
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
        if let Ok(text) = std::fs::read_to_string(&shell) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(n) = find_widget_segments(&v) {
                    return n;
                }
            }
        }
    }
    4
}

/// Traffic mode, from the widget settings in shell.json (or CATCH_BW_MODE).
/// Modes shape how much of the machine's capacity Catch is willing to use:
/// `unlimited` takes what it can, `balanced` keeps connection use moderate,
/// `low` restricts it hard. The mode only changes *defaults*; an explicit
/// bandwidth cap or connection budget always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficMode {
    Unlimited,
    Balanced,
    Low,
}

impl TrafficMode {
    /// Connection budget this mode allows when nothing explicit is set.
    /// Unlimited takes exactly what the user configured (parallel slots ×
    /// segments per file) — that is the peak the settings promise, and the
    /// global registry makes sure several workers cannot exceed it together.
    /// Balanced halves it, Low quarters it, both with a floor that keeps a
    /// transfer moving.
    pub fn default_budget(&self) -> usize {
        self.budget_for(max_parallel() * segments_setting())
    }

    /// The same mapping on a caller-supplied configuration — pure, so it can
    /// be tested without reading this machine's shell settings.
    pub fn budget_for(&self, configured: usize) -> usize {
        let configured = configured.clamp(1, 64);
        match self {
            TrafficMode::Unlimited => configured,
            TrafficMode::Balanced => (configured / 2).max(2),
            TrafficMode::Low => (configured / 4).max(1),
        }
    }
}

pub fn traffic_mode() -> TrafficMode {
    let raw = std::env::var("CATCH_BW_MODE").ok().or_else(|| {
        let home = std::env::var_os("HOME")?;
        let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
        let text = std::fs::read_to_string(&shell).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        find_widget_value(&v, "bwMode")?.as_str().map(String::from)
    });
    match raw.as_deref().map(str::trim) {
        Some("balanced") => TrafficMode::Balanced,
        Some("low") => TrafficMode::Low,
        _ => TrafficMode::Unlimited,
    }
}

/// Explicit global bandwidth cap in bytes/second (0 = none), from the widget
/// `bwCapKb` (KiB/s) or CATCH_BW_CAP_BPS. The cap is global: each worker is
/// allowed `cap / active workers` so the sum across processes stays within it.
pub fn bandwidth_cap_bps() -> u64 {
    if let Ok(raw) = std::env::var("CATCH_BW_CAP_BPS") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            return n;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let shell = std::path::PathBuf::from(home).join(".config/omarchy/shell.json");
        if let Ok(text) = std::fs::read_to_string(&shell) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(kb) = find_widget_value(&v, "bwCapKb").and_then(|x| x.as_u64()) {
                    return kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

/// Connection slices the bandwidth cap makes sensible: a cap of 1 MiB/s
/// cannot feed eight full-rate connections. `None` means no cap configured.
pub fn cap_slices() -> Option<usize> {
    let cap = bandwidth_cap_bps();
    if cap == 0 {
        return None;
    }
    Some((cap / (64 * 1024)).max(1) as usize)
}

fn find_widget_value<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        let entries = layout.get(section)?.as_array()?;
        for e in entries {
            if e.get("id")?.as_str()? == "io.github.tahadx.catch" {
                if let Some(found) = e.get(key) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn find_widget_segments(v: &serde_json::Value) -> Option<usize> {
    let layout = v.get("bar")?.get("layout")?;
    for section in ["left", "center", "right"] {
        let entries = match layout.get(section).and_then(|s| s.as_array()) {
            Some(e) => e,
            None => continue,
        };
        for e in entries {
            if e.get("id")?.as_str()? != "io.github.tahadx.catch" {
                continue;
            }
            let n = e.get("segments")?.as_u64()? as usize;
            if [1, 2, 4, 8].contains(&n) {
                return Some(n);
            }
        }
    }
    None
}

/// Pick a non-colliding destination inside `dir` for `filename`.
pub fn unique_destination(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], Some(&filename[i..])),
        _ => (filename, None),
    };
    for n in 1..10000 {
        let name = match ext {
            Some(e) => format!("{} ({}).{}", stem, n, &e[1..]),
            None => format!("{} ({})", stem, n),
        };
        // Strip the extra dot when ext already starts with one.
        let name = name.replace("..", ".");
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{}-{}", filename, new_id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: Status, pid: u32) -> Download {
        Download {
            id: id.to_string(),
            url: format!("https://example.com/{id}.bin"),
            filename: format!("{id}.bin"),
            status,
            bytes: 0,
            total: 0,
            error: String::new(),
            kind: DownloadKind::File,
            pid,
            claimed_at: 0,
            sched_hold: false,
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
        }
    }

    /// A queued row carries no pid and has no worker behind it yet, so
    /// reaping must leave it exactly as it is. `enqueue` builds rows this
    /// way on purpose: a row that claimed itself would look like an active
    /// transfer to the drainer while nothing was fetching it, and would hold
    /// a parallel slot until some later drain noticed.
    #[test]
    fn reaping_leaves_a_queued_row_alone() {
        let mut state = State {
            version: 1,
            downloads: vec![row("a", Status::Queued, 0), row("b", Status::Queued, 0)],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        assert_eq!(reap_orphans(&mut state), 0);
        assert!(state.downloads.iter().all(|d| d.status == Status::Queued));
        assert!(state.downloads.iter().all(|d| d.pid == 0));
    }

    /// The other half of the invariant: a Downloading row whose worker is
    /// gone has to come back as Queued, or a dead transfer would hold a slot
    /// forever. A live *catch worker* is covered by the parser tests below
    /// and by the end-to-end claim tests; here a vanished pid and an
    /// unrelated live process (init) both count as gone.
    #[test]
    fn a_downloading_row_without_a_live_worker_is_requeued() {
        let mut state = State {
            version: 1,
            downloads: vec![
                // pid 1 is init: alive, but never a catch worker, so it is
                // treated as gone rather than trusted.
                row("init", Status::Downloading, 1),
                row("dead", Status::Downloading, 0),
                // A large pid that cannot exist on a normal system.
                row("vanished", Status::Downloading, u32::MAX - 1),
            ],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let freed = reap_orphans(&mut state);
        assert!(state.downloads.iter().all(|d| d.status == Status::Queued));
        assert!(state.downloads.iter().all(|d| d.pid == 0));
        assert_eq!(freed, 3);
    }

    /// `/proc/PID/cmdline` uses NUL separators. An earlier substring check
    /// over the raw bytes could match "catch download" split across
    /// arguments, or miss the first argument entirely.
    #[test]
    fn cmdline_arguments_are_split_on_nul() {
        assert_eq!(
            parse_cmdline(b"/usr/bin/catch\0download\0abc-123\0"),
            vec!["/usr/bin/catch", "download", "abc-123"]
        );
        assert!(parse_cmdline(b"").is_empty());
        assert!(parse_cmdline(b"\0\0").is_empty());
    }

    #[test]
    fn only_the_matching_worker_command_holds_a_row() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The two real worker spellings, with or without a path.
        assert!(is_transfer_worker(&argv(&["catch", "download", "id"])));
        assert!(is_transfer_worker(&argv(&["catch", "video", "id"])));
        assert!(is_transfer_worker(&argv(&[
            "/home/u/.local/bin/catch-host",
            "download",
            "id"
        ])));
        // Another subcommand, a different entry, a truncated argv.
        assert!(!is_transfer_worker(&argv(&["catch", "add", "id"])));
        assert!(!is_transfer_worker(&argv(&["catch", "drain"])));
        assert!(!is_transfer_worker(&argv(&["catch", "download"])));
        // A shell whose *arguments* mention catch is not a worker.
        assert!(!is_transfer_worker(&argv(&[
            "bash",
            "-c",
            "catch download id"
        ])));
        // A different program named catch (or a renamed one) is still
        // accepted only by basename, which is the documented identity.
        assert!(!is_transfer_worker(&argv(&["catcher", "download", "id"])));
    }

    /// The fork bomb: a drainer reserves a row, spawns the worker, and exits.
    /// Between that exit and the worker stamping its own pid the row points at
    /// a dead pid. Reaping in that window handed the row straight back to the
    /// next drainer, and drain -> worker -> drain re-forked without end.
    /// A fresh claim must survive it.
    #[test]
    fn a_just_claimed_row_is_not_reaped() {
        let mut state = State {
            version: 1,
            downloads: vec![row("claimed", Status::Downloading, 0)],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        state.downloads[0].claimed_at = now_secs();
        assert_eq!(reap_orphans(&mut state), 0);
        assert_eq!(
            state.downloads[0].status,
            Status::Downloading,
            "a claim inside the grace window is not an orphan"
        );
        assert_eq!(state.downloads[0].pid, 0);
    }

    /// The same row must still be reclaimed once the worker really is gone,
    /// or a claim would hold a parallel slot for good.
    #[test]
    fn an_old_claim_is_reaped_when_the_worker_is_gone() {
        let mut state = State {
            version: 1,
            downloads: vec![row("claimed", Status::Downloading, 0)],
            saved: Default::default(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        state.downloads[0].claimed_at = now_secs() - CLAIM_GRACE.as_secs() - 1;
        assert_eq!(reap_orphans(&mut state), 1);
        assert_eq!(state.downloads[0].status, Status::Queued);
    }

    #[test]
    fn tilde_in_a_download_dir_is_home() {
        let home = std::path::Path::new("/home/you");
        assert_eq!(expand_home_in("~", home), home);
        assert_eq!(
            expand_home_in("~/Downloads/catch", home),
            home.join("Downloads/catch")
        );
        assert_eq!(expand_home_in("~ ", home), home);
        assert_eq!(
            expand_home_in("~other/x", home),
            std::path::PathBuf::from("~other/x")
        );
        assert_eq!(
            expand_home_in("/tmp/catch", home),
            std::path::PathBuf::from("/tmp/catch")
        );
    }

    #[test]
    fn categories_match_spec_table() {
        for (file, cat) in [
            ("a.mp4", "video"),
            ("a.mkv", "video"),
            ("a.avi", "video"),
            ("a.webm", "video"),
            ("a.mov", "video"),
            ("a.flv", "video"),
            ("a.m4v", "video"),
            ("a.mp3", "music"),
            ("a.flac", "music"),
            ("a.wav", "music"),
            ("a.m4a", "music"),
            ("a.ogg", "music"),
            ("a.aac", "music"),
            ("a.pdf", "documents"),
            ("a.doc", "documents"),
            ("a.docx", "documents"),
            ("a.xls", "documents"),
            ("a.xlsx", "documents"),
            ("a.ppt", "documents"),
            ("a.pptx", "documents"),
            ("a.txt", "documents"),
            ("a.odt", "documents"),
            ("a.zip", "compressed"),
            ("a.rar", "compressed"),
            ("a.7z", "compressed"),
            ("a.tar", "compressed"),
            ("a.gz", "compressed"),
            ("a.bz2", "compressed"),
            ("a.xz", "compressed"),
            ("a.AppImage", "programs"),
            ("a.deb", "programs"),
            ("a.rpm", "programs"),
            ("a.sh", "programs"),
            ("a.bin", "programs"),
            ("a.exe", "programs"),
        ] {
            assert_eq!(category_for(file), Some(cat), "file: {file}");
        }
    }

    #[test]
    fn unknown_fields_survive_round_trips() {
        // One version must never strip another's data: a stale backend
        // rewriting state used to erase `title` on every drain poke.
        let text = r#"{
          "version": 1,
          "downloads": [{
            "id": "1", "url": "https://x/", "filename": "f.zip",
            "status": "queued", "bytes": 1, "total": 2, "error": "",
            "kind": "file", "pid": 0, "sched_hold": false, "audio_only": false,
            "title": "T", "future_field": {"nested": [1, 2]}
          }],
          "top_extra": true
        }"#;
        let state: State = serde_json::from_str(text).unwrap();
        assert_eq!(state.downloads[0].title, "T");
        assert!(state.downloads[0].extra.contains_key("future_field"));
        let out = serde_json::to_string(&state).unwrap();
        assert!(out.contains("\"title\""), "{out}");
        assert!(out.contains("future_field"), "{out}");
        assert!(out.contains("top_extra"), "{out}");
    }

    #[test]
    fn categories_are_lowercase_and_opt_out() {
        assert_eq!(category_for("MOVIE.MP4"), Some("video"));
        assert_eq!(category_for("archive.tar.gz"), Some("compressed"));
        assert_eq!(category_for("noext"), None);
        assert_eq!(category_for("download-123"), None);
        assert_eq!(category_for(".hidden"), None);
        assert_eq!(category_for("weird.toolongext"), None);
        // The table still classifies, but nothing files a download away
        // unless CATCH_CATEGORIES=on (see dir_for_stays_flat...).
        assert_eq!(
            dir_for(&PathBuf::from("/dl"), "a.mp3"),
            PathBuf::from("/dl")
        );
    }

    #[test]
    fn type_folders_are_off_until_asked_for() {
        // Flat by default: a download lands in the download dir itself, not
        // in documents/ or video/. CATCH_CATEGORIES=on is the only way to get
        // the folders back.
        assert!(!categories_from(None));
        assert!(categories_from(Some("on")));
        assert!(categories_from(Some("ON")));
        assert!(!categories_from(Some("")));
        assert!(!categories_from(Some("off")));
        assert!(!categories_from(Some("  off  ")));
    }

    /// Flat is the default, so `dir_for` must hand back the base directory
    /// for every extension a category table knows about.
    #[test]
    fn dir_for_stays_flat_without_the_env_opt_in() {
        let base = std::path::Path::new("/downloads");
        for name in ["a.mp4", "a.mp3", "a.pdf", "a.zip", "a.deb", "a.png"] {
            assert_eq!(dir_for(base, name), base, "{name} was filed away");
        }
        assert_eq!(video_dir(base), base);
    }
}

#[cfg(test)]
mod video_page_tests {
    use super::*;

    #[test]
    fn watch_and_shorts_route_to_ytdlp() {
        for u in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=abc&list=xyz",
            "https://m.youtube.com/watch?v=abc",
            "https://music.youtube.com/watch?v=abc",
            "https://www.youtube.com/shorts/abc123XYZ-_",
            "https://youtu.be/abc123XYZ-_",
            "https://www.youtube.com/live/abc123",
            "https://vimeo.com/123456789",
        ] {
            assert!(looks_like_video_page(u), "missed: {u}");
        }
    }

    #[test]
    fn plain_links_stay_files() {
        for u in [
            "https://example.com/",
            "https://example.com/file.zip",
            "https://github.com/a/b/releases/download/v1/f.apk",
            "https://notyoutube.com/watch?v=abc",
            "https://youtube.com/",
            "https://youtube.com/results?search_query=x",
            "not a url",
            "",
        ] {
            assert!(!looks_like_video_page(u), "false hit: {u}");
        }
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    fn download(status: Status) -> Download {
        Download {
            id: new_id(),
            url: "https://x.test/a".to_string(),
            filename: "a.zip".to_string(),
            status,
            bytes: 0,
            total: 0,
            error: String::new(),
            kind: DownloadKind::File,
            pid: 0,
            claimed_at: 0,
            sched_hold: false,
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
        }
    }

    fn state_of(statuses: &[Status]) -> State {
        State {
            version: STATE_VERSION,
            downloads: statuses.iter().map(|s| download(s.clone())).collect(),
            saved: std::collections::BTreeMap::new(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn live_rows_are_reused_and_saved_ones_are_not() {
        // A link already on disk must be catchable again: the new row lands
        // as another file (unique_destination picks the name).
        for done in [
            Status::Done,
            Status::Error,
            Status::Cancelled,
            Status::Paused,
        ] {
            assert!(
                live_row_for(&state_of(std::slice::from_ref(&done)), "https://x.test/a").is_none(),
                "{done} must not swallow a re-catch"
            );
        }
        for live in [Status::Queued, Status::Downloading] {
            assert!(
                live_row_for(&state_of(std::slice::from_ref(&live)), "https://x.test/a").is_some(),
                "{live} must be reused"
            );
        }
        assert!(live_row_for(&State::default(), "https://x.test/a").is_none());
    }
}

/// The saved index is what still knows a link after its queue row is gone.
#[cfg(test)]
mod saved_tests {
    use super::*;

    fn saved_rec(path: &str, kind: DownloadKind) -> SavedFile {
        SavedFile {
            path: path.to_string(),
            kind,
            bytes: 7,
            at: 1_700_000_000,
        }
    }

    #[test]
    fn saved_index_outlives_the_queue() {
        let mut state = State {
            version: STATE_VERSION,
            downloads: Vec::new(),
            saved: std::collections::BTreeMap::new(),
            asks: Vec::new(),
            extra: serde_json::Map::new(),
        };
        record_saved(
            &mut state,
            "https://x.test/a",
            Path::new("/dl"),
            Path::new("/dl/compressed/a.zip"),
            DownloadKind::File,
        );
        record_saved(
            &mut state,
            "https://youtu.be/v",
            Path::new("/dl"),
            Path::new("/dl/video/clip.webm"),
            DownloadKind::Video,
        );
        // The paths are relative to the download dir, so they survive the
        // user moving that folder.
        assert_eq!(
            saved_match(&state, "https://x.test/a").map(|r| r.path.clone()),
            Some("compressed/a.zip".to_string())
        );
        assert_eq!(
            saved_match(&state, "https://youtu.be/v").map(|r| r.kind),
            Some(DownloadKind::Video)
        );
        assert!(saved_match(&state, "https://x.test/other").is_none());

        // Clearing the queue drops the rows and keeps the index, which is
        // the whole point: the files are still on disk.
        let mut cleared = state.clone();
        cleared.downloads.clear();
        assert!(saved_match(&cleared, "https://x.test/a").is_some());

        // It round-trips through the state file, so a reload keeps it.
        let text = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&text).unwrap();
        assert_eq!(
            saved_match(&back, "https://x.test/a").map(|r| r.path.clone()),
            Some("compressed/a.zip".to_string())
        );
    }

    #[test]
    fn saved_index_tolerates_older_state_files() {
        // A state file written before the index existed must still load.
        let old: State = serde_json::from_str(
            r#"{"version":1,"downloads":[{"id":"a","url":"https://x.test/a","filename":"a.zip","status":"done"}],"somethingNew":true}"#,
        )
        .unwrap();
        // A link caught before the index existed is still recognised, through its
        // finished row, with no recorded path: callers fall back to the row's name.
        assert_eq!(
            saved_match(&old, "https://x.test/a").map(|r| (r.path, r.kind)),
            Some((String::new(), DownloadKind::File)),
            "pre-index rows must not become invisible"
        );
        // Nothing at all for the link means nothing saved.
        assert!(saved_match(&old, "https://x.test/other").is_none());
        // A queued row is not a saved file.
        let mut queued = old.clone();
        queued.downloads[0].status = Status::Queued;
        assert!(saved_match(&queued, "https://x.test/a").is_none());
        // Unknown top-level keys survive a load/save cycle: an older binary
        // must not eat the index when it writes the file back.
        let again: State = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(
            again.extra.get("somethingNew"),
            Some(&serde_json::json!(true))
        );

        // Stand-in for the binary from before the index existed: it knows
        // only these fields and passes the rest through, so the index (and
        // any other key a newer Catch adds) is handed back untouched.
        #[derive(Serialize, Deserialize)]
        struct LegacyState {
            #[serde(default)]
            downloads: Vec<serde_json::Value>,
            #[serde(flatten)]
            extra: serde_json::Map<String, serde_json::Value>,
        }
        let mut with_index = old;
        with_index.saved.insert(
            "https://x.test/a".into(),
            saved_rec("a.zip", DownloadKind::File),
        );
        let text = serde_json::to_string(&with_index).unwrap();
        let legacy: LegacyState = serde_json::from_str(&text).unwrap();
        assert!(legacy.downloads.len() == 1, "known fields still read");
        let back = serde_json::to_string(&legacy).unwrap();
        assert!(back.contains("\"saved\""), "index kept: {back}");
        assert!(
            back.contains("\"somethingNew\""),
            "unknown keys kept: {back}"
        );
    }

    #[test]
    fn relative_paths_fall_back_to_the_full_path() {
        assert_eq!(
            relative_to_base(Path::new("/dl"), Path::new("/dl/video/a.webm")),
            "video/a.webm"
        );
        // Outside the download dir (a stale base): keep the name rather than
        // inventing a path.
        assert_eq!(
            relative_to_base(Path::new("/dl"), Path::new("/other/a.zip")),
            "/other/a.zip"
        );
    }

    #[test]
    fn locate_saved_requires_the_file_on_disk() {
        let dir = std::env::temp_dir().join(format!("catch-locate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("compressed")).unwrap();
        std::fs::write(dir.join("compressed/a.zip"), b"hello").unwrap();

        let mut state = State::default();
        state.saved.insert(
            "https://x.test/a".into(),
            saved_rec("compressed/a.zip", DownloadKind::File),
        );
        let ask = locate_saved(&state, &dir, "https://x.test/a").expect("file is there");
        assert_eq!(ask.path, "compressed/a.zip");
        assert_eq!(ask.filename, "a.zip");
        assert_eq!(ask.bytes, 5);
        assert!(locate_saved(&state, &dir, "https://x.test/other").is_none());

        std::fs::remove_file(dir.join("compressed/a.zip")).unwrap();
        assert!(locate_saved(&state, &dir, "https://x.test/a").is_none());
        assert!(indexed_file_missing(&state, &dir, "https://x.test/a"));

        // A finished row from before the index: the file's real location counts.
        let dest = dir_for(&dir, "app.apk").join("app.apk");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"apk").unwrap();
        state.saved.clear();
        state.downloads.push(Download {
            id: "old".into(),
            url: "https://x.test/apk".into(),
            filename: "app.apk".into(),
            status: Status::Done,
            bytes: 3,
            total: 3,
            error: String::new(),
            kind: DownloadKind::File,
            pid: 0,
            claimed_at: 0,
            sched_hold: false,
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
            title: "App".into(),
            extra: serde_json::Map::new(),
        });
        let legacy = locate_saved(&state, &dir, "https://x.test/apk").expect("legacy file");
        assert_eq!(legacy.filename, "app.apk");
        assert_eq!(legacy.title, "App");
        assert!(legacy.path.ends_with("app.apk"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asks_keep_the_newest_question_first() {
        let mut state = State::default();
        push_ask(
            &mut state,
            Ask {
                url: "https://x.test/a".into(),
                title: String::new(),
                filename: "a.zip".into(),
                kind: DownloadKind::File,
                path: "a.zip".into(),
                bytes: 1,
                live: false,
            },
        );
        push_ask(
            &mut state,
            Ask {
                url: "https://x.test/b".into(),
                title: String::new(),
                filename: "b.zip".into(),
                kind: DownloadKind::File,
                path: "b.zip".into(),
                bytes: 2,
                live: false,
            },
        );
        // Asking about `a` again promotes it, it does not stack a second row.
        push_ask(
            &mut state,
            Ask {
                url: "https://x.test/a".into(),
                title: "A".into(),
                filename: "a.zip".into(),
                kind: DownloadKind::File,
                path: "a.zip".into(),
                bytes: 1,
                live: false,
            },
        );
        assert_eq!(state.asks.len(), 2);
        assert_eq!(state.asks[0].url, "https://x.test/a");
        assert_eq!(state.asks[0].title, "A");
        clear_ask(&mut state, "https://x.test/a");
        assert_eq!(state.asks.len(), 1);
        assert_eq!(state.asks[0].url, "https://x.test/b");
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn rename_plan_shapes_titles() {
        assert_eq!(
            rename_plan("a.webm", "New Name"),
            Some(("New Name".to_string(), "New Name.webm".to_string()))
        );
        assert_eq!(
            rename_plan("a.webm", "bad/na\\me"),
            Some(("badname".to_string(), "badname.webm".to_string()))
        );
        assert_eq!(
            rename_plan("noext", "Title"),
            Some(("Title".to_string(), "Title".to_string()))
        );
        assert_eq!(
            rename_plan("x.zip", "dots..trim."),
            Some(("dots..trim".to_string(), "dots..trim.zip".to_string()))
        );
        assert_eq!(rename_plan("x.zip", "   ...  "), None);
        assert_eq!(rename_plan("x.zip", "\u{7}"), None);
        // A title that already carries the extension is the whole file name:
        // appending again used to write "ddr34.apk.apk" to disk.
        assert_eq!(
            rename_plan("ddr34.apk.apk", "ddr35.apk"),
            Some(("ddr35.apk".to_string(), "ddr35.apk".to_string()))
        );
        assert_eq!(
            rename_plan("kleeamp.apk", "Kleeamp 0.4.2.APK"),
            Some((
                "Kleeamp 0.4.2.APK".to_string(),
                "Kleeamp 0.4.2.APK".to_string()
            )),
            "case does not matter"
        );
        // A different extension typed on purpose keeps the old behaviour of getting
        // this file's own: guessing cannot tell "clip.mkv" from a title.
        assert_eq!(
            rename_plan("clip.webm", "clip.mkv"),
            Some(("clip.mkv".to_string(), "clip.mkv.webm".to_string()))
        );
        // Dots that are part of the title, not an extension, still get the
        // file's own extension appended.
        assert_eq!(
            rename_plan("app.apk", "v1.2.3 release"),
            Some((
                "v1.2.3 release".to_string(),
                "v1.2.3 release.apk".to_string()
            ))
        );
        assert_eq!(
            rename_plan("app.apk", "my.file.backup"),
            Some((
                "my.file.backup".to_string(),
                "my.file.backup.apk".to_string()
            )),
            "a long tail is a title, not an extension"
        );
        assert_eq!(
            rename_plan("archive.tar.gz", "notes"),
            Some(("notes".to_string(), "notes.gz".to_string())),
            "only the last extension is the file's own"
        );
        let long = rename_plan("x.zip", &"y".repeat(200)).unwrap();
        assert_eq!(long.0.chars().count(), 120);
        assert!(long.1.ends_with(".zip"));
    }
}

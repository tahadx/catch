use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;

/// Native Messaging wire: u32 LE length + UTF-8 JSON, both directions.
/// Chromium-family browsers spawn the host per message batch; Firefox keeps
/// it alive. This loop handles both: it exits cleanly on EOF.
///
/// A malformed *body* must not take the whole host down: the length frame is
/// still in sync, so the error is answered and the next message is served.
/// A bad length is different — without a trustworthy frame there is no way
/// to find the next message, so the loop stops rather than misparse garbage.
const MAX_FRAME: usize = 8 * 1024 * 1024;

pub async fn run_loop() -> Result<()> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    loop {
        let mut len_buf = [0u8; 4];
        match handle.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => anyhow::bail!("read native message length: {e}"),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            // A zero-length frame carries nothing to do. Answering keeps a
            // browser that sends one from waiting for ever.
            write_message(&Reply::error("empty native message", 0))?;
            continue;
        }
        if len > MAX_FRAME {
            anyhow::bail!("native message length out of range: {len}");
        }
        let mut buf = vec![0u8; len];
        handle
            .read_exact(&mut buf)
            .context("read native message body")?;
        let msg: Incoming = match serde_json::from_slice(&buf) {
            Ok(m) => m,
            Err(e) => {
                write_message(&Reply::error(&format!("invalid native message: {e}"), 0))?;
                continue;
            }
        };
        let mut reply = handle_message(&msg).await;
        reply.req = msg.req;
        write_message(&reply)?;
        if std::env::var_os("CATCH_HOST_SINGLE_SHOT").is_some() {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Incoming {
    #[serde(default)]
    url: String,
    #[serde(default)]
    page_url: String,
    /// Copy this URL to the Wayland clipboard instead of downloading.
    #[serde(default)]
    copy: String,
    /// Fetch with yt-dlp (video pages) instead of a plain download.
    #[serde(default)]
    video: String,
    /// Audio-only extraction for video entries.
    #[serde(default, alias = "audioOnly")]
    audio_only: bool,
    /// Out-of-band action: "open-folder".
    #[serde(default)]
    action: String,
    /// Page title, used as the queue label for videos.
    #[serde(default)]
    title: String,
    /// Browser-resolved filename (download interception), preferred as-is.
    #[serde(default, alias = "filenameHint")]
    filename_hint: String,
    /// Correlation id echoed back in the reply. A persistent port serves many
    /// messages over one process, so the browser needs to know which reply
    /// answers which request.
    #[serde(default)]
    req: u64,
    /// A whole page's worth of links in one message: strings, or objects with
    /// per-link title/kind overrides. They are accepted under one state lock
    /// and woken with one drain, so a thousand-link page costs one write, not
    /// a thousand process wake-ups.
    #[serde(default)]
    urls: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// The file is already in the download folder. Catch asked instead of
    /// fetching another copy. `ok` stays true so the browser lets go of the
    /// download: the panel is the place that decides. Omitted when false so
    /// older readers see the same reply they always did.
    #[serde(default, skip_serializing_if = "is_false")]
    already: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Per-link results for a batch, in request order. Omitted for a single
    /// message so the original reply shape is untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    results: Vec<BatchResult>,
    /// Count of batch entries accepted (queued or turned into a question).
    #[serde(default, skip_serializing_if = "is_zero")]
    accepted: usize,
    /// Echo of the request's `req`, so a shared port can pair replies up.
    #[serde(default)]
    req: u64,
}

impl Reply {
    fn error(message: &str, req: u64) -> Reply {
        Reply {
            ok: false,
            id: None,
            already: false,
            error: Some(message.to_string()),
            results: Vec::new(),
            accepted: 0,
            req,
        }
    }

    fn accepted(id: String) -> Reply {
        Reply {
            ok: true,
            id: Some(id),
            already: false,
            error: None,
            results: Vec::new(),
            accepted: 0,
            req: 0,
        }
    }

    fn asked() -> Reply {
        Reply {
            ok: true,
            id: None,
            already: true,
            error: None,
            results: Vec::new(),
            accepted: 0,
            req: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct BatchResult {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    already: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// One link requested by the browser, with its per-link options resolved.
#[derive(Debug, Clone)]
struct Requested {
    url: String,
    title: String,
    hint: String,
    video: bool,
    audio_only: bool,
    /// Position in the browser's batch, so results can be returned in order.
    pos: usize,
}

/// What accepting one link did.
#[derive(Debug, Clone)]
struct Outcome {
    url: String,
    id: Option<String>,
    already: bool,
    error: Option<String>,
    /// Position in the browser's batch; never serialized.
    pos: usize,
}

impl Outcome {
    fn failed(pos: usize, url: String, why: String) -> Outcome {
        Outcome {
            url,
            id: None,
            already: false,
            error: Some(why),
            pos,
        }
    }
}

async fn handle_message(msg: &Incoming) -> Reply {
    // Keepalive. Chrome suspends an idle MV3 service worker, which tears the
    // native port down and kills this host; the extension pings so the worker
    // and the port stay alive and a click never pays for a reconnect. Must do
    // nothing at all.
    if msg.action.trim() == "ping" {
        return Reply {
            req: 0,
            ok: true,
            id: None,
            already: false,
            error: None,
            results: Vec::new(),
            accepted: 0,
        };
    }
    if msg.action.trim() == "open-folder" {
        let dir = crate::state::resolve_download_dir(None);
        let ok = std::process::Command::new("xdg-open")
            .arg(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return Reply {
            req: 0,
            ok,
            id: None,
            already: false,
            error: if ok {
                None
            } else {
                Some("could not open the folder".into())
            },
            results: Vec::new(),
            accepted: 0,
        };
    }
    if !msg.copy.trim().is_empty() {
        return match copy_to_clipboard(msg.copy.trim()) {
            Ok(()) => Reply {
                req: 0,
                ok: true,
                id: None,
                already: false,
                error: None,
                results: Vec::new(),
                accepted: 0,
            },
            Err(e) => Reply::error(&format!("{e:#}"), 0),
        };
    }

    if !msg.urls.is_empty() {
        return handle_batch(&msg.urls).await;
    }

    let url = if !msg.video.trim().is_empty() {
        msg.video.trim().to_string()
    } else if !msg.url.trim().is_empty() {
        msg.url.trim().to_string()
    } else {
        msg.page_url.trim().to_string()
    };
    // A bare watch link caught as a file would only save HTML.
    let want_video = !msg.video.trim().is_empty() || crate::state::looks_like_video_page(&url);
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Reply::error("only http(s) URLs can be caught", 0);
    }
    let item = Requested {
        url,
        title: msg.title.trim().to_string(),
        hint: msg.filename_hint.trim().to_string(),
        video: want_video,
        audio_only: msg.audio_only,
        pos: 0,
    };
    match accept(vec![item]).await {
        Ok(mut outcomes) => match outcomes.pop() {
            Some(o) if o.error.is_some() => Reply::error(o.error.as_deref().unwrap_or(""), 0),
            Some(o) if o.id.is_some() => Reply::accepted(o.id.expect("checked")),
            Some(_) => Reply::asked(),
            None => Reply::error("nothing accepted", 0),
        },
        Err(e) => Reply::error(&format!("{e:#}"), 0),
    }
}

/// Turn a browser batch into per-link results. Invalid entries answer with an
/// error instead of failing the whole page: one bad URL must not discard the
/// other hundred. Results keep request order.
async fn handle_batch(values: &[Value]) -> Reply {
    let mut requested = Vec::new();
    let mut slots: Vec<Option<Outcome>> = (0..values.len()).map(|_| None).collect();
    for (pos, value) in values.iter().enumerate() {
        match batch_item(value, pos) {
            Ok(item) => requested.push(item),
            Err(outcome) => slots[pos] = Some(outcome),
        }
    }
    match accept(requested).await {
        Ok(outcomes) => {
            for outcome in outcomes {
                let pos = outcome.pos;
                slots[pos] = Some(outcome);
            }
        }
        Err(e) => {
            let why = format!("{e:#}");
            for slot in slots.iter_mut() {
                if slot.is_none() {
                    *slot = Some(Outcome::failed(0, String::new(), why.clone()));
                }
            }
        }
    }
    let outcomes: Vec<Outcome> = slots.into_iter().flatten().collect();
    let accepted = outcomes.iter().filter(|o| o.error.is_none()).count();
    let results: Vec<BatchResult> = outcomes
        .iter()
        .map(|o| BatchResult {
            url: o.url.clone(),
            id: o.id.clone(),
            already: o.already,
            error: o.error.clone(),
        })
        .collect();
    let fatal = outcomes
        .iter()
        .all(|o| o.error.is_some())
        .then(|| outcomes.first().and_then(|o| o.error.clone()))
        .flatten();
    Reply {
        // A batch with some bad links is still a success for the good ones;
        // the per-link errors tell the page what was rejected.
        ok: accepted > 0 || values.is_empty(),
        id: None,
        already: false,
        error: fatal,
        results,
        accepted,
        req: 0,
    }
}

fn batch_item(value: &Value, pos: usize) -> std::result::Result<Requested, Outcome> {
    let (url, title, hint, video, audio) = match value {
        Value::String(s) => (s.clone(), String::new(), String::new(), false, false),
        Value::Object(map) => {
            let get = |key: &str| {
                map.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string()
            };
            let url = get("url");
            let video = map.get("video").and_then(Value::as_bool).unwrap_or(false);
            let audio = map
                .get("audio_only")
                .or_else(|| map.get("audioOnly"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let hint = map
                .get("filename_hint")
                .or_else(|| map.get("filenameHint"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            (url, get("title"), hint, video, audio)
        }
        other => {
            return Err(Outcome::failed(
                pos,
                String::new(),
                format!(
                    "batch entry must be a URL string or an object, got {}",
                    kind_of(other)
                ),
            ));
        }
    };
    let url = url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Outcome::failed(
            pos,
            url,
            "only http(s) URLs can be caught".to_string(),
        ));
    }
    let video = video || crate::state::looks_like_video_page(&url);
    Ok(Requested {
        url,
        title,
        hint,
        video,
        audio_only: audio,
        pos,
    })
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Accept every link in one state write. A duplicate within the batch (or
/// an already-live row) points at the same row instead of stacking another
/// one. Existing files become questions, never quiet second copies.
async fn accept(items: Vec<Requested>) -> Result<Vec<Outcome>> {
    use crate::state::{
        filename_for_url, indexed_file_missing, live_row_for, locate_saved, new_id, push_ask,
        sanitize_filename, update_state, Ask, Download, DownloadKind, Status,
    };

    if items.is_empty() {
        return Ok(Vec::new());
    }
    let base = crate::state::resolve_download_dir(None);
    std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;

    let mut outcomes: Vec<Outcome> = items
        .iter()
        .map(|i| Outcome {
            url: i.url.clone(),
            id: None,
            already: false,
            error: None,
            pos: i.pos,
        })
        .collect();
    let mut first_for_url: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut notify: Vec<(String, bool)> = Vec::new();

    update_state(|state| {
        for (index, item) in items.iter().enumerate() {
            if let Some(&first) = first_for_url.get(&item.url) {
                // Duplicate in the same page: one row, same answer.
                outcomes[index].id = outcomes[first].id.clone();
                outcomes[index].already = outcomes[first].already;
                outcomes[index].error = outcomes[first].error.clone();
                continue;
            }
            first_for_url.insert(item.url.clone(), index);

            // A saved record whose file vanished is not a duplicate; forget
            // it so the link can be caught again.
            if state.saved.contains_key(&item.url) && indexed_file_missing(state, &base, &item.url)
            {
                state.saved.remove(&item.url);
            }

            if let Some(row) = live_row_for(state, &item.url) {
                let mut ask = Ask {
                    url: item.url.clone(),
                    title: if row.title.is_empty() {
                        item.title.clone()
                    } else {
                        row.title.clone()
                    },
                    filename: row.filename.clone(),
                    kind: row.kind,
                    path: String::new(),
                    bytes: row.bytes,
                    live: true,
                };
                if ask.filename.is_empty() {
                    ask.filename = item.url.clone();
                }
                let label = ask.filename.clone();
                push_ask(state, ask);
                outcomes[index].already = true;
                notify.push((label, true));
                continue;
            }

            if let Some(mut ask) = locate_saved(state, &base, &item.url) {
                if ask.title.is_empty() {
                    ask.title = item.title.clone();
                }
                let label = if ask.filename.is_empty() {
                    ask.path.clone()
                } else {
                    ask.filename.clone()
                };
                push_ask(state, ask);
                outcomes[index].already = true;
                notify.push((label, false));
                continue;
            }

            let id = new_id();
            // The browser hint is untrusted too: sanitize it to one safe
            // path segment before it can ever be joined onto the folder.
            let hinted = sanitize_filename(&item.hint);
            let filename = if hinted.is_empty() {
                filename_for_url(&item.url, &id)
            } else {
                hinted
            };
            state.downloads.push(Download {
                id: id.clone(),
                url: item.url.clone(),
                filename,
                title: item.title.clone(),
                status: Status::Queued,
                bytes: 0,
                total: 0,
                error: String::new(),
                kind: if item.video {
                    DownloadKind::Video
                } else {
                    DownloadKind::File
                },
                pid: 0,
                claimed_at: crate::state::now_secs(),
                sched_hold: false,
                scheduled_at: 0,
                sched_owned: false,
                dir: String::new(),
                audio_only: item.audio_only,
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
            outcomes[index].id = Some(id);
        }
        Ok(())
    })?;

    // Desktop feedback and the drainer wake-up happen after the single state
    // write: a page of already-saved links gets the same per-link notices it
    // had before batching, and a page of new links gets one drain.
    for (label, live) in notify {
        crate::notify_url_saved(&label, live);
    }
    if outcomes.iter().any(|o| o.id.is_some()) {
        crate::state::spawn_drain();
    }
    Ok(outcomes)
}

fn copy_to_clipboard(url: &str) -> Result<()> {
    let status = std::process::Command::new("wl-copy")
        .args(["--type", "text/plain"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn wl-copy")
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .context("open wl-copy stdin")?
                .write_all(url.as_bytes())
                .context("write clipboard")?;
            child.wait().context("wait wl-copy")
        })?;
    if !status.success() {
        anyhow::bail!("wl-copy exited {status}");
    }
    let _ = std::process::Command::new("omarchy-notification-send")
        .args(["-g", "󰅍", "URL copied to clipboard"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn write_message<T: Serialize>(msg: &T) -> Result<()> {
    use std::io::Write;
    let body = serde_json::to_vec(msg)?;
    let len = (body.len() as u32).to_le_bytes();
    let mut out = std::io::stdout().lock();
    out.write_all(&len)?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;

/// Native Messaging wire: u32 LE length + UTF-8 JSON, both directions.
/// Chromium-family browsers spawn the host per message batch; Firefox keeps
/// it alive. This loop handles both: it exits cleanly on EOF.
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
        if len == 0 || len > 1024 * 1024 {
            anyhow::bail!("native message length out of range: {len}");
        }
        let mut buf = vec![0u8; len];
        handle
            .read_exact(&mut buf)
            .context("read native message body")?;
        let msg: Incoming = serde_json::from_slice(&buf).context("parse native message")?;
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
    /// Echo of the request's `req`, so a shared port can pair replies up.
    #[serde(default)]
    req: u64,
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
            },
            Err(e) => Reply {
                req: 0,
                ok: false,
                id: None,
                already: false,
                error: Some(format!("{e:#}")),
            },
        };
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
        return Reply {
            req: 0,
            ok: false,
            id: None,
            already: false,
            error: Some("only http(s) URLs can be caught".into()),
        };
    }
    let title = msg.title.trim().to_string();
    let hint = msg.filename_hint.trim().to_string();
    let audio = msg.audio_only;
    match enqueue_and_spawn(&url, want_video, title, hint, audio).await {
        Ok(Some(id)) => Reply {
            req: 0,
            ok: true,
            id: Some(id),
            already: false,
            error: None,
        },
        Ok(None) => Reply {
            req: 0,
            ok: true,
            id: None,
            already: true,
            error: None,
        },
        Err(e) => Reply {
            req: 0,
            ok: false,
            id: None,
            already: false,
            error: Some(format!("{e:#}")),
        },
    }
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

/// Enqueue without downloading inline (the browser kills slow hosts), then
/// spawn a detached worker so the file still lands and the bar widget
/// follows progress through the state file.
async fn enqueue_and_spawn(
    url: &str,
    video: bool,
    title: String,
    hint: String,
    audio_only: bool,
) -> Result<Option<String>> {
    use crate::state::{
        clean_label, filename_for_url, new_id, update_state, Download, DownloadKind,
    };

    // Already in the download folder: ask in the panel, do not start a
    // second transfer. `None` is that case. A live row still falls through
    // and is reused below.
    if crate::raise_ask(url, &title)?.is_some() {
        return Ok(None);
    }

    let id = new_id();
    // Browser-resolved names first (interception knows the real file);
    // otherwise the URL tail. The human title travels separately and is
    // resolved properly by the worker — never baked into the filename.
    let filename = if !hint.is_empty() {
        clean_label(&hint, &format!("download-{id}"))
    } else {
        filename_for_url(url, &id)
    };
    let mut reuse: Option<String> = None;
    update_state(|state| {
        // One live row per link: a re-sent message (retry, double click)
        // reuses the transfer already underway. A link already on disk
        // never reaches here — raise_ask recorded the question instead.
        if let Some(d) = crate::state::live_row_for(state, url) {
            reuse = Some(d.id.clone());
            return Ok(());
        }
        state.downloads.push(Download {
            id: id.clone(),
            url: url.to_string(),
            filename: filename.clone(),
            title: title.clone(),
            status: crate::state::Status::Queued,
            bytes: 0,
            total: 0,
            error: String::new(),
            kind: if video {
                DownloadKind::Video
            } else {
                DownloadKind::File
            },
            pid: 0,
            claimed_at: crate::state::now_secs(),
            sched_hold: false,
            audio_only,
            extra: serde_json::Map::new(),
        });
        Ok(())
    })?;
    if let Some(existing) = reuse {
        crate::state::spawn_drain();
        return Ok(Some(existing));
    }

    // Wake the drainer: it starts this entry when a parallel slot is free.
    crate::state::spawn_drain();
    Ok(Some(id))
}

fn is_false(value: &bool) -> bool {
    !*value
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

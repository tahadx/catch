// Test support for the end-to-end suite: an isolated HOME per test, a
// scriptable local HTTP server, and bounded process helpers. No external
// crates and no network access.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub fn scratch_root() -> PathBuf {
    PathBuf::from("/tmp/opencode/catch-tests")
}

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub state_home: PathBuf,
    pub config_home: PathBuf,
    /// XDG_DOWNLOAD_DIR. Catch appends /catch, so the real base is `base()`.
    pub downloads: PathBuf,
    pub bin: PathBuf,
}

impl Sandbox {
    pub fn new(name: &str) -> Sandbox {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let root = scratch_root().join(format!("{}-{n}-{safe}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let state_home = root.join("state");
        let config_home = root.join("config");
        let downloads = root.join("downloads");
        let bin = root.join("bin");
        for dir in [&home, &state_home, &config_home, &downloads, &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Sandbox {
            root,
            home,
            state_home,
            config_home,
            downloads,
            bin,
        }
    }

    pub fn base(&self) -> PathBuf {
        self.downloads.join("catch")
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_home.join("omarchy/catch/state.json")
    }

    fn apply_env(&self, cmd: &mut Command) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DOWNLOAD_DIR", &self.downloads)
            .env("PATH", path)
            .env("CATCH_MAX_PARALLEL", "1")
            .env("CATCH_MAX_ATTEMPTS", "3")
            .env("CATCH_STALL_SECS", "2")
            .env("CATCH_RESPONSE_SECS", "3")
            .env_remove("CATCH_CATEGORIES")
            .env_remove("CATCH_TEST_KEEP");
    }

    pub fn cmd(&self) -> Command {
        self.cmd_for(env!("CARGO_BIN_EXE_catch"))
    }

    /// A catch binary copied somewhere else (for layout-sensitive tests).
    pub fn cmd_for(&self, program: impl AsRef<Path>) -> Command {
        let mut cmd = Command::new(program.as_ref());
        self.apply_env(&mut cmd);
        cmd
    }

    pub fn host_cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catch-host"));
        self.apply_env(&mut cmd);
        cmd
    }

    pub fn state(&self) -> Value {
        let text = std::fs::read_to_string(self.state_path()).unwrap_or_else(|_| "{}".into());
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("state is not JSON ({e}): {text}"))
    }

    pub fn downloads(&self) -> Vec<Value> {
        self.state()
            .get("downloads")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    pub fn row(&self, id: &str) -> Option<Value> {
        self.downloads()
            .into_iter()
            .find(|d| d.get("id").and_then(Value::as_str) == Some(id))
    }

    pub fn row_for_url(&self, url: &str) -> Option<Value> {
        self.downloads()
            .into_iter()
            .find(|d| d.get("url").and_then(Value::as_str) == Some(url))
    }

    pub fn status(&self, id: &str) -> Option<String> {
        self.row(id)
            .and_then(|d| d.get("status").and_then(Value::as_str).map(str::to_string))
    }

    pub fn seed(&self, rows: Vec<Value>) {
        let state = json!({
            "version": 1,
            "downloads": rows,
            "saved": {},
            "asks": [],
        });
        self.write_state(&state);
    }

    pub fn write_state(&self, state: &Value) {
        let path = self.state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(state).unwrap()).unwrap();
    }

    pub fn part(&self, id: &str, filename: &str) -> PathBuf {
        self.base().join(format!("{filename}.{id}.part"))
    }

    pub fn fake_ytdlp(&self, body: &str) {
        let path = self.bin.join("yt-dlp");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if std::env::var_os("CATCH_TEST_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

pub fn row(id: &str, url: &str, filename: &str, status: &str) -> Value {
    json!({
        "id": id,
        "url": url,
        "filename": filename,
        "title": "",
        "kind": "file",
        "status": status,
        "bytes": 0,
        "total": 0,
        "error": "",
        "pid": 0,
        "claimed_at": 0,
        "sched_hold": false,
        "audio_only": false,
        "etag": "",
        "last_modified": ""
    })
}

pub fn video_row(id: &str, url: &str) -> Value {
    let mut r = row(id, url, "video.bin", "queued");
    r["kind"] = json!("video");
    r
}

pub fn run(mut cmd: Command, limit: Duration) -> Output {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Output {
                status,
                stdout: out.join().unwrap(),
                stderr: err.join().unwrap(),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "command timed out after {limit:?}: {cmd:?}\nstderr: {}",
                String::from_utf8_lossy(&err.join().unwrap())
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn run_ok(cmd: Command, limit: Duration) -> Output {
    let out = run(cmd, limit);
    assert!(
        out.status.success(),
        "command failed ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

pub fn wait_until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    loop {
        if f() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn native_frame(value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn read_native_frame<R: Read>(input: &mut R) -> Value {
    let mut len = [0u8; 4];
    input.read_exact(&mut len).expect("read frame length");
    let len = u32::from_le_bytes(len) as usize;
    assert!(len > 0 && len <= 8 * 1024 * 1024, "frame length {len}");
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).expect("read frame body");
    serde_json::from_slice(&body).expect("frame is JSON")
}

// ---------- local HTTP server ----------

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn range_start(&self) -> Option<u64> {
        let raw = self.header("range")?;
        let rest = raw.strip_prefix("bytes=")?;
        let start = rest.split('-').next()?;
        start.parse().ok()
    }
}

pub enum Body {
    Bytes(Vec<u8>),
    /// Send these bytes (fewer than the declared Content-Length), wait, then
    /// close: a stalled or truncated response.
    Stalled {
        bytes: Vec<u8>,
        sleep: Duration,
    },
    Chunked(Vec<(Vec<u8>, Duration)>),
    Empty,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Explicit Content-Length. None auto-fills the body length for Bytes and
    /// Empty; a Stalled body should set it to the promised, larger total.
    pub declared_length: Option<u64>,
    pub body: Body,
    /// Close the connection without writing any response at all.
    pub close: bool,
}

impl Reply {
    pub fn new(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            declared_length: None,
            body: Body::Empty,
            close: false,
        }
    }

    pub fn ok(body: Vec<u8>) -> Reply {
        let mut r = Reply::new(200);
        r.body = Body::Bytes(body);
        r
    }

    pub fn partial(body: Vec<u8>, start: u64, total: u64) -> Reply {
        let mut r = Reply::new(206);
        r.declared_length = Some(body.len() as u64);
        r.headers.push((
            "Content-Range".into(),
            format!("bytes {start}-{}/{total}", start + body.len() as u64 - 1),
        ));
        r.body = Body::Bytes(body);
        r
    }

    pub fn close() -> Reply {
        Reply {
            status: 0,
            headers: Vec::new(),
            declared_length: None,
            body: Body::Empty,
            close: true,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

pub struct Server {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start<F>(handler: F) -> Server
    where
        F: Fn(Request, usize) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let join = {
            let stop = stop.clone();
            let hits = hits.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let handler = handler.clone();
                            let hits = hits.clone();
                            std::thread::spawn(move || {
                                if let Some(req) = read_request(&mut stream) {
                                    let n = hits.fetch_add(1, Ordering::SeqCst);
                                    write_reply(stream, handler(req, n));
                                }
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Server {
            addr,
            stop,
            hits,
            join: Some(join),
        }
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("http://{}{}", self.addr, path)
        } else {
            format!("http://{}/{}", self.addr, path)
        }
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some(Request {
        method,
        target,
        headers,
    })
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_reply(mut stream: TcpStream, reply: Reply) {
    if reply.close {
        return;
    }
    let chunked = matches!(reply.body, Body::Chunked(_));
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    head.push_str("Connection: close\r\n");
    for (k, v) in &reply.headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    if chunked {
        if !has_header(&reply.headers, "Transfer-Encoding") {
            head.push_str("Transfer-Encoding: chunked\r\n");
        }
    } else if !has_header(&reply.headers, "Content-Length") {
        let len = reply.declared_length.unwrap_or(match &reply.body {
            Body::Bytes(b) => b.len() as u64,
            Body::Empty => 0,
            Body::Stalled { bytes, .. } => bytes.len() as u64,
            Body::Chunked(_) => 0,
        });
        head.push_str(&format!("Content-Length: {len}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    match reply.body {
        Body::Bytes(bytes) => {
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
        }
        Body::Stalled { bytes, sleep } => {
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            std::thread::sleep(sleep);
        }
        Body::Chunked(chunks) => {
            for (bytes, delay) in chunks {
                let header = format!("{:x}\r\n", bytes.len());
                if stream.write_all(header.as_bytes()).is_err() {
                    return;
                }
                if stream.write_all(&bytes).is_err() || stream.write_all(b"\r\n").is_err() {
                    return;
                }
                let _ = stream.flush();
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
        }
        Body::Empty => {}
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

pub fn wait_for_file(path: &Path, limit: Duration) {
    wait_until(&format!("file {}", path.display()), limit, || {
        path.is_file()
    });
}

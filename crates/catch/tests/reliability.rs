// End-to-end reliability suite: real `catch` processes, isolated state and
// download folders, and a controlled local HTTP server. Everything here runs
// offline; no test touches the user's real home or downloads.
mod support;

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{
    native_frame, read_native_frame, row, run, run_ok, video_row, wait_until, Body, Reply, Sandbox,
    Server,
};

fn dir(sb: &Sandbox) -> String {
    sb.base().to_string_lossy().into_owned()
}

fn patterned(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

fn add(sb: &Sandbox, url: &str) -> std::process::Output {
    let mut cmd = sb.cmd();
    cmd.args(["add", url, "--dir", &dir(sb)]);
    run(cmd, Duration::from_secs(20))
}

fn run_args(sb: &Sandbox, args: &[&str], limit: Duration) -> std::process::Output {
    let mut cmd = sb.cmd();
    cmd.args(args);
    run_ok(cmd, limit)
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn header_connection_failure_then_retry_succeeds() {
    let sb = Sandbox::new("header-retry");
    let body = patterned(4096, 3);
    let serve = body.clone();
    let server = Server::start(move |_req, n| {
        if n == 0 {
            Reply::close()
        } else {
            Reply::ok(serve.clone())
        }
    });
    let url = server.url("file.bin");
    let out = add(&sb, &url);
    assert!(
        out.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(sb.base().join("file.bin")).unwrap(),
        body,
        "full body after the retry"
    );
    let row = sb.row_for_url(&url).expect("row");
    assert_eq!(row["status"], "done");
    assert_eq!(row["pid"], 0);
    assert_eq!(server.hits(), 2, "one failed connection, one success");
}

#[test]
fn stalled_body_times_out_and_resumes_with_range() {
    let sb = Sandbox::new("stall");
    let body = patterned(5000, 7);
    let serve = body.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let server = Server::start(move |req, n| {
        record.lock().unwrap().push(req.clone());
        if n == 0 {
            let mut r = Reply::new(200);
            r.declared_length = Some(serve.len() as u64);
            r.body = Body::Stalled {
                bytes: serve[..100].to_vec(),
                sleep: Duration::from_secs(4),
            };
            r
        } else {
            let start = req.range_start().unwrap_or(0) as usize;
            Reply::partial(serve[start..].to_vec(), start as u64, serve.len() as u64)
        }
    });
    let url = server.url("file.bin");
    let mut cmd = sb.cmd();
    cmd.env("CATCH_STALL_SECS", "1")
        .args(["add", &url, "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "stalled transfer did not recover: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 2);
    let requests = seen.lock().unwrap();
    assert_eq!(
        requests[1].range_start(),
        Some(100),
        "resume must ask for the bytes already on disk: {requests:?}"
    );
}

#[test]
fn malformed_content_range_restarts_the_attempt() {
    let sb = Sandbox::new("bad-range");
    let body = patterned(5000, 11);
    sb.seed(vec![row(
        "bad-range",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("bad-range", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &body[..100]).unwrap();

    let serve = body.clone();
    let server = Server::start(move |req, n| {
        if n == 0 {
            // A 206 that claims to start at zero although 100 bytes are
            // already held. It must be refused, not appended.
            Reply::new(206)
                .header("Content-Range", "bytes 0-99/5000")
                .header("Content-Length", "100")
        } else {
            let start = req.range_start().unwrap_or(0) as usize;
            Reply::partial(serve[start..].to_vec(), start as u64, serve.len() as u64)
        }
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    state["downloads"][0]["bytes"] = json!(100);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "bad-range", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "malformed 206 was not recovered: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 2);
}

#[test]
fn server_ignoring_range_restarts_instead_of_appending() {
    let sb = Sandbox::new("ignore-range");
    let body = patterned(5000, 21);
    sb.seed(vec![row(
        "ignore-range",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("ignore-range", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &body[..100]).unwrap();

    let serve = body.clone();
    let server = Server::start(move |_req, _n| Reply::ok(serve.clone()));
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    state["downloads"][0]["bytes"] = json!(100);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "ignore-range", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "200 answer to a resume failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(sb.base().join("file.bin")).unwrap(),
        body,
        "the old 100 bytes must not be prefixed to the full body"
    );
}

#[test]
fn complete_part_with_416_finishes_and_is_recorded_as_saved() {
    let sb = Sandbox::new("range-416-complete");
    let body = patterned(5000, 31);
    sb.seed(vec![row(
        "done-416",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("done-416", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &body).unwrap();

    let server = Server::start(|_req, _n| Reply::new(416).header("Content-Range", "bytes */5000"));
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "done-416", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "416 with a complete part did not finish: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dest = sb.base().join("file.bin");
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!part.exists(), "part moved into place");
    assert_eq!(sb.status("done-416").as_deref(), Some("done"));

    let saved = sb.state();
    let rec = &saved["saved"][url.as_str()];
    assert_eq!(rec["path"], "file.bin", "finished-from-part was indexed");
    assert_eq!(rec["bytes"], 5000);

    // The saved index is what makes the second catch a question, not a
    // silent second copy.
    let before = server.hits();
    let mut second = sb.cmd();
    second.args(["add", &url, "--dir", &dir(&sb)]);
    let again = run(second, Duration::from_secs(10));
    assert!(again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("already"),
        "second add: {}",
        String::from_utf8_lossy(&again.stdout)
    );
    assert_eq!(
        server.hits(),
        before,
        "saved file must not be fetched again"
    );
    let asks = sb.state()["asks"].as_array().cloned().unwrap_or_default();
    assert_eq!(asks.len(), 1, "the duplicate became a question");
}

#[test]
fn incomplete_part_with_416_restarts_from_zero() {
    let sb = Sandbox::new("range-416-partial");
    let body = patterned(5000, 41);
    sb.seed(vec![row(
        "part-416",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("part-416", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &body[..400]).unwrap();

    let serve = body.clone();
    let server = Server::start(move |_req, n| {
        if n == 0 {
            Reply::new(416).header("Content-Range", "bytes */5000")
        } else {
            Reply::ok(serve.clone())
        }
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "part-416", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "incomplete 416 did not restart: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 2);
}

#[test]
fn changed_remote_file_restarts_instead_of_splicing() {
    let sb = Sandbox::new("if-range");
    let old = vec![0xAAu8; 5000];
    let new = patterned(5000, 51);
    sb.seed(vec![row(
        "if-range",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("if-range", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &old[..100]).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let serve = new.clone();
    let server = Server::start(move |req, _n| {
        record.lock().unwrap().push(req.clone());
        Reply::ok(serve.clone()).header("ETag", "\"v2\"")
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    state["downloads"][0]["bytes"] = json!(100);
    state["downloads"][0]["etag"] = json!("\"v1\"");
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "if-range", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "changed file failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(sb.base().join("file.bin")).unwrap(),
        new,
        "must not splice the old first 100 bytes onto the new representation"
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests[0].range_start(), Some(100));
    assert_eq!(
        requests[0].header("if-range"),
        Some("\"v1\""),
        "persisted ETag must travel as If-Range"
    );
}

#[test]
fn changed_etag_on_a_206_restarts_instead_of_splicing() {
    let sb = Sandbox::new("if-range-206");
    let old = vec![0xAAu8; 5000];
    let new = patterned(5000, 56);
    sb.seed(vec![row(
        "etag-206",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let part = sb.part("etag-206", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &old[..100]).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let serve = new.clone();
    let server = Server::start(move |req, n| {
        record.lock().unwrap().push(req.clone());
        if n == 0 {
            // A broken server ignores If-Range and answers a 206 whose own
            // validator says the representation changed. Appending this
            // tail to the old first 100 bytes would splice two files.
            Reply::new(206)
                .header("Content-Range", "bytes 100-4999/5000")
                .header("Content-Length", "4900")
                .header("ETag", "\"v2\"")
        } else {
            Reply::ok(serve.clone()).header("ETag", "\"v2\"")
        }
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    state["downloads"][0]["bytes"] = json!(100);
    state["downloads"][0]["etag"] = json!("\"v1\"");
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "etag-206", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "changed 206 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(sb.base().join("file.bin")).unwrap(),
        new,
        "the old prefix must not survive a changed representation"
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests[0].range_start(), Some(100));
    assert_eq!(requests[0].header("if-range"), Some("\"v1\""));
    assert_eq!(
        requests[1].range_start(),
        None,
        "after the change the transfer restarts with a whole-file request"
    );
    assert_eq!(server.hits(), 2);
}

#[test]
fn permanent_status_is_not_retried() {
    let sb = Sandbox::new("permanent");
    let seen = Arc::new(Mutex::new(0usize));
    let count = seen.clone();
    let server = Server::start(move |_req, _n| {
        *count.lock().unwrap() += 1;
        Reply::new(404)
    });
    sb.seed(vec![row(
        "gone",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.env("CATCH_MAX_ATTEMPTS", "4")
        .args(["download", "gone", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(!out.status.success(), "404 must fail");
    assert_eq!(*seen.lock().unwrap(), 1, "a permanent 404 is tried once");
    assert_eq!(sb.status("gone").as_deref(), Some("error"));
    assert!(
        sb.row("gone").unwrap()["error"]
            .as_str()
            .unwrap_or("")
            .contains("404"),
        "the error should name the status"
    );
    assert!(!sb.base().join("file.bin").exists());
}

#[test]
fn retry_after_delays_the_next_attempt() {
    let sb = Sandbox::new("retry-after");
    let body = patterned(2000, 57);
    let serve = body.clone();
    let server = Server::start(move |_req, n| {
        if n == 0 {
            Reply::new(429).header("Retry-After", "1")
        } else {
            Reply::ok(serve.clone())
        }
    });
    sb.seed(vec![row(
        "back-off",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.env("CATCH_MAX_ATTEMPTS", "3")
        .args(["download", "back-off", "--dir", &dir(&sb)]);
    let started = Instant::now();
    let out = run(cmd, Duration::from_secs(20));
    let elapsed = started.elapsed();
    assert!(
        out.status.success(),
        "429 then success failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "Retry-After: 1 must be honored, elapsed {elapsed:?}"
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 2);
}

#[test]
fn truncated_body_is_not_reported_as_done() {
    let sb = Sandbox::new("truncated");
    let body = patterned(5000, 61);
    sb.seed(vec![row(
        "trunc",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let serve = body.clone();
    let server = Server::start(move |_req, _n| {
        let mut r = Reply::new(200);
        r.declared_length = Some(serve.len() as u64);
        r.body = Body::Stalled {
            bytes: serve[..400].to_vec(),
            sleep: Duration::ZERO,
        };
        r
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.env("CATCH_MAX_ATTEMPTS", "2")
        .args(["download", "trunc", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(!out.status.success(), "a short body must fail the transfer");
    assert_eq!(sb.status("trunc").as_deref(), Some("error"));
    assert!(!sb.base().join("file.bin").exists());
    assert_eq!(
        std::fs::metadata(sb.part("trunc", "file.bin"))
            .unwrap()
            .len(),
        400,
        "partial bytes stay for a manual resume"
    );
    assert_eq!(server.hits(), 2);
}

#[test]
fn slow_chunked_body_shows_progress_before_it_finishes() {
    let sb = Sandbox::new("chunked-progress");
    let body = patterned(4000, 71);
    sb.seed(vec![row(
        "chunk",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let serve = body.clone();
    let server = Server::start(move |_req, _n| {
        let mut r = Reply::new(200);
        r.body = Body::Chunked(vec![
            (serve[..100].to_vec(), Duration::ZERO),
            (serve[100..].to_vec(), Duration::from_millis(1200)),
        ]);
        r
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "chunk", "--dir", &dir(&sb)]);
    let child = std::thread::spawn(move || run(cmd, Duration::from_secs(20)));
    wait_until(
        "live progress on a slow chunked body",
        Duration::from_secs(5),
        || {
            sb.row("chunk")
                .map(|d| d["bytes"].as_u64().unwrap_or(0) > 0 && d["status"] == "downloading")
                .unwrap_or(false)
        },
    );
    let out = child.join().expect("download thread");
    assert!(
        out.status.success(),
        "chunked transfer failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 1);
}

#[test]
fn concurrent_downloads_of_one_entry_start_only_one_worker() {
    let sb = Sandbox::new("claim");
    let body = patterned(5000, 81);
    sb.seed(vec![row(
        "claim",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let serve = body.clone();
    let server = Server::start(move |_req, _n| {
        let mut r = Reply::new(200);
        r.body = Body::Chunked(vec![
            (serve[..10].to_vec(), Duration::ZERO),
            (serve[10..].to_vec(), Duration::from_millis(1200)),
        ]);
        r
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut c1 = sb.cmd();
    c1.args(["download", "claim", "--dir", &dir(&sb)]);
    let mut c2 = sb.cmd();
    c2.args(["download", "claim", "--dir", &dir(&sb)]);
    let (o1, o2) = std::thread::scope(|s| {
        let h1 = s.spawn(|| run(c1, Duration::from_secs(20)));
        let h2 = s.spawn(|| run(c2, Duration::from_secs(20)));
        (h1.join().unwrap(), h2.join().unwrap())
    });
    let ok = [&o1, &o2].iter().filter(|o| o.status.success()).count();
    assert_eq!(ok, 1, "exactly one worker may win:\n{o1:?}\n{o2:?}");
    let loser = if o1.status.success() { &o2 } else { &o1 };
    assert!(
        String::from_utf8_lossy(&loser.stderr).contains("already being downloaded"),
        "loser said: {}",
        String::from_utf8_lossy(&loser.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert_eq!(server.hits(), 1, "the loser must not open a second body");
}

#[test]
fn vanished_or_foreign_worker_pid_is_reaped() {
    let sb = Sandbox::new("reap");
    let body = patterned(2000, 91);
    let serve = body.clone();
    let server = Server::start(move |_req, _n| Reply::ok(serve.clone()));
    let url1 = server.url("one.bin");
    let url2 = server.url("two.bin");
    let mut a = row("vanish", &url1, "one.bin", "downloading");
    a["pid"] = json!(u32::MAX - 1);
    a["claimed_at"] = json!(0);
    let mut b = row("foreign", &url2, "two.bin", "downloading");
    b["pid"] = json!(1);
    b["claimed_at"] = json!(0);
    sb.seed(vec![a, b]);

    let mut cmd = sb.cmd();
    cmd.args(["drain", "--max", "1"]);
    run_ok(cmd, Duration::from_secs(10));
    wait_until(
        "both orphaned rows to finish",
        Duration::from_secs(20),
        || {
            sb.status("vanish").as_deref() == Some("done")
                && sb.status("foreign").as_deref() == Some("done")
        },
    );
    assert_eq!(std::fs::read(sb.base().join("one.bin")).unwrap(), body);
    assert_eq!(std::fs::read(sb.base().join("two.bin")).unwrap(), body);
}

#[test]
fn simultaneous_enqueues_never_lose_rows() {
    let sb = Sandbox::new("simultaneous");
    const N: usize = 24;
    let outputs = std::thread::scope(|s| {
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let mut cmd = sb.cmd();
                cmd.env("CATCH_MAX_ATTEMPTS", "1")
                    .env("CATCH_RESPONSE_SECS", "1");
                cmd.args([
                    "enqueue",
                    &format!("http://127.0.0.1:9/simultaneous-{i}.bin"),
                ]);
                s.spawn(move || run(cmd, Duration::from_secs(30)))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    for out in &outputs {
        assert!(
            out.status.success(),
            "enqueue failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let rows = sb.downloads();
    assert_eq!(rows.len(), N, "all rows survived concurrent writes");
    let mut ids: Vec<&str> = rows.iter().filter_map(|d| d["id"].as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), N, "ids are unique");
    let mut urls: Vec<&str> = rows.iter().filter_map(|d| d["url"].as_str()).collect();
    urls.sort_unstable();
    urls.dedup();
    assert_eq!(urls.len(), N, "urls are unique");
    let _ = std::fs::remove_file(sb.state_path());
}

#[test]
fn enqueue_batch_queues_every_link() {
    let sb = Sandbox::new("enqueue-batch");
    let urls = [
        "http://127.0.0.1:9/batch-a.bin",
        "http://127.0.0.1:9/batch-b.bin",
        "http://127.0.0.1:9/batch-c.bin",
    ];
    let mut cmd = sb.cmd();
    cmd.env("CATCH_MAX_ATTEMPTS", "1")
        .env("CATCH_RESPONSE_SECS", "1")
        .arg("enqueue")
        .args(urls);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "enqueue failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = sb.downloads();
    let queued: Vec<&str> = rows.iter().filter_map(|d| d["url"].as_str()).collect();
    assert_eq!(queued.len(), urls.len(), "one row per link");
    for url in urls {
        assert!(queued.contains(&url), "missing {url} in {queued:?}");
    }
    let ids: Vec<&str> = rows.iter().filter_map(|d| d["id"].as_str()).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "ids are unique");
    let _ = std::fs::remove_file(sb.state_path());
}

#[test]
fn enqueue_batch_keeps_the_good_links_when_one_is_bad() {
    let sb = Sandbox::new("enqueue-batch-bad");
    let good = "http://127.0.0.1:9/batch-good.bin";
    let mut cmd = sb.cmd();
    cmd.env("CATCH_MAX_ATTEMPTS", "1")
        .env("CATCH_RESPONSE_SECS", "1")
        .args(["enqueue", "not-a-url", good]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "the good link must still queue: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = sb.downloads();
    assert_eq!(rows.len(), 1, "only the valid link queues");
    assert_eq!(rows[0]["url"].as_str(), Some(good));

    // A lone bad URL is still an error, not a silent no-op.
    let mut one = sb.cmd();
    one.args(["enqueue", "not-a-url"]);
    let bad = run(one, Duration::from_secs(10));
    assert!(!bad.status.success(), "a single bad URL must fail");

    // A batch made only of bad links fails too: nothing was accepted.
    let mut all = sb.cmd();
    all.args(["enqueue", "not-a-url", "also-not-a-url"]);
    let none = run(all, Duration::from_secs(10));
    assert!(!none.status.success(), "an all-bad batch must fail");
    let _ = std::fs::remove_file(sb.state_path());
}

#[test]
fn malformed_state_is_quarantined_and_the_cli_still_works() {
    let sb = Sandbox::new("bad-state");
    std::fs::create_dir_all(sb.state_path().parent().unwrap()).unwrap();
    std::fs::write(sb.state_path(), b"{ this is not json").unwrap();

    let out = run_args(&sb, &["list"], Duration::from_secs(10));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unreadable"),
        "no recovery notice: {stderr}"
    );
    let bad: Vec<_> = std::fs::read_dir(sb.state_path().parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("state.json.bad-")
        })
        .collect();
    assert_eq!(bad.len(), 1, "the unreadable file is kept, not deleted");

    // A second run must not trip over the quarantine itself.
    run_args(&sb, &["list"], Duration::from_secs(10));
    run_args(
        &sb,
        &["enqueue", "http://127.0.0.1:9/recovered.bin"],
        Duration::from_secs(10),
    );
    assert!(sb.row_for_url("http://127.0.0.1:9/recovered.bin").is_some());
    let _ = std::fs::remove_file(sb.state_path());
}

/// State carries full download URLs (signed query strings) and the saved
/// history, and its parent directory is traversable by other local users.
/// An install built before the fix left the directory at 0755 and every file
/// at 0644; the next command must heal it, including old quarantine copies,
/// so no other account can read the links or the browsing history.
#[test]
fn state_and_history_files_are_owner_only() {
    let sb = Sandbox::new("state-perms");
    let dir = sb.state_path().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in [
        ("state.json", "{}"),
        ("state.json.bad-1", "{}"),
        ("connections.json", "{}"),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        chmod(&path, 0o644);
    }
    chmod(&dir, 0o755);

    run_args(
        &sb,
        &["enqueue", "http://127.0.0.1:9/private.bin"],
        Duration::from_secs(10),
    );

    assert_eq!(mode(&dir), 0o700, "state directory");
    assert_eq!(mode(&sb.state_path()), 0o600, "state.json");
    assert_eq!(
        mode(&dir.join("state.json.bad-1")),
        0o600,
        "quarantined copy"
    );
    assert_eq!(
        mode(&dir.join("connections.json")),
        0o600,
        "connections registry"
    );
    assert!(
        !dir.join("state.json.tmp").exists(),
        "the private temp is renamed away, never left behind"
    );
}

/// A signed link's token must never sit in a child's argv: /proc/<pid>/cmdline
/// is world-readable, and `replace`/`add` keep that child alive for the whole
/// transfer. `-` reads the URL from stdin instead, and a batch arrives as one
/// whitespace-separated line so a browser hand-off is still one enqueue call.
#[test]
fn urls_arrive_on_stdin_instead_of_argv() {
    let sb = Sandbox::new("stdin-urls");
    let mut cmd = sb.cmd();
    cmd.args(["enqueue", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"http://127.0.0.1:9/one.bin http://127.0.0.1:9/two.bin\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sb.row_for_url("http://127.0.0.1:9/one.bin").is_some());
    assert!(sb.row_for_url("http://127.0.0.1:9/two.bin").is_some());
}

#[test]
fn cancel_keeps_the_part_and_a_resume_finishes_byte_exact() {
    let sb = Sandbox::new("cancel-io");
    let body = patterned(5000, 101);
    sb.seed(vec![row(
        "cancel",
        "http://placeholder/file.bin",
        "file.bin",
        "queued",
    )]);
    let serve = body.clone();
    let server = Server::start(move |req, _n| {
        if let Some(start) = req.range_start() {
            // The resume asks for the bytes already on disk.
            let start = (start as usize).min(serve.len());
            Reply::partial(serve[start..].to_vec(), start as u64, serve.len() as u64)
        } else {
            let mut r = Reply::new(200);
            // The chunk format delays *after* each chunk, so stall on the
            // first one: 100 bytes, a long gap (the cancel lands here), then
            // the rest.
            r.body = Body::Chunked(vec![
                (serve[..100].to_vec(), Duration::from_secs(5)),
                (serve[100..].to_vec(), Duration::ZERO),
            ]);
            r
        }
    });
    let url = server.url("file.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "cancel", "--dir", &dir(&sb)]);
    let child = std::thread::spawn(move || run(cmd, Duration::from_secs(20)));
    wait_until(
        "the transfer to start moving bytes",
        Duration::from_secs(5),
        || {
            sb.row("cancel")
                .map(|d| d["bytes"].as_u64().unwrap_or(0) > 0)
                .unwrap_or(false)
        },
    );
    run_args(&sb, &["cancel", "cancel"], Duration::from_secs(10));
    let out = child.join().expect("download thread");
    assert!(!out.status.success(), "the worker must stop on cancel");
    assert_eq!(sb.status("cancel").as_deref(), Some("cancelled"));

    // Stop is not remove: the partial file and its progress survive so the
    // same URL can be resumed (or refreshed) later — only `remove` deletes.
    let part = sb.part("cancel", "file.bin");
    assert!(part.is_file(), "stop must keep the partial data");
    assert!(part.metadata().unwrap().len() > 0);
    assert!(!sb.base().join("file.bin").exists());

    // Resume against the same link: `resume` re-queues the row and wakes the
    // drainer, which asks for the bytes already on disk and finishes
    // byte-for-byte.
    run_args(&sb, &["resume", "cancel"], Duration::from_secs(10));
    wait_until(
        "the resumed transfer to finish",
        Duration::from_secs(20),
        || sb.status("cancel").as_deref() == Some("done"),
    );
    assert_eq!(
        sb.status("cancel").as_deref(),
        Some("done"),
        "the background drainer must finish the resume"
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert!(!part.exists(), "the part is gone once published");
}

#[test]
fn content_disposition_cannot_escape_the_download_folder() {
    let sb = Sandbox::new("traversal");
    let server = Server::start(|req, _n| {
        if req.target.contains("get") {
            Reply::ok(b"zip".to_vec()).header(
                "Content-Disposition",
                "attachment; filename*=UTF-8''..%2F..%2Fevil%2Ezip",
            )
        } else {
            Reply::ok(b"text".to_vec()).header(
                "Content-Disposition",
                "attachment; filename=\"../../outside.txt\"",
            )
        }
    });
    let u1 = server.url("/download?id=1");
    let u2 = server.url("/get?id=2");
    for url in [&u1, &u2] {
        let mut cmd = sb.cmd();
        cmd.args(["add", url, "--dir", &dir(&sb)]);
        run_ok(cmd, Duration::from_secs(15));
    }
    assert_eq!(
        std::fs::read(sb.base().join("outside.txt")).unwrap(),
        b"text"
    );
    assert_eq!(std::fs::read(sb.base().join("evil.zip")).unwrap(), b"zip");
    for escape in [
        sb.root.join("outside.txt"),
        sb.home.join("outside.txt"),
        sb.root.join("evil.zip"),
    ] {
        assert!(!escape.exists(), "escaped to {}", escape.display());
    }
}

#[test]
fn non_ascii_names_truncate_on_a_character_boundary() {
    let sb = Sandbox::new("cjk-name");
    let name = format!("{}.zip", "汉".repeat(60));
    let url = format!("http://127.0.0.1:9/{name}");
    run_args(&sb, &["enqueue", &url], Duration::from_secs(10));
    let row = sb.row_for_url(&url).expect("row");
    let filename = row["filename"].as_str().unwrap();
    assert!(filename.len() <= 128, "too long: {} bytes", filename.len());
    assert!(filename.ends_with(".zip"), "extension lost: {filename}");
    assert!(filename.chars().all(|c| !c.is_control()));
    assert_eq!(
        filename,
        String::from_utf8_lossy(filename.as_bytes()),
        "valid UTF-8"
    );
    let _ = std::fs::remove_file(sb.state_path());
}

const FAKE_YTDLP: &str = r#"#!/bin/bash
set -u
out="${FAKE_OUT:?}"
# More stderr than a pipe buffer: reading it only after exit used to deadlock.
for _ in $(seq 1 220); do
  head -c 1000 /dev/zero | tr '\0' e >&2
  printf '\n' >&2
done
# One oversized stdout line (a hostile page title), then real protocol lines.
head -c 100000 /dev/zero | tr '\0' x
printf '\n'
printf 'CATCH_PROG\t50\t100\n'
printf 'CATCH_META\t"Fake title"\t100\n'
mkdir -p "$(dirname -- "$out")"
printf 'video-bytes' >"$out"
printf 'CATCH_FILE\t%s\n' "$out"
if [ -n "${FAKE_SLEEP:-}" ]; then
  sleep "$FAKE_SLEEP" &
  child=$!
  printf '%s\n' "$child" >"${FAKE_CHILD_PID:?}"
  wait "$child"
fi
exit 0
"#;

#[test]
fn video_survives_large_stdout_and_stderr() {
    let sb = Sandbox::new("video-output");
    sb.fake_ytdlp(FAKE_YTDLP);
    sb.seed(vec![video_row("vid", "http://127.0.0.1:9/watch")]);
    let out = sb.base().join("clip.mp4");
    let mut cmd = sb.cmd();
    cmd.env("FAKE_OUT", &out)
        .args(["video", "vid", "--dir", &dir(&sb)]);
    let result = run(cmd, Duration::from_secs(20));
    assert!(
        result.status.success(),
        "video download failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"video-bytes");
    let row = sb.row("vid").expect("row");
    assert_eq!(row["status"], "done");
    assert_eq!(row["title"], "Fake title");
    assert_eq!(row["pid"], 0);
}

#[test]
fn cancel_during_video_post_processing_kills_the_group() {
    let sb = Sandbox::new("video-cancel");
    sb.fake_ytdlp(FAKE_YTDLP);
    sb.seed(vec![video_row("vid-cancel", "http://127.0.0.1:9/watch")]);
    let out = sb.base().join("clip.mp4");
    let child_pid_file = sb.root.join("child.pid");
    let mut cmd = sb.cmd();
    cmd.env("FAKE_OUT", &out)
        .env("FAKE_SLEEP", "30")
        .env("FAKE_CHILD_PID", &child_pid_file)
        .args(["video", "vid-cancel", "--dir", &dir(&sb)]);
    let child = std::thread::spawn(move || run(cmd, Duration::from_secs(30)));
    wait_until(
        "the fake post-processing child",
        Duration::from_secs(10),
        || child_pid_file.is_file(),
    );
    let helper: u32 = std::fs::read_to_string(&child_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    run_args(&sb, &["cancel", "vid-cancel"], Duration::from_secs(10));
    let result = child.join().expect("video thread");
    assert!(
        !result.status.success(),
        "cancelled video must not report success"
    );
    assert_eq!(sb.status("vid-cancel").as_deref(), Some("cancelled"));
    wait_until(
        "the helper process group member to die",
        Duration::from_secs(5),
        || !Path::new(&format!("/proc/{helper}")).exists(),
    );
}

#[test]
fn pause_during_video_keeps_the_row_paused_not_errored() {
    let sb = Sandbox::new("video-pause");
    sb.fake_ytdlp(FAKE_YTDLP);
    sb.seed(vec![video_row("vid-pause", "http://127.0.0.1:9/watch")]);
    let out = sb.base().join("clip.mp4");
    let child_pid_file = sb.root.join("child.pid");
    let mut cmd = sb.cmd();
    cmd.env("FAKE_OUT", &out)
        .env("FAKE_SLEEP", "30")
        .env("FAKE_CHILD_PID", &child_pid_file)
        .args(["video", "vid-pause", "--dir", &dir(&sb)]);
    let child = std::thread::spawn(move || run(cmd, Duration::from_secs(30)));
    wait_until(
        "the fake post-processing child",
        Duration::from_secs(10),
        || child_pid_file.is_file(),
    );
    run_args(&sb, &["pause", "vid-pause"], Duration::from_secs(10));
    let result = child.join().expect("video thread");
    assert!(
        result.status.success(),
        "a pause is not a failure: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        sb.status("vid-pause").as_deref(),
        Some("paused"),
        "pause must survive the worker's exit"
    );
}

#[test]
fn native_host_accepts_a_large_batch_in_one_write() {
    let sb = Sandbox::new("batch");
    const N: usize = 2500;
    let urls: Vec<Value> = (0..N)
        .map(|i| Value::String(format!("http://127.0.0.1:9/batch-{i:04}.bin")))
        .collect();
    let frame = native_frame(&json!({ "req": 7, "urls": urls }));

    let mut child = sb
        .host_cmd()
        .env("CATCH_HOST_SINGLE_SHOT", "1")
        .env("CATCH_MAX_ATTEMPTS", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn catch-host");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&frame)
        .expect("write batch frame");
    let mut stdout = child.stdout.take().unwrap();
    let reply = read_native_frame(&mut stdout);
    wait_until("the native host to exit", Duration::from_secs(20), || {
        child.try_wait().unwrap().is_some()
    });
    assert_eq!(reply["req"], 7);
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["accepted"].as_u64(), Some(N as u64));
    assert_eq!(reply["results"].as_array().unwrap().len(), N);

    let rows = sb.downloads();
    assert_eq!(rows.len(), N, "one state write held the whole batch");
    let mut ids: Vec<&str> = rows.iter().filter_map(|d| d["id"].as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), N, "batch ids are unique");
    // Stop the detached queue before the process churns through 2500
    // connection-refused workers. A missing state file is an empty queue.
    let _ = std::fs::remove_file(sb.state_path());
}

#[test]
fn native_host_sanitizes_browser_filename_hints() {
    let sb = Sandbox::new("hint-traversal");
    let frame = native_frame(&json!({
        "req": 11,
        "urls": [
            {"url": "http://127.0.0.1:9/one.bin", "filenameHint": "../../escape.txt"},
            {"url": "http://127.0.0.1:9/two.bin", "filenameHint": "..\\..\\windows.txt"},
            {"url": "http://127.0.0.1:9/three.bin", "filename_hint": "/etc/passwd"}
        ]
    }));
    let mut child = sb
        .host_cmd()
        .env("CATCH_HOST_SINGLE_SHOT", "1")
        .env("CATCH_MAX_ATTEMPTS", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn catch-host");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&frame)
        .expect("write hint frame");
    let mut stdout = child.stdout.take().unwrap();
    let reply = read_native_frame(&mut stdout);
    wait_until("the native host to exit", Duration::from_secs(20), || {
        child.try_wait().unwrap().is_some()
    });
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["accepted"].as_u64(), Some(3));

    let rows = sb.downloads();
    let mut names: Vec<String> = rows
        .iter()
        .filter_map(|d| d["filename"].as_str().map(str::to_string))
        .collect();
    names.sort();
    assert_eq!(names, vec!["escape.txt", "passwd", "windows.txt"]);
    assert!(
        names.iter().all(|n| !n.contains("..") && !n.contains('/')),
        "hints must not survive as paths: {names:?}"
    );
    let _ = std::fs::remove_file(sb.state_path());
}

#[test]
fn native_host_survives_a_malformed_message() {
    let sb = Sandbox::new("bad-frame");
    let mut child = sb
        .host_cmd()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn catch-host");
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&native_frame(&Value::String("not a request".into())))
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let first = read_native_frame(&mut stdout);
    assert_eq!(first["ok"], false);
    assert!(
        first["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid native message"),
        "reply: {first}"
    );
    stdin
        .write_all(&native_frame(&json!({"action":"ping","req":2})))
        .unwrap();
    let second = read_native_frame(&mut stdout);
    assert_eq!(second["ok"], true);
    assert_eq!(second["req"], 2);
    drop(stdin);
    wait_until("the native host to exit", Duration::from_secs(10), || {
        child.try_wait().unwrap().is_some()
    });
}

#[test]
fn a_dead_drainer_marker_is_reclaimed_but_a_live_one_is_respected() {
    let sb = Sandbox::new("drain-marker");
    let body = patterned(2048, 9);
    let serve = body.clone();
    let server = Server::start(move |_req, _n| Reply::ok(serve.clone()));
    let url = server.url("drain.bin");
    let state_dir = sb.state_home.join("omarchy/catch");
    std::fs::create_dir_all(&state_dir).unwrap();
    let marker = state_dir.join("drain.lock");

    // A live holder in another process keeps this drainer out. The test
    // process stands in for it: its pid is unquestionably alive.
    sb.seed(vec![row("drain-one", &url, "drain.bin", "queued")]);
    std::fs::write(&marker, format!("{}\n0\n", std::process::id())).unwrap();
    run_args(&sb, &["drain", "--max", "1"], Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        sb.status("drain-one").as_deref(),
        Some("queued"),
        "a live drainer marker must not be stolen"
    );
    assert_eq!(server.hits(), 0);

    // A killed drainer used to block every later drain for two minutes. A
    // marker naming a dead pid is now reclaimed immediately.
    std::fs::write(&marker, "999999999\n0\n").unwrap();
    run_args(&sb, &["drain", "--max", "1"], Duration::from_secs(5));
    wait_until(
        "the reclaimed drainer to finish",
        Duration::from_secs(20),
        || sb.status("drain-one").as_deref() == Some("done"),
    );
    assert_eq!(std::fs::read(sb.base().join("drain.bin")).unwrap(), body);
    assert_eq!(server.hits(), 1);
}

#[test]
fn version_never_runs_self_configure_and_configure_can_suppress_it() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new("self-configure");
    let plugin = sb.root.join("plugin");
    let dist = plugin.join("bin/dist");
    std::fs::create_dir_all(&dist).unwrap();
    let copied = dist.join("catch");
    std::fs::copy(env!("CARGO_BIN_EXE_catch"), &copied).unwrap();
    let configure = plugin.join("bin/configure");
    std::fs::write(
        &configure,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$HOME/configure-ran\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&configure).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&configure, perms).unwrap();
    let marker = sb.home.join("configure-ran");

    // `--version` is what bin/configure calls to check the shipped build. It
    // must answer and exit without trying to configure anything.
    let mut cmd = sb.cmd_for(&copied);
    cmd.arg("--version");
    let out = run_ok(cmd, Duration::from_secs(5));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")),
        "--version prints the package version"
    );
    assert!(
        !marker.exists(),
        "--version must not touch links or host manifests"
    );

    // bin/configure exports this guard before asking its children anything:
    // otherwise the missing links it is creating make every child recursive.
    let mut cmd = sb.cmd_for(&copied);
    cmd.env("CATCH_NO_SELF_CONFIGURE", "1").arg("status");
    let out = run_ok(cmd, Duration::from_secs(5));
    assert_eq!(out.status.code(), Some(0));
    assert!(
        !marker.exists(),
        "the configure guard must reach every child"
    );

    // Sanity check that the hook is still wired: an unguarded run from the
    // shipped layout does invoke configure, exactly once.
    let mut cmd = sb.cmd_for(&copied);
    cmd.arg("status");
    run_ok(cmd, Duration::from_secs(10));
    let runs = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(runs.lines().count(), 1, "configure ran {runs:?}");
}

#[test]
fn unknown_total_content_range_falls_back_to_full_download() {
    let sb = Sandbox::new("unknown-total");
    let body = patterned(5000, 7);
    let serve = body.clone();
    let hits = Arc::new(Mutex::new(0usize));
    let count = hits.clone();
    let server = Server::start(move |_req, n| {
        *count.lock().unwrap() += 1;
        if n == 0 {
            let mut r = Reply::new(206);
            r.headers
                .push(("Content-Range".into(), "bytes 0-99/*".into()));
            r.headers.push(("Content-Length".into(), "100".into()));
            r.body = Body::Bytes(serve[..100].to_vec());
            r
        } else {
            Reply::ok(serve.clone())
        }
    });
    sb.seed(vec![row(
        "unk",
        "http://placeholder/f.bin",
        "f.bin",
        "queued",
    )]);
    let url = server.url("f.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "unk", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "unknown-total did not recover: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("f.bin")).unwrap(), body);
    assert!(
        *hits.lock().unwrap() >= 2,
        "must fall back to a full download, not stop at 100 bytes"
    );
}

#[test]
fn larger_representation_without_validators_restarts() {
    let sb = Sandbox::new("larger-repr");
    let old = vec![0xAAu8; 100];
    let new = patterned(7000, 91);
    sb.seed(vec![row(
        "big",
        "http://placeholder/f.bin",
        "f.bin",
        "queued",
    )]);
    let part = sb.part("big", "f.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &old).unwrap();

    let serve = new.clone();
    let server = Server::start(move |_req, _n| Reply::ok(serve.clone()));
    let url = server.url("f.bin");
    let mut state = sb.state();
    state["downloads"][0]["url"] = json!(url);
    state["downloads"][0]["bytes"] = json!(100);
    // no etag / last_modified (validators absent), total unknown
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["download", "big", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "larger representation failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("f.bin")).unwrap(), new);
}

#[test]
fn rename_cannot_escape_download_dir() {
    let sb = Sandbox::new("rename-traversal");
    let outside = sb.root.join("outside.txt");
    std::fs::write(&outside, b"secret").unwrap();

    let url = "https://x.test/a";
    let state = json!({
        "version": 1,
        "downloads": [],
        "saved": { url: { "path": "../../outside.txt", "kind": "file", "bytes": 6, "at": 1 } },
        "asks": []
    });
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["rename", url, "newname"]);
    let _ = run(cmd, Duration::from_secs(10));

    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"secret",
        "rename must not touch a file outside the download dir"
    );
}

#[test]
fn replace_cannot_delete_outside_download_dir() {
    let sb = Sandbox::new("replace-traversal");
    let outside = sb.root.join("outside.txt");
    std::fs::write(&outside, b"secret").unwrap();

    let url = "https://x.test/a";
    let state = json!({
        "version": 1,
        "downloads": [],
        "saved": { url: { "path": "../../outside.txt", "kind": "file", "bytes": 6, "at": 1 } },
        "asks": []
    });
    sb.write_state(&state);

    let mut cmd = sb.cmd();
    cmd.args(["replace", url]);
    let _ = run(cmd, Duration::from_secs(10));

    assert!(
        outside.exists(),
        "replace must not delete a file outside the download dir"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"secret");
}

// Byte-range serving shared by the segmented tests: a ranged request gets
// exactly the asked slice as a 206 with validators, anything else the full
// representation. The one-byte probe (bytes=0-0) is answered the same way.
fn range_reply(body: &[u8], req: &support::Request) -> Reply {
    let total = body.len() as u64;
    let mut reply = if let Some(raw) = req.header("range") {
        let rest = raw.strip_prefix("bytes=").unwrap_or("");
        let mut parts = rest.split('-');
        let start: u64 = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let end: u64 = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(total - 1)
            .min(total - 1);
        if start > end || start >= total {
            return Reply::new(416);
        }
        Reply::partial(body[start as usize..=end as usize].to_vec(), start, total)
    } else {
        Reply::ok(body.to_vec())
    };
    reply.headers.push(("ETag".into(), "\"seg-v1\"".into()));
    reply.headers.push(("Accept-Ranges".into(), "bytes".into()));
    reply
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    let out = sha2::Sha256::digest(bytes);
    out.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn segmented_download_is_byte_exact_across_connections() {
    let sb = Sandbox::new("seg-e2e");
    let body = patterned(300_000, 41);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let server = Server::start({
        let body = body.clone();
        move |req, _n| {
            record.lock().unwrap().push(req.clone());
            range_reply(&body, &req)
        }
    });
    let url = server.url("file.bin");
    let mut cmd = sb.cmd();
    cmd.env("CATCH_SEGMENT_MIN_BYTES", "1024")
        .env("CATCH_SEGMENT_MIN_PIECE", "256")
        .env("CATCH_SEGMENTS", "4")
        .args(["add", &url, "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "segmented add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = sb.row_for_url(&url).expect("row");
    assert_eq!(row["status"], "done");
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);

    let id = row["id"].as_str().unwrap().to_string();
    let part = sb.part(&id, "file.bin");
    assert!(!part.exists(), "the part is gone once published");
    assert!(
        !std::path::PathBuf::from(format!("{}.meta", part.display())).exists(),
        "the manifest is cleaned up with it"
    );
    let requests = seen.lock().unwrap();
    let offsets: Vec<u64> = requests
        .iter()
        .filter_map(|r| r.range_start())
        .filter(|s| *s > 0)
        .collect();
    assert!(
        !offsets.is_empty(),
        "the transfer must fan out to real segment offsets: {requests:?}"
    );
}

// The runner hands a straggler's tail to an idle slot by marking the victim's
// frontier. The victim only obeys the mark at its next poll, so a final chunk
// that completes the range can land after the mark was set and finish the
// work outright. That attempt is done — it must run the durable commit, not
// report a handoff: a handoff with nothing left looks like "finished, no
// remainder" while the segment stays unmarked, the queue drains, and the
// transfer fails with "segment N did not complete" with every byte of it on
// disk.
#[test]
fn split_mark_outrun_by_the_last_chunk_still_commits() {
    let sb = Sandbox::new("seg-split-commit");
    let body = patterned(300_000, 43);
    let server = Server::start({
        let body = body.clone();
        move |req, _n| {
            let mut reply = range_reply(&body, &req);
            let bytes = match std::mem::replace(&mut reply.body, Body::Empty) {
                Body::Bytes(b) => b,
                other => {
                    reply.body = other;
                    return reply;
                }
            };
            match req.range_start() {
                // Second half [150000, 299999]: a small first chunk, the bulk
                // of the tail, then the last 8 KiB after a pause. The pause is
                // where the idle slot marks this segment; the last piece then
                // completes the whole work, outrunning the mark.
                Some(150_000) => {
                    let cut = bytes.len() - 8192;
                    reply.declared_length = None;
                    reply.body = Body::Chunked(vec![
                        (bytes[..4096].to_vec(), Duration::from_millis(100)),
                        (bytes[4096..cut].to_vec(), Duration::from_millis(150)),
                        (bytes[cut..].to_vec(), Duration::ZERO),
                    ]);
                }
                // First half [0, 149999]: all but the last kilobyte at once,
                // then the last kilobyte 150 ms later, so this slot frees
                // between the two tail chunks above. (The capability probe
                // 0-1023 is too small to match.)
                Some(0) if bytes.len() > 4096 => {
                    let cut = bytes.len() - 1024;
                    reply.declared_length = None;
                    reply.body = Body::Chunked(vec![
                        (bytes[..cut].to_vec(), Duration::from_millis(150)),
                        (bytes[cut..].to_vec(), Duration::ZERO),
                    ]);
                }
                _ => reply.body = Body::Bytes(bytes),
            }
            reply
        }
    });
    let url = server.url("file.bin");
    let mut cmd = sb.cmd();
    cmd.env("CATCH_SEGMENT_MIN_BYTES", "1024")
        .env("CATCH_SEGMENT_MIN_PIECE", "256")
        .env("CATCH_SEGMENTS", "2")
        .env("CATCH_MAX_CONNECTIONS", "2")
        .args(["add", &url, "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "a split mark the last chunk outran left the segment uncommitted: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = sb.row_for_url(&url).expect("row");
    assert_eq!(row["status"], "done");
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    let id = row["id"].as_str().unwrap().to_string();
    let part = sb.part(&id, "file.bin");
    assert!(!part.exists(), "the part is gone once published");
    assert!(
        !std::path::PathBuf::from(format!("{}.meta", part.display())).exists(),
        "the manifest is cleaned up with it"
    );
}

#[test]
fn segmented_pause_keeps_the_manifest_and_resume_finishes() {
    let sb = Sandbox::new("seg-pause");
    let body = patterned(300_000, 77);
    let served = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&served);
    let server = Server::start({
        let body = body.clone();
        move |req, _n| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            let mut reply = range_reply(&body, &req);
            // The first handful of full-size responses arrive in two halves
            // with a long gap: long enough for a pause to land mid-transfer,
            // and only for the first run — the resumed segments are fast.
            if (1..6).contains(&n) {
                reply.body = match std::mem::replace(&mut reply.body, Body::Empty) {
                    Body::Bytes(bytes) if bytes.len() > 1000 => {
                        let half = bytes.len() / 2;
                        reply.declared_length = None;
                        // The delay comes after each chunk, so the gap must
                        // sit on the first half: bytes, pause, then the rest.
                        Body::Chunked(vec![
                            (bytes[..half].to_vec(), Duration::from_secs(4)),
                            (bytes[half..].to_vec(), Duration::ZERO),
                        ])
                    }
                    other => other,
                };
            }
            reply
        }
    });
    let url = server.url("file.bin");
    let mut cmd = sb.cmd();
    cmd.env("CATCH_SEGMENT_MIN_BYTES", "1024")
        .env("CATCH_SEGMENT_MIN_PIECE", "256")
        .env("CATCH_SEGMENTS", "4")
        .args(["add", &url, "--dir", &dir(&sb)]);
    let child = std::thread::spawn(move || run(cmd, Duration::from_secs(60)));
    wait_until(
        "the segmented transfer to show progress",
        Duration::from_secs(10),
        || {
            sb.row_for_url(&url)
                .map(|d| d["bytes"].as_u64().unwrap_or(0) > 0)
                .unwrap_or(false)
        },
    );
    let id = sb.row_for_url(&url).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    run_args(&sb, &["pause", &id], Duration::from_secs(10));
    let _ = child.join();

    assert_eq!(sb.status(&id).as_deref(), Some("paused"));
    let part = sb.part(&id, "file.bin");
    let meta = std::path::PathBuf::from(format!("{}.meta", part.display()));
    assert!(
        part.is_file() && meta.is_file(),
        "pause keeps part + manifest"
    );
    assert_eq!(
        part.metadata().unwrap().len(),
        body.len() as u64,
        "the part stays preallocated to the full length"
    );

    // Resume: the manifest drives the same segment ranges and the same
    // segment connections, and the output must be byte-for-byte correct even
    // though the first run died mid-segment. `resume` wakes the drainer.
    run_args(&sb, &["resume", &id], Duration::from_secs(10));
    wait_until(
        "the segmented resume to finish",
        Duration::from_secs(30),
        || sb.status(&id).as_deref() == Some("done"),
    );
    assert_eq!(
        sb.status(&id).as_deref(),
        Some("done"),
        "the background drainer must finish the segmented resume"
    );
    assert_eq!(std::fs::read(sb.base().join("file.bin")).unwrap(), body);
    assert!(!part.exists() && !meta.exists());
}

#[test]
fn corrupt_manifest_quarantines_the_part_and_refetches() {
    let sb = Sandbox::new("bad-manifest");
    let body = patterned(5000, 13);
    let server = Server::start({
        let body = body.clone();
        move |_req, _n| Reply::ok(body.clone())
    });
    let url = server.url("file.bin");

    // A preallocated part whose manifest is garbage. This used to read as "no
    // manifest, part holds N complete bytes" and could publish a file of
    // holes; the part must be set aside and the file fetched again.
    let part = sb.part("bad-meta", "file.bin");
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, vec![0xAAu8; body.len()]).unwrap();
    std::fs::write(format!("{}.meta", part.display()), b"{ not json").unwrap();
    sb.seed(vec![row("bad-meta", &url, "file.bin", "queued")]);

    let mut cmd = sb.cmd();
    cmd.args(["download", "bad-meta", "--dir", &dir(&sb)]);
    let out = run(cmd, Duration::from_secs(20));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "recovery failed: {stderr}");
    assert!(
        stderr.contains("corrupt manifest"),
        "no quarantine notice: {stderr}"
    );
    assert_eq!(
        std::fs::read(sb.base().join("file.bin")).unwrap(),
        body,
        "a part no manifest can vouch for is never published"
    );
    let quarantined = std::fs::read_dir(sb.base())
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("quarantined"));
    assert!(quarantined, "the suspect part is kept aside, not reused");
}

#[test]
fn a_wrong_checksum_removes_the_bytes_and_reports_it() {
    let sb = Sandbox::new("checksum");
    let good = patterned(4000, 5);
    let bad = patterned(4000, 6);
    let expected = hex_sha256(&good);
    let server = Server::start({
        let good = good.clone();
        let bad = bad.clone();
        move |req, _n| {
            if req.target.contains("bad.bin") {
                Reply::ok(bad.clone())
            } else {
                Reply::ok(good.clone())
            }
        }
    });

    let good_url = server.url("good.bin");
    let mut cmd = sb.cmd();
    cmd.args(["add", &good_url, "--dir", &dir(&sb), "--sha256", &expected]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        out.status.success(),
        "verified download failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(sb.base().join("good.bin")).unwrap(), good);
    assert_eq!(sb.row_for_url(&good_url).unwrap()["status"], "done");

    let bad_url = server.url("bad.bin");
    let mut cmd = sb.cmd();
    cmd.args(["add", &bad_url, "--dir", &dir(&sb), "--sha256", &expected]);
    let out = run(cmd, Duration::from_secs(20));
    assert!(
        !out.status.success(),
        "a checksum mismatch must not report success"
    );
    let row = sb.row_for_url(&bad_url).expect("row");
    assert_eq!(row["status"], "error");
    assert!(
        row["error"]
            .as_str()
            .unwrap_or("")
            .contains("checksum mismatch"),
        "the error names the check: {row:?}"
    );
    let id = row["id"].as_str().unwrap().to_string();
    assert!(
        !sb.base().join("bad.bin").exists(),
        "wrong bytes are never published"
    );
    assert!(
        !sb.part(&id, "bad.bin").exists(),
        "wrong bytes are removed, not resumed"
    );
}

#[test]
fn connection_budget_is_enforced_across_workers() {
    let sb = Sandbox::new("conn-budget");
    // Three transfers share a global budget of two. Each held request lasts
    // 400 ms, so the third worker can only leave after one of the first two
    // has finished — its request must arrive visibly later.
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&arrivals);
    let server = Server::start(move |_req, _n| {
        seen.lock().unwrap().push(Instant::now());
        let mut reply = Reply::new(200);
        // The bytes arrive immediately, the terminal chunk 400 ms later, so
        // the connection (and its permit) is held for that long.
        reply.body = Body::Chunked(vec![(vec![7u8; 4096], Duration::from_millis(400))]);
        reply
    });

    let mut cmds = Vec::new();
    for name in ["one.bin", "two.bin", "three.bin"] {
        let url = server.url(name);
        let mut cmd = sb.cmd();
        cmd.env("CATCH_MAX_CONNECTIONS", "2")
            .env("CATCH_SEGMENTS", "1")
            .args(["add", &url, "--dir", &dir(&sb)]);
        cmds.push(cmd);
    }
    let outs: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = cmds
            .into_iter()
            .map(|cmd| s.spawn(move || run(cmd, Duration::from_secs(30))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("add thread"))
            .collect()
    });
    for out in &outs {
        assert!(
            out.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for name in ["one.bin", "two.bin", "three.bin"] {
        assert_eq!(
            std::fs::read(sb.base().join(name)).unwrap().len(),
            4096,
            "{name} must be written completely"
        );
    }

    let arrivals = arrivals.lock().unwrap();
    assert_eq!(
        arrivals.len(),
        3,
        "one request per transfer with segments=1 (no probes): {arrivals:?}"
    );
    let spread = arrivals[2] - arrivals[0];
    assert!(
        spread >= Duration::from_millis(300),
        "the third worker must wait for a free permit, arrived after {spread:?}"
    );
}

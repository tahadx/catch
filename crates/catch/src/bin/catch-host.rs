// Stable native-messaging entry point: browsers exec this path with no
// arguments, so it cannot be `catch native-host` (manifests carry no argv).
// Re-execs the sibling `catch` binary when found beside us, else falls back
// to `catch` on PATH.
fn main() {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("catch")))
        .filter(|p| p.exists());
    let target = sibling.unwrap_or_else(|| "catch".into());
    let status = std::process::Command::new(target)
        .arg("native-host")
        .status();
    if let Err(e) = status {
        eprintln!("catch-host: failed to launch backend: {e}");
        std::process::exit(1);
    }
}

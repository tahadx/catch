fn main() {
    // Bake the commit hash (-dirty when crates/ has uncommitted changes)
    // into the binary so `catch status` always shows exactly which tree is
    // running: a stale dist binary rewriting state caused real bugs.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(["-C", "../.."])
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    // Stamp the crates/ tree id, not the commit id. Writing bin/dist into
    // the same commit that contains crates/ changes the commit id, so a
    // commit-id stamp can never match the pre-push rebuild. The tree id
    // stays put when only bin/dist is replaced.
    let mut sha = git(&["rev-parse", "HEAD:crates"]);
    if sha.is_empty() {
        sha = git(&["rev-parse", "--short", "HEAD"]);
    }
    if sha.is_empty() {
        sha = "unknown".to_string();
    }
    if !git(&["status", "--porcelain", "--", "crates"]).is_empty() {
        sha.push_str("-dirty");
    }
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rustc-env=CATCH_GIT_SHA={sha}");
}

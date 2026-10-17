use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Native host name shared by both browser families.
pub const HOST_NAME: &str = "io.github.tahadx.catch";
/// Pinned by the "key" in extensions/chromium-catch/manifest.json: the id
/// never changes, so installed manifests stay valid across updates.
pub const CHROMIUM_EXT_ID: &str = "ieogbbmgppgkglcdmaknkgmopgeknkof";
/// Declared in extensions/firefox-catch/manifest.json (gecko id).
pub const GECKO_ID: &str = "catch@io.github.tahadx";

const CHROMIUM_TEMPLATE: &str = include_str!("../templates/chromium-host.json");
const FIREFOX_TEMPLATE: &str = include_str!("../templates/firefox-host.json");

const CHROMIUM_DIRS: &[&str] = &[
    ".config/chromium",
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/google-chrome-unstable",
    ".config/BraveSoftware/Brave-Browser",
    ".config/BraveSoftware/Brave-Browser-Beta",
    ".config/BraveSoftware/Brave-Browser-Nightly",
    ".config/BraveSoftware/Brave-Origin",
    ".config/BraveSoftware/Brave-Origin-Beta",
    ".config/BraveSoftware/Brave-Origin-Nightly",
    ".config/microsoft-edge",
    ".config/microsoft-edge-dev",
];

const FIREFOX_DIRS: &[&str] = &[
    ".mozilla",
    ".zen",
    ".librewolf",
    ".floorp",
    ".waterfox",
    ".var/app/org.mozilla.firefox/.mozilla",
    ".var/app/app.zen_browser.zen/.zen",
    ".var/app/io.gitlab.librewolf-community/.librewolf",
];

const FLAG_FILES: &[&str] = &[
    "chromium-flags.conf",
    "chrome-flags.conf",
    "brave-flags.conf",
    "brave-origin-flags.conf",
    "microsoft-edge-flags.conf",
    "microsoft-edge-dev-flags.conf",
];

fn home() -> PathBuf {
    crate::state::home_dir()
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn render(template: &str, host_bin: &str, ext_id: &str) -> String {
    template
        .replace("__HOST_PATH__", host_bin)
        .replace("__EXTENSION_ID__", ext_id)
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// The catch-host binary users' browsers spawn: the installed link when
/// present, else this very binary's canonical location.
pub fn host_binary_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let linked = home().join(".local/bin/catch-host");
    if is_executable(&linked) {
        return Ok(linked);
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .context("locate catch-host (pass --host)")?;
    Ok(exe)
}

/// Find the plugin's chromium extension dir from this binary's location:
/// shipped layout `<plugin>/bin/dist/*`, or a cargo workspace whose
/// target dir sits at the repo root.
pub fn extension_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        anyhow::ensure!(
            p.join("manifest.json").is_file(),
            "no manifest.json in {}",
            p.display()
        );
        return Ok(p);
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .context("locate extension dir (pass DIR)")?;
    let mut cursor = exe.as_path();
    // Shipped layout: <plugin>/bin/dist/<bin> — up three.
    if cursor
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        == Some("dist")
    {
        if let Some(plugin) = cursor
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let ext = plugin.join("extensions/chromium-catch");
            if ext.join("manifest.json").is_file() {
                return Ok(ext);
            }
        }
    }
    // Cargo layout: <repo>/target/<profile>/<bin> — walk up for the dir.
    for _ in 0..5 {
        if let Some(parent) = cursor.parent() {
            cursor = parent;
            let ext = cursor.join("extensions/chromium-catch");
            if ext.join("manifest.json").is_file() {
                return Ok(ext);
            }
        }
    }
    anyhow::bail!("extension dir not found (pass DIR explicitly)")
}

/// Write Chromium + Firefox native host manifests for every known profile
/// root. Idempotent: same bytes every run.
pub fn install_hosts(explicit_host: Option<PathBuf>) -> Result<()> {
    let host = host_binary_path(explicit_host)?;
    let host = host.to_string_lossy().into_owned();
    let home = home();
    for rel in CHROMIUM_DIRS {
        let path = home
            .join(rel)
            .join("NativeMessagingHosts")
            .join(format!("{HOST_NAME}.json"));
        write_file(&path, &render(CHROMIUM_TEMPLATE, &host, CHROMIUM_EXT_ID))?;
    }
    for rel in FIREFOX_DIRS {
        let path = home
            .join(rel)
            .join("native-messaging-hosts")
            .join(format!("{HOST_NAME}.json"));
        write_file(&path, &render(FIREFOX_TEMPLATE, &host, GECKO_ID))?;
    }
    println!("installed {HOST_NAME} host -> {host}");
    Ok(())
}

/// Merge one `--load-extension=` entry into a flags file's content.
/// Returns None when already listed (idempotent). Any *other* Catch
/// checkout path (repo vs installed plugin copy) is replaced, so exactly
/// one Catch entry survives no matter which binary ran last.
fn apply_flag(content: &str, ext: &str) -> Option<String> {
    let mut changed = false;
    let mut out: Vec<String> = Vec::new();
    let mut has_flag = false;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("--load-extension=") {
            has_flag = true;
            let mut parts: Vec<&str> = rest
                .split(',')
                .filter(|p| !p.is_empty())
                .filter(|p| !p.ends_with("extensions/chromium-catch") || *p == ext)
                .collect();
            if !parts.contains(&ext) {
                parts.push(ext);
                changed = true;
            }
            // A replaced checkout path counts as a change even when the
            // entry itself was already listed.
            if parts.len() != rest.split(',').filter(|p| !p.is_empty()).count() {
                changed = true;
            }
            out.push(format!("--load-extension={}", parts.join(",")));
        } else {
            out.push(line.to_string());
        }
    }
    if !has_flag {
        if !out.is_empty() && !out.last().map(|l| l.is_empty()).unwrap_or(true) {
            out.push(String::new());
        }
        out.push(format!("--load-extension={ext}"));
        changed = true;
    }
    if !changed {
        return None;
    }
    let mut text = out.join("\n");
    if content.ends_with('\n') || content.is_empty() {
        text.push('\n');
    }
    Some(text)
}

/// Register the extension in every Chromium-family flags file that already
/// exists (browsers in use). Same rail Omarchy's own extensions ride.
pub fn install_extension(explicit_dir: Option<PathBuf>) -> Result<()> {
    let ext = extension_dir(explicit_dir)?;
    let ext = ext
        .canonicalize()
        .unwrap_or(ext)
        .to_string_lossy()
        .into_owned();
    let config = config_home();
    let mut touched = 0;
    for file in FLAG_FILES {
        let path = config.join(file);
        if !path.is_file() {
            continue;
        }
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if let Some(next) = apply_flag(&content, &ext) {
            write_file(&path, &next)?;
            touched += 1;
        }
    }
    println!("catch: extension registered in {touched} browser flags file(s)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_render_both_placeholders() {
        for (tpl, id) in [
            (CHROMIUM_TEMPLATE, CHROMIUM_EXT_ID),
            (FIREFOX_TEMPLATE, GECKO_ID),
        ] {
            let out = render(tpl, "/h/catch-host", id);
            assert!(!out.contains("__HOST_PATH__"), "host placeholder left");
            assert!(!out.contains("__EXTENSION_ID__"), "id placeholder left");
            assert!(out.contains("/h/catch-host"));
            assert!(out.contains(id));
            let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
            assert_eq!(v["name"], HOST_NAME);
        }
    }

    #[test]
    fn chromium_template_uses_origins_firefox_uses_extensions() {
        let c = render(CHROMIUM_TEMPLATE, "/h", "EXTID");
        assert!(c.contains("allowed_origins") && c.contains("chrome-extension://EXTID/"));
        let f = render(FIREFOX_TEMPLATE, "/h", "EXTID");
        assert!(f.contains("allowed_extensions") && !f.contains("allowed_origins"));
    }

    #[test]
    fn dir_lists_cover_both_families() {
        assert!(CHROMIUM_DIRS.len() >= 10);
        assert!(FIREFOX_DIRS.len() >= 5);
        assert!(FLAG_FILES.len() >= 5);
        assert!(CHROMIUM_DIRS.iter().any(|d| d.contains("chromium")));
        assert!(FIREFOX_DIRS.iter().any(|d| d.contains(".mozilla")));
    }

    #[test]
    fn flag_appended_when_missing() {
        let next = apply_flag("--ozone-platform=wayland\n", "/ext").unwrap();
        assert!(next.contains("--load-extension=/ext\n"));
        assert!(next.starts_with("--ozone-platform=wayland\n"));
    }

    #[test]
    fn flag_extended_without_duplicates() {
        let once = apply_flag("--load-extension=/a,/b\n", "/ext").unwrap();
        assert!(once.contains("--load-extension=/a,/b,/ext"));
        assert!(apply_flag(&once, "/ext").is_none());
        assert!(apply_flag("--load-extension=/ext\n", "/ext").is_none());
    }

    #[test]
    fn flag_replaces_other_checkout_path() {
        let other = "/home/dev/catch/extensions/chromium-catch";
        let mine =
            "/home/taha/.config/omarchy/plugins/io.github.tahadx.catch/extensions/chromium-catch";
        let once = apply_flag(&format!("--load-extension=/a,{other}\n"), mine).unwrap();
        assert!(once.contains(mine));
        assert!(!once.contains(other));
        assert_eq!(once.matches("chromium-catch").count(), 1);
        // Already exactly ours: no-op.
        assert!(apply_flag(&once, mine).is_none());
    }

    #[test]
    fn flag_preserves_other_lines_bytewise() {
        let content = "# comment\n--foo=1\n--load-extension=/a\n\n";
        let next = apply_flag(content, "/ext").unwrap();
        assert!(next.starts_with("# comment\n--foo=1\n"));
        assert!(next.contains("--load-extension=/a,/ext\n"));
    }

    #[test]
    fn install_hosts_writes_both_families_to_sandbox() {
        let tmp = std::env::temp_dir().join(format!("catch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let host = "/fake/catch-host";
        let cpath = tmp.join(".config/chromium/NativeMessagingHosts/io.github.tahadx.catch.json");
        write_file(&cpath, &render(CHROMIUM_TEMPLATE, host, CHROMIUM_EXT_ID)).unwrap();
        let fpath = tmp.join(".mozilla/native-messaging-hosts/io.github.tahadx.catch.json");
        write_file(&fpath, &render(FIREFOX_TEMPLATE, host, GECKO_ID)).unwrap();
        let c: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cpath).unwrap()).unwrap();
        assert!(c["allowed_origins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains(CHROMIUM_EXT_ID)));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

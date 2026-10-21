//! Cross-process connection budget.
//!
//! One worker process owns one queue entry, but the connection budget is
//! global: ten workers each opening eight sockets would hit a server with
//! eighty requests at once. Permits live in a small registry file under the
//! state directory, taken and released under the state lock:
//!
//! * every worker takes one permit before its first request and holds it for
//!   its whole life — retries, probes and every segment reuse the same
//!   reserved capacity, so nothing the worker does can bypass the budget;
//! * a segmented worker grows its reservation up to its share when the
//!   server proves it supports ranges, and then runs with whatever it
//!   actually holds;
//! * permits of dead processes are reaped on the next acquire, and a reboot
//!   (new boot id) clears the registry outright, so a crashed worker cannot
//!   leak capacity for ever;
//! * `Drop` releases them on every exit path — error, cancel, panic.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::state::{now_secs, pid_live, with_lock};

static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Permit {
    pid: u32,
    host: String,
    token: String,
    at: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Registry {
    /// Permits taken by dead workers are only reaped when re-examined, so the
    /// count here is an upper bound the next acquire trims.
    #[serde(default)]
    permits: Vec<Permit>,
    /// Kernel boot id: across a reboot every pid is suspect, so permits are
    /// not trusted at all.
    #[serde(default)]
    boot: String,
}

/// How many concurrent connections Catch may use machine-wide. The explicit
/// environment override wins; otherwise the traffic mode decides (unlimited
/// by default). At least one, so the queue always makes progress.
pub fn budget() -> usize {
    if let Ok(raw) = std::env::var("CATCH_MAX_CONNECTIONS") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if (1..=64).contains(&n) {
                return n;
            }
        }
    }
    crate::state::traffic_mode().default_budget().clamp(1, 64)
}

/// Per-host ceiling. Defaults to the global budget (no extra restriction),
/// lowered with CATCH_MAX_PER_HOST when one server must be protected.
pub fn per_host_limit() -> usize {
    if let Ok(raw) = std::env::var("CATCH_MAX_PER_HOST") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if (1..=64).contains(&n) {
                return n;
            }
        }
    }
    budget()
}

/// The host part of a URL used for per-host accounting; "-" for schemes
/// without one.
pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_else(|| "-".to_string())
}

fn registry_path() -> PathBuf {
    crate::state::state_dir().join("connections.json")
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn load() -> Registry {
    let boot = boot_id();
    match std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Registry>(&t).ok())
    {
        Some(reg) if reg.boot == boot => reg,
        // Different boot, unreadable or corrupt: nothing in there is
        // trustworthy, and an empty registry only costs a moment of
        // undercounting — workers that still hold permits keep running.
        _ => Registry {
            permits: Vec::new(),
            boot,
        },
    }
}

fn save(reg: &Registry) -> Result<()> {
    let dir = crate::state::ensure_state_dir()?;
    let tmp = dir.join("connections.json.tmp");
    let path = registry_path();
    let body = serde_json::to_string(reg)?;
    crate::state::write_private(&tmp, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| anyhow::anyhow!("rename {}: {e}", path.display()))?;
    Ok(())
}

/// Forget permits whose owner process is gone. A live pid is never touched:
/// transfers may legitimately run for hours.
fn reap(reg: &mut Registry) -> usize {
    let before = reg.permits.len();
    reg.permits.retain(|p| pid_live(p.pid));
    before - reg.permits.len()
}

/// How many permits `want` more would actually get for `host` right now.
fn room(reg: &Registry, host: &str, want: usize) -> usize {
    let global_room = budget().saturating_sub(reg.permits.len());
    let host_used = reg.permits.iter().filter(|p| p.host == host).count();
    let host_room = per_host_limit().saturating_sub(host_used);
    want.min(global_room).min(host_room)
}

/// One live worker holds a permit for its whole life: retries, probes and
/// segments all reuse it. Released by `Drop` on every exit path, and reaped
/// by pid if the process crashed.
pub struct Permits {
    tokens: Vec<String>,
    host: String,
}

impl Permits {
    /// Reserve up to `want` more permits for this worker (segment fan-out).
    /// Returns how many are held now — fewer connections is slower, not
    /// wrong, so failure just means the caller runs with less fan-out.
    pub fn grow(&mut self, want: usize) -> usize {
        let extra = want.saturating_sub(self.tokens.len());
        if extra == 0 {
            return self.tokens.len();
        }
        let host = self.host.clone();
        let taken = with_lock(|| {
            let mut reg = load();
            let reaped = reap(&mut reg);
            let granted = room(&reg, &host, extra);
            let mut tokens = Vec::new();
            for _ in 0..granted {
                let token = format!(
                    "{}-{}",
                    std::process::id(),
                    NONCE.fetch_add(1, Ordering::Relaxed)
                );
                reg.permits.push(Permit {
                    pid: std::process::id(),
                    host: host.clone(),
                    token: token.clone(),
                    at: now_secs(),
                });
                tokens.push(token);
            }
            if granted > 0 || reaped > 0 {
                save(&reg)?;
            }
            Ok(tokens)
        })
        .unwrap_or_default();
        self.tokens.extend(taken);
        self.tokens.len()
    }
}

impl Drop for Permits {
    fn drop(&mut self) {
        if self.tokens.is_empty() {
            return;
        }
        let tokens = std::mem::take(&mut self.tokens);
        let pid = std::process::id();
        let _ = with_lock(|| {
            let mut reg = load();
            reg.permits
                .retain(|p| !(p.pid == pid && tokens.contains(&p.token)));
            save(&reg)?;
            Ok(())
        });
    }
}

/// Take one permit for `host`, or `None` when the budget has no room. The
/// caller decides how long to wait and rechecks stop orders while waiting.
pub fn try_acquire(host: &str) -> Option<Permits> {
    let mut permits = Permits {
        tokens: Vec::new(),
        host: host.to_string(),
    };
    if permits.grow(1) == 0 {
        return None;
    }
    Some(permits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_is_stable() {
        assert_eq!(host_of("https://Example.COM/a/b"), "example.com");
        assert_eq!(host_of("https://x.test:8443/f"), "x.test");
        assert_eq!(host_of("not a url"), "-");
    }

    #[test]
    fn traffic_modes_scale_the_configured_budget() {
        use crate::state::TrafficMode;
        // Unlimited is exactly what the settings promise; the other modes
        // trade connections for responsiveness, never to zero.
        assert_eq!(TrafficMode::Unlimited.budget_for(12), 12);
        assert_eq!(TrafficMode::Balanced.budget_for(12), 6);
        assert_eq!(TrafficMode::Low.budget_for(12), 3);
        assert_eq!(TrafficMode::Balanced.budget_for(1), 2, "never stalls");
        assert_eq!(TrafficMode::Low.budget_for(1), 1);
        assert_eq!(TrafficMode::Unlimited.budget_for(1000), 64, "hard ceiling");
    }

    #[test]
    fn budget_defaults_to_the_mode() {
        // No override in tests: whatever this machine's settings derive, the
        // number must be a usable budget (never zero, never unbounded).
        if std::env::var("CATCH_MAX_CONNECTIONS").is_err() {
            assert!((1..=64).contains(&budget()), "sane budget: {}", budget());
        }
    }
}

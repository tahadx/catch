//! Global bandwidth cap.
//!
//! The cap is configured once (widget `bwCapKb` or CATCH_BW_CAP_BPS) and
//! enforced per worker: every worker is allowed `cap / active downloads` and
//! applies that as a token bucket shared by all of its connections — the
//! sequential body stream, every segment task, and every retry inside them.
//! The sum across processes therefore stays inside the cap without any
//! cross-process coordination beyond the state file; the division is
//! refreshed on each progress flush (~4 Hz), so finishing a download hands
//! its share to the survivors within a fraction of a second.
//!
//! Sleeping is done in short slices (<= 250 ms) so stop orders stay
//! responsive: the caller checks its directive between writes, and a
//! throttled connection is never asleep for a full second.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Never starve a connection completely, whatever the cap divides down to.
const MIN_RATE: u64 = 16 * 1024;
/// Allow catching up after a stall or a segment reconnect, but not so much
/// that a burst can swamp the whole cap.
const BURST: f64 = 128.0 * 1024.0;
/// Longest single sleep while waiting for tokens.
const SLEEP_SLICE: f64 = 0.25;

/// Async token bucket. One instance is shared by every connection of one
/// worker process (`Arc` across segment tasks).
pub struct RateLimiter {
    state: Mutex<Bucket>,
}

struct Bucket {
    per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_sec: u64) -> RateLimiter {
        let per_sec = per_sec.max(MIN_RATE) as f64;
        RateLimiter {
            state: Mutex::new(Bucket {
                per_sec,
                tokens: burst(per_sec),
                last: Instant::now(),
            }),
        }
    }

    /// The rate this limiter currently enforces, bytes/second.
    #[cfg(test)]
    pub fn rate(&self) -> u64 {
        self.state
            .lock()
            .map(|b| b.per_sec as u64)
            .unwrap_or(MIN_RATE)
    }

    /// Re-target the rate as the queue grows or shrinks. Tokens accrued at
    /// the old rate are kept (bounded by the burst) so the change itself
    /// never introduces a gap.
    pub fn set_rate(&self, per_sec: u64) {
        let per_sec = per_sec.max(MIN_RATE) as f64;
        if let Ok(mut b) = self.state.lock() {
            let now = Instant::now();
            let dt = now.duration_since(b.last).as_secs_f64();
            b.tokens = (b.tokens + dt * b.per_sec).min(burst(per_sec));
            b.last = now;
            b.per_sec = per_sec;
        }
    }

    /// Block until `bytes` may be sent at the current rate.
    ///
    /// Chunks larger than the burst are paid for in burst-sized slices: the
    /// bucket can never hold more than `burst`, so a single huge request
    /// would otherwise wait forever for tokens it can never accumulate.
    pub async fn acquire(&self, bytes: u64) {
        let mut left = bytes;
        while left > 0 {
            let slice = left.min(BURST as u64);
            self.acquire_slice(slice).await;
            left -= slice;
        }
    }

    async fn acquire_slice(&self, bytes: u64) {
        loop {
            let wait = {
                let Ok(mut b) = self.state.lock() else {
                    return;
                };
                let now = Instant::now();
                let dt = now.duration_since(b.last).as_secs_f64();
                b.tokens = (b.tokens + dt * b.per_sec).min(burst(b.per_sec));
                b.last = now;
                if b.tokens >= bytes as f64 {
                    b.tokens -= bytes as f64;
                    return;
                }
                let need = bytes as f64 - b.tokens;
                Duration::from_secs_f64((need / b.per_sec).min(SLEEP_SLICE))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

fn burst(per_sec: f64) -> f64 {
    per_sec.max(BURST)
}

/// This worker's share of the global cap with `active` downloads running.
/// `None` means no cap is configured: callers skip limiting entirely.
pub fn worker_limit(active: usize) -> Option<u64> {
    let cap = crate::state::bandwidth_cap_bps();
    if cap == 0 {
        return None;
    }
    let active = active.max(1) as u64;
    Some((cap / active).max(MIN_RATE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_when_no_cap() {
        if std::env::var("CATCH_BW_CAP_BPS").is_err() {
            assert_eq!(worker_limit(3), None);
        }
    }

    #[test]
    fn empty_requests_never_block() {
        let limiter = RateLimiter::new(64 * 1024);
        let t = std::time::Instant::now();
        // Tokio runtime for the sleep path — with 0 bytes there is none.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(limiter.acquire(0));
        assert!(t.elapsed() < Duration::from_millis(200));
        assert_eq!(limiter.rate(), 64 * 1024);
    }

    #[test]
    fn a_low_rate_actually_slows_a_big_chunk() {
        let limiter = RateLimiter::new(64 * 1024);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let t = std::time::Instant::now();
        // Burst is 128 KiB; ask for 256 KiB at 64 KiB/s => ~2 s for the rest.
        rt.block_on(limiter.acquire(256 * 1024));
        let elapsed = t.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1500),
            "256 KiB at 64 KiB/s took only {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_secs(4),
            "256 KiB at 64 KiB/s took {elapsed:?}"
        );
    }
}

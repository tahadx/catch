use anyhow::Result;

use crate::state::{update_state, with_lock, Status};

/// Releases the single-drainer marker however this drain ends, including an
/// early return or a panic.
struct DrainGuard;

impl Drop for DrainGuard {
    fn drop(&mut self) {
        crate::state::release_drain_slot();
    }
}

/// Start queued entries while slots are free (up to `max`). The whole
/// decision runs under the state lock: concurrent triggers (browser
/// handoff, worker completion, panel watcher) serialize, see the same
/// reservations, and never over-spawn or lose entries.
pub fn drain(max: usize) -> Result<()> {
    // One drainer at a time. spawn_drain hands out the marker, but a manual
    // `catch drain` and a scheduler tick can both arrive at once, and two of
    // them reserving the same queued rows is what produced duplicate rows
    // sharing one id.
    if !crate::state::claim_drain_slot() {
        return Ok(());
    }
    let _guard = DrainGuard;
    let max = max.clamp(1, 10);
    let ids: Vec<(String, bool, bool)> = with_lock(|| {
        let mut state = crate::state::load_state()?;
        // Free slots held by dead workers first: an entry stuck at 100%
        // (or anywhere) with no process behind it goes back to queued.
        let freed = crate::state::reap_orphans(&mut state);
        let active = state
            .downloads
            .iter()
            .filter(|d| d.status == Status::Downloading)
            .count();
        if freed > 0 {
            eprintln!("catch: requeued {freed} orphaned download(s)");
        }
        if active >= max {
            // Nothing to start: save only if reaping changed something,
            // otherwise every drain rewrites the file and retriggers every
            // watcher into a hot loop.
            if freed > 0 {
                crate::state::save_state(&state)?;
            }
            return Ok(Vec::new());
        }
        // Schedule: outside the window nothing new starts. Entries stay
        // queued — marked so the UI can say why — and the next in-window
        // tick releases them. Transfers already live are parked by tick.
        if crate::schedule::window_closed() == Some(true) {
            let mut marked = 0;
            for d in state.downloads.iter_mut() {
                if d.status == Status::Queued && !d.sched_hold {
                    d.sched_hold = true;
                    marked += 1;
                }
            }
            if freed > 0 || marked > 0 {
                crate::state::save_state(&state)?;
            }
            return Ok(Vec::new());
        }
        // Priority first (higher runs first), FIFO inside a priority level:
        // the queue order is the tiebreaker, and sort_by is stable.
        let mut queued: Vec<&crate::state::Download> = state
            .downloads
            .iter()
            .filter(|d| d.status == Status::Queued)
            .collect();
        queued.sort_by_key(|d| std::cmp::Reverse(d.priority));
        let ids: Vec<(String, bool, bool)> = queued
            .into_iter()
            .take(max - active)
            .map(|d| {
                (
                    d.id.clone(),
                    d.kind == crate::state::DownloadKind::Video,
                    d.audio_only,
                )
            })
            .collect();
        // Reserve slots before spawning, stamping our own pid so a
        // concurrent drain sees a live holder instead of an orphan.
        // Workers re-stamp themselves on startup; a failed spawn re-queues.
        // `claimed_at` records when the reservation was made so a reaper
        // leaves it alone until the worker has had time to appear.
        let holder = std::process::id();
        let claimed = crate::state::now_secs();
        for (id, _, _) in &ids {
            if let Some(d) = state.downloads.iter_mut().find(|d| &d.id == id) {
                if d.status == Status::Queued {
                    d.status = Status::Downloading;
                    d.pid = holder;
                    d.claimed_at = claimed;
                    d.sched_hold = false;
                    d.next_retry_at = 0;
                }
            }
        }
        crate::state::save_state(&state)?;
        Ok(ids)
    })?;
    if ids.is_empty() {
        return Ok(());
    }
    let Ok(exe) = std::env::current_exe() else {
        return Ok(());
    };
    for (id, video, audio) in &ids {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(if *video { "video" } else { "download" });
        if *video && *audio {
            cmd.arg("--audio");
        }
        let spawned = cmd
            .arg(id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok();
        if !spawned {
            let _ = update_state(|state| {
                if let Some(d) = state.downloads.iter_mut().find(|d| &d.id == id) {
                    d.status = Status::Queued;
                    d.pid = 0;
                }
                Ok(())
            });
        }
    }
    Ok(())
}

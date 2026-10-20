use anyhow::Result;
use std::os::unix::process::CommandExt;

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
    let now = crate::state::now_secs();
    let ids: Vec<(String, bool, bool)> = with_lock(|| {
        let mut state = crate::state::load_state()?;
        // Free slots held by dead workers first: an entry stuck at 100%
        // (or anywhere) with no process behind it goes back to queued.
        let freed = crate::state::reap_orphans(&mut state);
        // Apply the window on every drain, not only on a timer tick: a
        // parked transfer must resume and a held row must be released even
        // if the timer is missing and only a worker completion (or the
        // panel) triggers this pass.
        let open = crate::schedule::window_closed() != Some(true);
        let window = crate::schedule::apply_window(&mut state, open);
        let changed = freed > 0 || window.released > 0 || window.parked > 0;
        let active = state
            .downloads
            .iter()
            .filter(|d| d.status == Status::Downloading)
            .count();
        if freed > 0 {
            eprintln!("catch: requeued {freed} orphaned download(s)");
        }
        if window.parked > 0 {
            eprintln!("catch: window closed, parked {window}");
        }
        if active >= max {
            // Nothing to start: save only if reaping/window work changed
            // something, otherwise every drain rewrites the file and
            // retriggers every watcher into a hot loop.
            if changed {
                crate::state::save_state(&state)?;
            }
            return Ok(Vec::new());
        }
        // Schedule: outside the window nothing new starts. Entries stay
        // queued — marked so the UI can say why — and the next in-window
        // tick releases them. Transfers already live are parked by tick.
        // A future one-time entry is not marked as window-held: its own
        // time has not arrived, which the panel reports separately.
        if !open {
            let mut marked = 0;
            for d in state.downloads.iter_mut() {
                if d.status == Status::Queued
                    && !d.sched_hold
                    && crate::schedule::one_time_ready(d, now)
                {
                    d.sched_hold = true;
                    marked += 1;
                }
            }
            if changed || marked > 0 {
                crate::state::save_state(&state)?;
            }
            return Ok(Vec::new());
        }
        // Priority first (higher runs first), FIFO inside a priority level:
        // the queue order is the tiebreaker, and sort_by is stable. A
        // one-time delay is a hard gate: neither the timer, the panel, a
        // manual `catch drain` nor a worker completion may start it early.
        let mut queued: Vec<&crate::state::Download> = state
            .downloads
            .iter()
            .filter(|d| d.status == Status::Queued && crate::schedule::one_time_ready(d, now))
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
                    // Consume the one-time delay: a failure/requeue after
                    // this point must run now, not wait for the old time
                    // again. sched_owned marks this transfer as started by
                    // the scheduler so a window close may park it later.
                    d.scheduled_at = 0;
                    d.sched_owned = true;
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
            // Tells the worker a scheduler pass owns this transfer, so the
            // window may park it. A manual `catch download` omits it.
            .arg("--scheduler")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Own process group: Ctrl-C in the terminal that ran `catch
            // drain` or `catch tick` must not take running transfers down
            // with it. The systemd pass needs KillMode=process for the same
            // reason at the cgroup level.
            .process_group(0)
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

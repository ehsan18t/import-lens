//! Hands idle threads' freed memory back to the OS (ADR-0007, decision 3).
//!
//! mimalloc gives every thread its own heap, and a page whose blocks another thread freed is only
//! reclaimed when its owner next allocates or collects. A pool thread that goes idle after a build
//! does neither, so the build's pages stay resident however short the purge delay. Every long-lived
//! thread therefore collects its own heap when it goes idle: Tokio workers from their park hook,
//! the Rayon pools from one sweep that runs once the daemon's activity has settled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long activity must stay quiet before the Rayon pools are swept. Long enough that a sweep
/// never lands between the steps of one request; short against how long a session sits idle.
const SETTLE: Duration = Duration::from_secs(2);

static ACTIVITY: AtomicU64 = AtomicU64::new(0);

/// Collects the calling thread's heap. Tokio's `on_thread_park` hook on every runtime.
pub fn collect_this_thread() {
    // SAFETY: `mi_collect` only touches the calling thread's own heap.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

/// Records that work ran (a frame arrived, a build or a lane job finished), so the next quiet
/// period ends in a sweep.
pub fn note_activity() {
    ACTIVITY.fetch_add(1, Ordering::Relaxed);
}

/// Sweeps the Rayon pools once after every burst of activity, when it has been quiet for
/// [`SETTLE`]. Runs for the life of the IPC runtime; a quiet daemon costs one timer tick per period.
pub async fn sweep_when_settled() {
    let mut seen = ACTIVITY.load(Ordering::Relaxed);
    let mut swept = seen;
    loop {
        tokio::time::sleep(SETTLE).await;
        let now = ACTIVITY.load(Ordering::Relaxed);
        if now != seen {
            seen = now;
            continue;
        }
        if now == swept {
            continue;
        }
        swept = now;
        // `broadcast` waits for every worker to reach the job, so it must not block a Tokio worker.
        let _ = tokio::task::spawn_blocking(sweep).await;
    }
}

fn sweep() {
    rayon::broadcast(|_| collect_this_thread());
    crate::pipeline::asset_boundary::broadcast_to_workers(collect_this_thread);
    collect_this_thread();
}

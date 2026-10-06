//! Bounded lanes over the daemon's threads (ADR-0007).
//!
//! The daemon has one CPU pool, Rayon's global one. Background work (prewarm, workspace reports)
//! shares it through [`background`], which runs at most half the pool at once, so interactive work
//! always finds free workers. Registry refresh is network I/O and must not hold a CPU worker while
//! it waits, so a [`registry`] lane runs on plain threads that exist only while it has work: each
//! exits when the queue drains, and its allocator heap goes with it. Each service owns its registry
//! lane, so one connection's stalled fetches never queue another's.
//!
//! Spawning never blocks the caller. A job waits in the lane's queue until a runner is free.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct Lane {
    name: &'static str,
    limit: usize,
    start_runner: fn(Arc<Lane>),
    state: Mutex<LaneState>,
}

struct LaneState {
    queue: VecDeque<Job>,
    running: usize,
}

impl Lane {
    fn new(name: &'static str, limit: usize, start_runner: fn(Arc<Lane>)) -> Arc<Self> {
        Arc::new(Self {
            name,
            limit: limit.max(1),
            start_runner,
            state: Mutex::new(LaneState {
                queue: VecDeque::new(),
                running: 0,
            }),
        })
    }

    pub fn spawn(self: &Arc<Self>, job: impl FnOnce() + Send + 'static) {
        let start = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.queue.push_back(Box::new(job));
            let start = state.running < self.limit;
            if start {
                state.running += 1;
            }
            start
        };
        if start {
            (self.start_runner)(Arc::clone(self));
        }
    }

    /// Runs queued jobs until none is left. A panicking job is logged and the runner moves on: an
    /// unwinding runner would leave `running` counted forever and narrow the lane.
    fn drain(&self) {
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                match state.queue.pop_front() {
                    Some(job) => job,
                    None => {
                        state.running -= 1;
                        return;
                    }
                }
            };
            if catch_unwind(AssertUnwindSafe(job)).is_err() {
                crate::logging::log_warn("lanes", format!("a {} job panicked", self.name));
            }
            crate::reclaim::note_activity();
        }
    }
}

/// Prewarm and workspace reports, on the CPU pool, at most half its width at a time.
pub fn background() -> &'static Arc<Lane> {
    static LANE: OnceLock<Arc<Lane>> = OnceLock::new();
    LANE.get_or_init(|| {
        Lane::new("background", rayon::current_num_threads() / 2, |lane| {
            rayon::spawn(move || lane.drain());
        })
    })
}

/// A new registry-refresh lane. The width is the in-flight request cap, not a thread pool.
pub fn registry() -> Arc<Lane> {
    Lane::new(
        "registry",
        crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
        |lane| {
            let runner = Arc::clone(&lane);
            let spawned = std::thread::Builder::new()
                .name("import-lens-registry".to_owned())
                .spawn(move || runner.drain());
            // Out of threads: drain here rather than strand the queue with `running` counted.
            if spawned.is_err() {
                lane.drain();
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::Lane;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    fn thread_lane(limit: usize) -> Arc<Lane> {
        Lane::new("test", limit, |lane| {
            thread::spawn(move || lane.drain());
        })
    }

    /// The bound holds over every job, queued or running, and every job still runs.
    #[test]
    fn a_lane_never_runs_more_jobs_at_once_than_its_limit() {
        let lane = thread_lane(2);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));

        for _ in 0..12 {
            let (running, peak, finished) = (
                Arc::clone(&running),
                Arc::clone(&peak),
                Arc::clone(&finished),
            );
            lane.spawn(move || {
                let now = running.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(now, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(5));
                running.fetch_sub(1, Ordering::AcqRel);
                finished.fetch_add(1, Ordering::AcqRel);
            });
        }
        while finished.load(Ordering::Acquire) < 12 {
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(peak.load(Ordering::Acquire), 2);
    }

    /// A panic must not cost the lane a runner: with a width of one, a lost runner would leave the
    /// next job queued forever.
    #[test]
    fn a_panicking_job_does_not_narrow_the_lane() {
        let lane = thread_lane(1);
        lane.spawn(|| panic!("forced lane panic (test only)"));
        let (sender, receiver) = mpsc::channel();
        lane.spawn(move || {
            let _ = sender.send(());
        });

        assert!(
            receiver.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the job after a panic never ran"
        );
    }
}

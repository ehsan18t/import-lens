use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

type AnalysisFlightKey = (String, u64);

#[derive(Clone)]
pub struct AnalysisFlightRegistry<T> {
    flights: Arc<Mutex<HashMap<AnalysisFlightKey, Arc<AnalysisFlight<T>>>>>,
}

impl<T> AnalysisFlightRegistry<T> {
    pub fn new() -> Self {
        Self {
            flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Runs one compute for a cache key within a cache generation. Followers block
    /// until the leader publishes a result; if the leader panics or exits before
    /// publishing, followers retry and one becomes the replacement leader.
    pub fn run_or_join(&self, key: String, generation: u64, compute: impl FnOnce() -> T) -> T
    where
        T: Clone,
    {
        let flight_key = (key, generation);
        let mut compute = Some(compute);

        loop {
            match self.claim_flight(flight_key.clone()) {
                FlightClaim::Leader(mut leader) => {
                    let compute = compute
                        .take()
                        .expect("analysis flight compute should run at most once");
                    let result = compute();
                    leader.publish(result.clone());
                    return result;
                }
                FlightClaim::Follower(flight) => {
                    if let Some(result) = flight.wait_for_result() {
                        return result;
                    }
                }
            }
        }
    }

    fn claim_flight(&self, key: AnalysisFlightKey) -> FlightClaim<T> {
        let mut flights = lock_unpoisoned(&self.flights);
        match flights.entry(key.clone()) {
            Entry::Occupied(entry) => FlightClaim::Follower(Arc::clone(entry.get())),
            Entry::Vacant(entry) => {
                let flight = Arc::new(AnalysisFlight::default());
                entry.insert(Arc::clone(&flight));
                FlightClaim::Leader(AnalysisFlightLeader {
                    flights: Arc::clone(&self.flights),
                    key,
                    flight,
                    published: false,
                })
            }
        }
    }
}

impl<T> Default for AnalysisFlightRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

enum FlightClaim<T> {
    Leader(AnalysisFlightLeader<T>),
    Follower(Arc<AnalysisFlight<T>>),
}

struct AnalysisFlight<T> {
    state: Mutex<AnalysisFlightState<T>>,
    ready: Condvar,
}

struct AnalysisFlightState<T> {
    result: Option<T>,
    closed: bool,
}

impl<T> Default for AnalysisFlight<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(AnalysisFlightState {
                result: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }
}

impl<T: Clone> AnalysisFlight<T> {
    fn wait_for_result(&self) -> Option<T> {
        let mut state = lock_unpoisoned(&self.state);
        while !state.closed {
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|err| err.into_inner());
        }
        state.result.clone()
    }
}

impl<T> AnalysisFlight<T> {
    fn publish(&self, result: T) {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.result = Some(result);
            state.closed = true;
        }
        self.ready.notify_all();
    }

    fn close_without_result(&self) {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.closed = true;
        }
        self.ready.notify_all();
    }
}

struct AnalysisFlightLeader<T> {
    flights: Arc<Mutex<HashMap<AnalysisFlightKey, Arc<AnalysisFlight<T>>>>>,
    key: AnalysisFlightKey,
    flight: Arc<AnalysisFlight<T>>,
    published: bool,
}

impl<T> AnalysisFlightLeader<T> {
    fn publish(&mut self, result: T) {
        self.flight.publish(result);
        self.published = true;
    }

    fn remove_current_flight(&self) {
        let mut flights = lock_unpoisoned(&self.flights);
        if flights
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl<T> Drop for AnalysisFlightLeader<T> {
    fn drop(&mut self) {
        if !self.published {
            self.remove_current_flight();
            self.flight.close_without_result();
            return;
        }

        self.remove_current_flight();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{self, AssertUnwindSafe},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use crate::ipc::protocol::{ConfidenceLevel, ImportResult, ResultFreshness};

    use super::AnalysisFlightRegistry;

    fn result_for(specifier: &str, bytes: u64) -> ImportResult {
        ImportResult {
            specifier: specifier.to_owned(),
            raw_bytes: bytes,
            minified_bytes: bytes,
            gzip_bytes: bytes,
            brotli_bytes: bytes,
            zstd_bytes: bytes,
            cache_hit: false,
            side_effects: false,
            truly_treeshakeable: true,
            is_cjs: false,
            confidence: ConfidenceLevel::High,
            confidence_reasons: Vec::new(),
            error: None,
            diagnostics: Vec::new(),
            module_breakdown: None,
            shared_bytes: None,
            freshness: ResultFreshness::fresh(),
            internal_contributions: Vec::new(),
        }
    }

    fn wait_until_released(pair: &(Mutex<bool>, Condvar)) {
        let (lock, cvar) = pair;
        let mut released = lock.lock().expect("release lock");
        while !*released {
            released = cvar.wait(released).expect("release wait");
        }
    }

    fn release(pair: &(Mutex<bool>, Condvar)) {
        let (lock, cvar) = pair;
        *lock.lock().expect("release lock") = true;
        cvar.notify_all();
    }

    #[test]
    fn run_or_join_coalesces_concurrent_same_generation_analysis() {
        let registry = Arc::new(AnalysisFlightRegistry::new());
        let compute_count = Arc::new(AtomicUsize::new(0));
        let release_compute = Arc::new((Mutex::new(false), Condvar::new()));
        let (leader_started_tx, leader_started_rx) = mpsc::channel();

        let leader_registry = Arc::clone(&registry);
        let leader_count = Arc::clone(&compute_count);
        let leader_release = Arc::clone(&release_compute);
        let leader = thread::spawn(move || {
            leader_registry.run_or_join("react".to_owned(), 7, || {
                leader_count.fetch_add(1, Ordering::SeqCst);
                leader_started_tx.send(()).expect("leader started");
                wait_until_released(&leader_release);
                result_for("react", 42)
            })
        });

        leader_started_rx.recv().expect("leader should start");

        let follower_registry = Arc::clone(&registry);
        let follower_count = Arc::clone(&compute_count);
        let follower = thread::spawn(move || {
            follower_registry.run_or_join("react".to_owned(), 7, || {
                follower_count.fetch_add(1, Ordering::SeqCst);
                result_for("react-follower", 99)
            })
        });

        thread::sleep(Duration::from_millis(50));
        release(&release_compute);

        let leader_result = leader.join().expect("leader thread");
        let follower_result = follower.join().expect("follower thread");

        assert_eq!(compute_count.load(Ordering::SeqCst), 1);
        assert_eq!(leader_result, result_for("react", 42));
        assert_eq!(follower_result, result_for("react", 42));
    }

    #[test]
    fn run_or_join_keeps_different_generations_independent() {
        let registry = Arc::new(AnalysisFlightRegistry::new());
        let compute_count = Arc::new(AtomicUsize::new(0));
        let release_compute = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();

        let gen_1_registry = Arc::clone(&registry);
        let gen_1_count = Arc::clone(&compute_count);
        let gen_1_release = Arc::clone(&release_compute);
        let gen_1_started = started_tx.clone();
        let gen_1 = thread::spawn(move || {
            gen_1_registry.run_or_join("react".to_owned(), 7, || {
                gen_1_count.fetch_add(1, Ordering::SeqCst);
                gen_1_started.send(()).expect("gen 1 started");
                wait_until_released(&gen_1_release);
                result_for("react-gen-7", 7)
            })
        });

        let gen_2_registry = Arc::clone(&registry);
        let gen_2_count = Arc::clone(&compute_count);
        let gen_2_release = Arc::clone(&release_compute);
        let gen_2 = thread::spawn(move || {
            gen_2_registry.run_or_join("react".to_owned(), 8, || {
                gen_2_count.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).expect("gen 2 started");
                wait_until_released(&gen_2_release);
                result_for("react-gen-8", 8)
            })
        });

        started_rx.recv().expect("first generation should start");
        started_rx.recv().expect("second generation should start");
        release(&release_compute);

        let gen_1_result = gen_1.join().expect("gen 1 thread");
        let gen_2_result = gen_2.join().expect("gen 2 thread");

        assert_eq!(compute_count.load(Ordering::SeqCst), 2);
        assert_eq!(gen_1_result, result_for("react-gen-7", 7));
        assert_eq!(gen_2_result, result_for("react-gen-8", 8));
    }

    #[test]
    fn run_or_join_wakes_waiters_after_panicking_leader() {
        let registry = Arc::new(AnalysisFlightRegistry::new());
        let recovery_count = Arc::new(AtomicUsize::new(0));
        let release_panic = Arc::new((Mutex::new(false), Condvar::new()));
        let (leader_started_tx, leader_started_rx) = mpsc::channel();

        let leader_registry = Arc::clone(&registry);
        let leader_release = Arc::clone(&release_panic);
        let leader = thread::spawn(move || {
            panic::catch_unwind(AssertUnwindSafe(|| {
                leader_registry.run_or_join("react".to_owned(), 7, || -> ImportResult {
                    leader_started_tx.send(()).expect("leader started");
                    wait_until_released(&leader_release);
                    panic!("leader failed before publishing")
                });
            }))
        });

        leader_started_rx.recv().expect("leader should start");

        let follower_registry = Arc::clone(&registry);
        let follower_recovery_count = Arc::clone(&recovery_count);
        let follower = thread::spawn(move || {
            follower_registry.run_or_join("react".to_owned(), 7, || {
                follower_recovery_count.fetch_add(1, Ordering::SeqCst);
                result_for("react", 42)
            })
        });

        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        thread::sleep(Duration::from_millis(50));
        release(&release_panic);
        let leader_result = leader.join().expect("leader thread");
        panic::set_hook(previous_hook);

        assert!(leader_result.is_err());
        assert_eq!(
            follower.join().expect("follower thread"),
            result_for("react", 42)
        );
        assert_eq!(recovery_count.load(Ordering::SeqCst), 1);
    }
}

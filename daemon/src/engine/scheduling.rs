//! Bounded miss scheduling for the Rolldown execution boundary.
//!
//! Service callers are synchronous worker threads, so a small scoped worker
//! set feeds the two-permit async engine boundary without parking the global
//! Rayon pool. Returned values preserve input order; callbacks invoked by the
//! work closure naturally remain completion-ordered.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use super::boundary::ENGINE_PERMITS;

/// Two permits, but a worker keeps running after it releases one: minify, compress,
/// fingerprint, insert. At exactly `ENGINE_PERMITS` workers that post-build tail
/// leaves both permits idle with misses still queued behind it. The extra workers
/// exist to refill the permits, not to widen them — concurrency at the engine is
/// still bounded by the semaphore, so peak memory is unchanged.
const MISS_DRAIN_WORKERS: usize = ENGINE_PERMITS + 2;

/// Run `run` over every item with a fixed number of scoped worker threads, returning
/// `(index, result)` in completion order.
fn drain_bounded<T, R, F>(items: &[T], workers: usize, run: F) -> Vec<(usize, R)>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> (usize, R) + Sync,
{
    let workers = workers.min(items.len()).max(1);
    let cursor = AtomicUsize::new(0);
    let completed = Mutex::new(Vec::with_capacity(items.len()));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = run(index, item);
                    completed
                        .lock()
                        .expect("drain results should not be poisoned")
                        .push(result);
                }
            });
        }
    });

    completed
        .into_inner()
        .expect("drain results should not be poisoned")
}

pub(crate) fn drain_ordered<T, R, F>(items: &[T], run: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let mut pairs = drain_bounded(items, ENGINE_PERMITS, |index, item| {
        (index, run(index, item))
    });
    pairs.sort_by_key(|(index, _)| *index);
    pairs.into_iter().map(|(_, result)| result).collect()
}

/// Classify every item at pool width, then drain only the ones that need the engine.
///
/// The engine permits (§9) bound *builds* to two. They say nothing about cache hits,
/// yet running the whole of `analyze_with_cache` inside `drain_ordered` throttled hit
/// and miss alike to two at a time — so a batch of ninety cached imports, none of
/// which touches the engine, was served two-wide. `classify` runs on the Rayon pool
/// (`Ok` = answered, `Err` = pending work); only the `Err`s reach the bounded drain.
///
/// The miss drain runs slightly wider than the permit count on purpose: a worker
/// that finished its build still has to minify, compress, fingerprint and insert,
/// and it does all of that *after* releasing its permit. At exactly two workers that
/// post-build tail leaves both permits idle with work queued behind it.
pub(crate) fn drain_classified<T, P, R, C, F>(items: &[T], classify: C, run: F) -> Vec<R>
where
    T: Sync,
    P: Send,
    R: Send,
    C: Fn(usize, &T) -> Result<R, P> + Sync + Send,
    F: Fn(usize, &T, P) -> R + Sync,
{
    use rayon::prelude::*;

    let classified: Vec<Result<R, P>> = items
        .par_iter()
        .enumerate()
        .map(|(index, item)| classify(index, item))
        .collect();

    let mut settled: Vec<Option<R>> = Vec::with_capacity(classified.len());
    let mut pending: Vec<(usize, P)> = Vec::new();
    for (index, outcome) in classified.into_iter().enumerate() {
        match outcome {
            Ok(result) => settled.push(Some(result)),
            Err(work) => {
                settled.push(None);
                pending.push((index, work));
            }
        }
    }

    if !pending.is_empty() {
        let slots: Vec<Mutex<Option<(usize, P)>>> = pending
            .into_iter()
            .map(|work| Mutex::new(Some(work)))
            .collect();
        let completed = drain_bounded(&slots, MISS_DRAIN_WORKERS, |_, slot| {
            let (index, work) = slot
                .lock()
                .expect("drain slot should not be poisoned")
                .take()
                .expect("each drain slot is taken exactly once");
            (index, run(index, &items[index], work))
        });
        for (index, result) in completed {
            settled[index] = Some(result);
        }
    }

    settled
        .into_iter()
        .map(|result| result.expect("every item is either classified or drained"))
        .collect()
}

pub(crate) fn drain_ordered_owned<T, R, F>(items: Vec<T>, run: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(usize, T) -> R + Sync,
{
    let slots = items
        .into_iter()
        .map(|item| Mutex::new(Some(item)))
        .collect::<Vec<_>>();

    drain_ordered(&slots, |index, slot| {
        let item = slot
            .lock()
            .expect("drain slot should not be poisoned")
            .take()
            .expect("each drain slot is taken exactly once");
        run(index, item)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use super::{ENGINE_PERMITS, drain_classified, drain_ordered, drain_ordered_owned};

    /// The classified drain reorders by construction: hits settle on the Rayon pool
    /// while misses queue for the engine, so the two halves finish interleaved and
    /// out of order. The caller gets input order back or the wrong size lands on the
    /// wrong import.
    #[test]
    fn drain_classified_restores_input_order() {
        let items: Vec<usize> = (0..64).collect();

        let results = drain_classified(
            &items,
            // Odd items are "cache hits", answered immediately; even items are "misses"
            // and must go through the bounded drain.
            |_, item| {
                if item % 2 == 1 {
                    Ok(format!("hit:{item}"))
                } else {
                    Err(*item)
                }
            },
            |_, item, pending| {
                assert_eq!(*item, pending, "the drain must see the item it deferred");
                // Reverse-ordered sleeps: without the index bookkeeping, completion
                // order and input order disagree.
                thread::sleep(Duration::from_micros((64 - pending) as u64 * 50));
                format!("miss:{pending}")
            },
        );

        let expected: Vec<String> = items
            .iter()
            .map(|item| {
                if item % 2 == 1 {
                    format!("hit:{item}")
                } else {
                    format!("miss:{item}")
                }
            })
            .collect();
        assert_eq!(results, expected);
    }

    /// Every item must be settled exactly once — a classified item must not also be
    /// drained, and a deferred one must not be dropped.
    #[test]
    fn drain_classified_runs_each_item_once() {
        let items: Vec<usize> = (0..50).collect();
        let classified = AtomicUsize::new(0);
        let drained = AtomicUsize::new(0);

        let results = drain_classified(
            &items,
            |_, item| {
                classified.fetch_add(1, Ordering::Relaxed);
                if *item < 10 { Ok(*item) } else { Err(*item) }
            },
            |_, _, pending| {
                drained.fetch_add(1, Ordering::Relaxed);
                pending
            },
        );

        assert_eq!(results, items);
        assert_eq!(classified.load(Ordering::Relaxed), 50);
        assert_eq!(drained.load(Ordering::Relaxed), 40);
    }

    #[test]
    fn preserves_input_order_while_work_completes_out_of_order() {
        let completion_order = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&completion_order);
        let output = drain_ordered(&[30_u64, 1, 10], |index, delay| {
            thread::sleep(Duration::from_millis(*delay));
            observed.lock().expect("completion order").push(index);
            index
        });

        assert_eq!(output, vec![0, 1, 2]);
        assert_ne!(
            *completion_order.lock().expect("completion order"),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn caps_work_at_the_engine_permit_count() {
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let output = drain_ordered(&[0, 1, 2, 3], |_, item| {
            let current = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(current, Ordering::AcqRel);
            thread::sleep(Duration::from_millis(10));
            in_flight.fetch_sub(1, Ordering::AcqRel);
            *item
        });

        assert_eq!(output, vec![0, 1, 2, 3]);
        assert_eq!(peak.load(Ordering::Acquire), ENGINE_PERMITS);
    }

    #[test]
    fn owned_drain_moves_each_item_exactly_once() {
        let output = drain_ordered_owned(vec!["a".to_owned(), "b".to_owned()], |_, item| item);
        assert_eq!(output, vec!["a", "b"]);
    }
}

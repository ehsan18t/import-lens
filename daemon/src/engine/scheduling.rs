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

use super::boundary::{ENGINE_PERMITS, is_background, run_as_background};

/// A worker keeps running after it releases its permit (minify, compress, fingerprint,
/// insert), so at exactly `ENGINE_PERMITS` workers that tail idles the permits with misses
/// still queued. The extra workers refill the permits, not widen them: the semaphore still
/// bounds engine concurrency and peak memory.
const MISS_DRAIN_WORKERS: usize = ENGINE_PERMITS + 2;

/// Run `run` over every item with a fixed number of scoped worker threads, returning
/// `(index, result)` in completion order.
///
/// A lone item runs on the caller: the caller blocks on the result either way, and an OS
/// thread spawn costs more than many a classified miss.
fn drain_bounded<T, R, F>(items: &[T], workers: usize, run: F) -> Vec<(usize, R)>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> (usize, R) + Sync,
{
    if items.len() <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| run(index, item))
            .collect();
    }
    let workers = workers.min(items.len());
    let cursor = AtomicUsize::new(0);
    let completed = Mutex::new(Vec::with_capacity(items.len()));
    let background = is_background();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let drain = || loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = run(index, item);
                    completed
                        .lock()
                        .expect("drain results should not be poisoned")
                        .push(result);
                };
                // A worker's builds keep the priority of the caller that queued them.
                if background {
                    run_as_background(drain);
                } else {
                    drain();
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
    let mut pairs = drain_bounded(items, MISS_DRAIN_WORKERS, |index, item| {
        (index, run(index, item))
    });
    pairs.sort_by_key(|(index, _)| *index);
    pairs.into_iter().map(|(_, result)| result).collect()
}

/// Classify every item at pool width, then drain only the ones that need the engine.
///
/// The engine permits (§9) bound *builds*; cache hits and imports that never resolve would
/// be needlessly throttled to drain width. `classify` runs on the Rayon pool (`Ok` =
/// answered, `Err` = pending work); only the `Err`s reach the bounded drain.
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

/// Drain items that are ALL engine misses, at the miss-drain width, running `run` on each as it
/// completes.
///
/// There is no result vector to reassemble: this is the streaming document path, where each
/// completed import is pushed to the client on its own (`ipc::server`), so completion order *is*
/// the delivery order and an import that finishes first is not held back by one that parks.
pub(crate) fn drain_misses_owned<T, F>(items: Vec<T>, run: F)
where
    T: Send,
    F: Fn(T) + Sync,
{
    let slots = items
        .into_iter()
        .map(|item| Mutex::new(Some(item)))
        .collect::<Vec<_>>();

    drain_bounded(&slots, MISS_DRAIN_WORKERS, |index, slot| {
        let item = slot
            .lock()
            .expect("drain slot should not be poisoned")
            .take()
            .expect("each drain slot is taken exactly once");
        (index, run(item))
    });
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

    use super::{
        MISS_DRAIN_WORKERS, drain_classified, drain_misses_owned, drain_ordered,
        drain_ordered_owned, is_background, run_as_background,
    };

    /// A prewarm drain's builds run on worker threads; each must still be admitted as
    /// background work, and an interactive drain's must not be.
    #[test]
    fn drain_workers_keep_the_callers_build_priority() {
        let items: Vec<usize> = (0..MISS_DRAIN_WORKERS * 2).collect();
        let marks = |items: &[usize]| drain_ordered(items, |_, _| is_background());

        assert!(run_as_background(|| marks(&items)).iter().all(|mark| *mark));
        assert!(marks(&items).iter().all(|mark| !*mark));
        assert!(
            !is_background(),
            "the mark is restored when the work returns"
        );
    }

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

    /// Every item must be settled exactly once: a classified item must not also be
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
    fn caps_work_at_the_miss_drain_width() {
        let items: Vec<usize> = (0..MISS_DRAIN_WORKERS * 2).collect();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let output = drain_ordered(&items, |_, item| {
            let current = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(current, Ordering::AcqRel);
            thread::sleep(Duration::from_millis(10));
            in_flight.fetch_sub(1, Ordering::AcqRel);
            *item
        });

        assert_eq!(output, items);
        assert_eq!(peak.load(Ordering::Acquire), MISS_DRAIN_WORKERS);
    }

    /// A lone miss must not pay for a thread spawn: every drain blocks its caller anyway.
    #[test]
    fn a_lone_item_runs_on_the_calling_thread() {
        let caller = thread::current().id();
        let ran_on = Mutex::new(Vec::new());
        let record = |_: usize| {
            ran_on
                .lock()
                .expect("thread ids")
                .push(thread::current().id())
        };

        drain_misses_owned(vec![0], record);
        drain_ordered(&[0], |index, _: &i32| record(index));
        drain_classified(
            &[0],
            |_, item: &i32| Err::<(), i32>(*item),
            |index, _, _| {
                record(index);
            },
        );

        assert_eq!(*ran_on.lock().expect("thread ids"), vec![caller; 3]);
    }

    #[test]
    fn owned_drain_moves_each_item_exactly_once() {
        let output = drain_ordered_owned(vec!["a".to_owned(), "b".to_owned()], |_, item| item);
        assert_eq!(output, vec!["a", "b"]);
    }

    /// One import that parks the bundler must not hold back the imports beside it.
    ///
    /// The drain half of the streaming guarantee: each import that lands is pushed to the
    /// client from here, so emitting in input order (or after the whole set) would let one
    /// parked build delay the document's other imports.
    ///
    /// The slow item is deliberately FIRST: an implementation that collected results and returned
    /// them in order would pass a test where the slow one is last.
    #[test]
    fn a_slow_import_does_not_hold_back_the_ones_beside_it() {
        let parked = Duration::from_millis(400);
        let emitted = Arc::new(Mutex::new(Vec::new()));

        let observed = Arc::clone(&emitted);
        drain_misses_owned(vec![0_usize, 1, 2, 3], move |item| {
            if item == 0 {
                thread::sleep(parked);
            }
            observed.lock().expect("emissions").push(item);
        });

        let emitted = emitted.lock().expect("emissions").clone();
        assert_eq!(
            emitted.len(),
            4,
            "every import must be delivered, the parked one included"
        );
        assert_eq!(
            emitted.last(),
            Some(&0),
            "the parked import must be delivered LAST — the three beside it were not waiting on \
             it: {emitted:?}"
        );
    }
}

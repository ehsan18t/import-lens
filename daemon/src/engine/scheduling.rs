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

pub(crate) fn drain_ordered<T, R, F>(items: &[T], run: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let workers = ENGINE_PERMITS.min(items.len()).max(1);
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
                        .push((index, result));
                }
            });
        }
    });

    let mut pairs = completed
        .into_inner()
        .expect("drain results should not be poisoned");
    pairs.sort_by_key(|(index, _)| *index);
    pairs.into_iter().map(|(_, result)| result).collect()
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

    use super::{ENGINE_PERMITS, drain_ordered, drain_ordered_owned};

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

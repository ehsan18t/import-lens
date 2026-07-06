use super::{BudgetCoordinator, EVICTION_FLOOR, EvictableShard};
use crate::cache::disk::ShardRollup;
use std::cell::RefCell;

struct ReplenishingShard {
    id: String,
    total_bytes: RefCell<u64>,
    entry_count: u64,
    calls: RefCell<u64>,
}

impl ReplenishingShard {
    fn new(id: &str, total_bytes: u64, entry_count: u64) -> Self {
        Self {
            id: id.to_owned(),
            total_bytes: RefCell::new(total_bytes),
            entry_count,
            calls: RefCell::new(0),
        }
    }
}

impl EvictableShard for ReplenishingShard {
    fn shard_id(&self) -> &str {
        &self.id
    }

    fn rollup(&self) -> ShardRollup {
        ShardRollup {
            total_bytes: *self.total_bytes.borrow(),
            oldest_seq: 1,
            // Simulate concurrent refills: the shard keeps at least as many entries
            // even though each eviction frees bytes.
            entry_count: self.entry_count,
        }
    }

    fn lowest_seq_keys(&self, n: usize, _floor: u64) -> Vec<String> {
        (0..n).map(|index| format!("{}:{index}", self.id)).collect()
    }

    fn evict_keys(&self, keys: &[String]) -> u64 {
        *self.calls.borrow_mut() += 1;
        let freed = keys.len() as u64 * 10;
        let mut total = self.total_bytes.borrow_mut();
        *total = total.saturating_sub(freed);
        freed
    }
}

#[test]
fn eviction_continues_when_bytes_are_freed_even_if_entry_count_is_replenished() {
    let shard = ReplenishingShard::new("replenishing", 10_000, EVICTION_FLOOR + 1_000);
    let coordinator = BudgetCoordinator::new(8_000);

    let outcome = coordinator.evict_to_budget(&[&shard]);

    assert!(
        *shard.calls.borrow() > 1,
        "entry-count replenishment must not retire a shard that is still freeing bytes"
    );
    assert!(
        outcome.evicted_bytes > 1_280,
        "the evictor should continue beyond the first batch while bytes are freed"
    );
    assert!(
        !outcome.still_over_budget,
        "freed bytes should carry the shard below the budget low-water mark"
    );
}

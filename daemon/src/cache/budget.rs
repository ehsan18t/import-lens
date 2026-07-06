use crate::cache::disk::ShardRollup;
use std::collections::{HashMap, HashSet};

// Per-project floor: the evictor never touches a shard's `FLOOR` most-recently
// -used entries, so switching to a large project cannot evict a small project's
// warm set out from under the user (design §5.3 / 3.4).
pub const EVICTION_FLOOR: u64 = 128;
// Low-water mark: once over budget (high water), evict down to this fraction of
// the budget so a steady insert stream does not thrash the evictor every insert.
pub const LOW_WATER: f64 = 0.9;
// Keys evicted per victim per round before the victim's rollup is recomputed and
// victim selection re-runs. Bounds re-scan frequency without over-evicting.
pub const EVICTION_BATCH: usize = 128;
// Upper bound on how far victim selection pages through a shard's ascending
// `(last_seq, key)` index while SKIPPING memory-hot entries (promoted in memory
// but not yet flushed — never valid victims). Without a bound, a shard whose
// entire evictable prefix is hot would scan the whole shard every round; with it,
// selection stays O(log N + window) and a genuinely all-hot shard still
// terminates — it returns an empty batch and the evictor retires it for the pass.
// Eight batches deep tolerates a large run of hot entries before giving up; the
// next flush re-persists their promoted seqs, sorting them out of the evictable
// prefix so the shard self-heals on a later pass (Finding 10c).
pub const MAX_EVICTION_SCAN: usize = 8 * EVICTION_BATCH;

/// A shard the byte-budget evictor can inspect and trim. Implemented over the real
/// disk cache in production and over fakes in tests, so the cross-shard eviction
/// loop is unit-testable in isolation.
pub trait EvictableShard {
    fn shard_id(&self) -> &str;
    /// Current byte/recency/count summary of the shard.
    fn rollup(&self) -> ShardRollup;
    /// Up to `n` least-recently-used keys, excluding the shard's `floor` newest.
    fn lowest_seq_keys(&self, n: usize, floor: u64) -> Vec<String>;
    /// Deletes `keys`, returning the on-disk bytes freed.
    fn evict_keys(&self, keys: &[String]) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictionOutcome {
    pub evicted_bytes: u64,
    pub evicted_keys: u64,
    /// True when the cache is still over budget after eviction (every remaining
    /// entry was floor-protected).
    pub still_over_budget: bool,
}

/// Result of one full maintenance pass (byte-budget eviction + compaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaintenanceOutcome {
    pub eviction: EvictionOutcome,
    /// Shard files whose free pages were reclaimed this pass.
    pub compacted_shards: usize,
    /// True when the cheap physical-size gate proved the cache under budget and
    /// the full pass (shard opens + seq scans) was skipped.
    pub skipped_under_budget: bool,
}

/// Owns the global disk-byte budget and runs entry-granular LRU eviction across
/// shards. Exact-enough global LRU: repeatedly pick the shard holding the oldest
/// entry (smallest `oldest_seq`), evict its oldest entries down to the low-water
/// mark, recompute, repeat.
#[derive(Debug)]
pub struct BudgetCoordinator {
    budget_bytes: u64,
}

impl BudgetCoordinator {
    pub fn new(budget_bytes: u64) -> Self {
        Self { budget_bytes }
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Evicts across `shards` until the summed logical bytes are at or below
    /// `budget * LOW_WATER`. A `budget_bytes` of 0 disables the budget (no
    /// eviction). Victim selection is by smallest `oldest_seq`; each shard's newest
    /// `EVICTION_FLOOR` entries are protected.
    pub fn evict_to_budget(&self, shards: &[&dyn EvictableShard]) -> EvictionOutcome {
        let mut outcome = EvictionOutcome::default();
        if self.budget_bytes == 0 {
            return outcome;
        }

        // De-dupe by shard_id before anything keys on it. `collect_shard_targets`
        // should never yield two targets with the same id, but a copied/corrupted
        // shard dir (or a shard-id hash collision) could. Everything below keys on
        // shard_id — the `rollups` map, the `exhausted` set, victim selection — so a
        // duplicate left in the vec would alias ONE rollup across two entries and make
        // `total` (summed over the id-keyed map) under-count the pair. Keeping only the
        // first occurrence per id keeps the vec and the id-keyed accounting consistent.
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let shards: Vec<&dyn EvictableShard> = shards
            .iter()
            .copied()
            .filter(|shard| seen_ids.insert(shard.shard_id()))
            .collect();

        // Snapshot each shard's rollup (ShardRollup is Copy, so no borrow is held
        // across the eviction mutations below).
        let mut rollups: HashMap<String, ShardRollup> = shards
            .iter()
            .map(|shard| (shard.shard_id().to_owned(), shard.rollup()))
            .collect();
        let mut total: u64 = rollups.values().map(|rollup| rollup.total_bytes).sum();

        if total <= self.budget_bytes {
            return outcome;
        }
        let low_water = (self.budget_bytes as f64 * LOW_WATER) as u64;

        // Shards with nothing left to evict (all remaining entries floor-protected).
        let mut exhausted: HashSet<String> = HashSet::new();

        while total > low_water {
            // Victim = the non-exhausted shard with entries above its floor whose
            // oldest entry is globally oldest.
            let victim = shards
                .iter()
                .filter(|shard| !exhausted.contains(shard.shard_id()))
                .filter(|shard| {
                    rollups
                        .get(shard.shard_id())
                        .is_some_and(|rollup| rollup.entry_count > EVICTION_FLOOR)
                })
                .min_by_key(|shard| {
                    rollups
                        .get(shard.shard_id())
                        .map_or(u64::MAX, |rollup| rollup.oldest_seq)
                });

            let Some(victim) = victim else {
                break;
            };

            let keys = victim.lowest_seq_keys(EVICTION_BATCH, EVICTION_FLOOR);
            if keys.is_empty() {
                exhausted.insert(victim.shard_id().to_owned());
                continue;
            }

            let evicted = keys.len() as u64;
            let freed = victim.evict_keys(&keys);
            outcome.evicted_bytes += freed;
            outcome.evicted_keys += evicted;
            total = total.saturating_sub(freed);

            // Recompute the victim's rollup (fresh oldest_seq/total/count) so the
            // next round's victim selection reflects the eviction.
            let after = victim.rollup();
            // Progress guard: if the removal did not actually shrink the shard (a
            // failed/uncommitted write on a full or read-only disk returns
            // freed = 0 and removes no rows), the victim would be re-selected forever.
            // Retire it so the loop terminates rather than spinning at 100% CPU.
            // Entry count alone is not progress: concurrent refills may keep the
            // count flat while bytes are genuinely being freed.
            if freed == 0 {
                exhausted.insert(victim.shard_id().to_owned());
            }
            rollups.insert(victim.shard_id().to_owned(), after);
        }

        outcome.still_over_budget = total > self.budget_bytes;
        outcome
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cache_budget_progress.rs"]
mod cache_budget_progress_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake shard backed by an in-memory `(seq -> size)` map, so the eviction
    /// loop can be exercised without a real redb database.
    struct FakeShard {
        id: String,
        // (last_seq, size_bytes), one per entry, keyed by a synthetic key.
        entries: RefCell<Vec<(u64, u64)>>,
    }

    impl FakeShard {
        fn new(id: &str, entries: Vec<(u64, u64)>) -> Self {
            Self {
                id: id.to_owned(),
                entries: RefCell::new(entries),
            }
        }
    }

    impl EvictableShard for FakeShard {
        fn shard_id(&self) -> &str {
            &self.id
        }

        fn rollup(&self) -> ShardRollup {
            let entries = self.entries.borrow();
            if entries.is_empty() {
                return ShardRollup::empty();
            }
            ShardRollup {
                total_bytes: entries.iter().map(|(_, size)| *size).sum(),
                oldest_seq: entries.iter().map(|(seq, _)| *seq).min().unwrap(),
                entry_count: entries.len() as u64,
            }
        }

        fn lowest_seq_keys(&self, n: usize, floor: u64) -> Vec<String> {
            let mut entries = self.entries.borrow().clone();
            entries.sort_by_key(|(seq, _)| *seq);
            let eligible = entries.len().saturating_sub(floor as usize);
            entries
                .into_iter()
                .take(eligible.min(n))
                .map(|(seq, _)| format!("{}:{seq}", self.id))
                .collect()
        }

        fn evict_keys(&self, keys: &[String]) -> u64 {
            let seqs: HashSet<u64> = keys
                .iter()
                .filter_map(|key| key.rsplit(':').next())
                .filter_map(|seq| seq.parse::<u64>().ok())
                .collect();
            let mut entries = self.entries.borrow_mut();
            let mut freed = 0;
            entries.retain(|(seq, size)| {
                if seqs.contains(seq) {
                    freed += *size;
                    false
                } else {
                    true
                }
            });
            freed
        }
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let shard = FakeShard::new("a", vec![(1, 100), (2, 100)]);
        let coordinator = BudgetCoordinator::new(1_000);
        let outcome = coordinator.evict_to_budget(&[&shard]);
        assert_eq!(outcome.evicted_bytes, 0);
        assert_eq!(shard.entries.borrow().len(), 2);
    }

    #[test]
    fn zero_budget_disables_eviction() {
        let shard = FakeShard::new("a", vec![(1, 100), (2, 100)]);
        let coordinator = BudgetCoordinator::new(0);
        let outcome = coordinator.evict_to_budget(&[&shard]);
        assert_eq!(outcome.evicted_bytes, 0);
        assert_eq!(shard.entries.borrow().len(), 2);
    }

    #[test]
    fn duplicate_shard_ids_are_deduped_to_a_single_target() {
        // Two targets sharing a shard_id (a copied/corrupted shard dir) must collapse
        // to one: everything downstream keys on shard_id, so a duplicate left in the
        // vec would alias one rollup across both. The evictor keeps the FIRST
        // occurrence; the second is never inspected or evicted.
        let first = FakeShard::new("dup", (1..=1_000).map(|seq| (seq, 10)).collect());
        let second = FakeShard::new("dup", (5_000..=5_999).map(|seq| (seq, 10)).collect());

        // Tiny budget forces heavy eviction on whichever target is considered.
        let coordinator = BudgetCoordinator::new(1_000);
        let outcome = coordinator.evict_to_budget(&[&first, &second]);

        assert!(
            outcome.evicted_bytes > 0,
            "the surviving (first) target is evicted"
        );
        assert_eq!(
            first.entries.borrow().len(),
            EVICTION_FLOOR as usize,
            "the first occurrence is evicted down to the per-project floor"
        );
        assert_eq!(
            second.entries.borrow().len(),
            1_000,
            "the duplicate shard_id is skipped entirely, keeping all its entries"
        );
    }

    #[test]
    fn evicts_globally_oldest_first_across_shards_down_to_low_water() {
        // Shard a is large and globally oldest (seqs 1..=1000, 10 bytes = 10000)
        // with plenty of evictable entries beyond its floor. Shard b is newer
        // (seqs 5000..=5199, 10 bytes = 2000). Total 12000.
        let a: Vec<(u64, u64)> = (1..=1000).map(|s| (s, 10)).collect();
        let b: Vec<(u64, u64)> = (5000..=5199).map(|s| (s, 10)).collect();
        let shard_a = FakeShard::new("a", a);
        let shard_b = FakeShard::new("b", b);

        // Budget 11000 → low water 9900 → must free ≥ 2100 bytes. Shard a alone has
        // 872 evictable entries (8720 bytes), so eviction takes only from a.
        let coordinator = BudgetCoordinator::new(11_000);
        let outcome = coordinator.evict_to_budget(&[&shard_a, &shard_b]);

        // Deterministic: two EVICTION_BATCH rounds of a's lowest seqs — exactly
        // seqs 1..=256 (2560 bytes) — bring 12000 down to 9440 ≤ 9900.
        assert_eq!(outcome.evicted_keys, 2 * EVICTION_BATCH as u64);
        assert_eq!(outcome.evicted_bytes, 2 * EVICTION_BATCH as u64 * 10);
        assert!(!outcome.still_over_budget);

        // Shard a lost precisely its oldest entries; shard b (all newer) is
        // untouched because a never ran out of evictable entries.
        let a_min = shard_a
            .entries
            .borrow()
            .iter()
            .map(|(s, _)| *s)
            .min()
            .unwrap_or(0);
        assert_eq!(
            a_min,
            2 * EVICTION_BATCH as u64 + 1,
            "exactly the lowest seqs are gone, nothing else"
        );
        assert_eq!(
            shard_b.entries.borrow().len(),
            200,
            "shard b (all newer) is untouched while a still had evictable entries"
        );
    }

    #[test]
    fn a_stuck_shard_does_not_stop_eviction_from_healthy_shards() {
        // The stuck shard is globally oldest, so it is selected first — and its
        // evictions never persist. The progress guard must retire it and let the
        // loop continue into the healthy shard rather than bail (or spin).
        let stuck = StuckShard {
            id: "stuck".to_owned(),
            rollup: ShardRollup {
                total_bytes: 10_000,
                oldest_seq: 1,
                entry_count: EVICTION_FLOOR + 500,
            },
        };
        let healthy_entries: Vec<(u64, u64)> = (1_000..=2_000).map(|seq| (seq, 10)).collect();
        let healthy = FakeShard::new("healthy", healthy_entries);

        // Total 20010 > budget 12000 → low water 10800.
        let coordinator = BudgetCoordinator::new(12_000);
        let outcome = coordinator.evict_to_budget(&[&stuck, &healthy]);

        assert!(
            outcome.evicted_bytes > 0,
            "the healthy shard must keep evicting after the stuck one is retired"
        );
        assert!(
            healthy.entries.borrow().len() < 1_001,
            "eviction reached the healthy shard"
        );
    }

    /// A shard whose removals never persist (e.g. a full/read-only disk): its
    /// rollup never shrinks, so a naive evictor would re-select it forever.
    struct StuckShard {
        id: String,
        rollup: ShardRollup,
    }

    impl EvictableShard for StuckShard {
        fn shard_id(&self) -> &str {
            &self.id
        }
        fn rollup(&self) -> ShardRollup {
            self.rollup
        }
        fn lowest_seq_keys(&self, n: usize, _floor: u64) -> Vec<String> {
            // Always offers keys, but they never get removed.
            (0..n).map(|i| format!("{}:{i}", self.id)).collect()
        }
        fn evict_keys(&self, _keys: &[String]) -> u64 {
            0
        }
    }

    #[test]
    fn a_shard_whose_eviction_never_progresses_does_not_spin() {
        // Over budget, entry_count well above the floor, but evict_keys frees
        // nothing and the rollup never changes. The progress guard must retire the
        // shard so evict_to_budget returns instead of looping forever.
        let stuck = StuckShard {
            id: "stuck".to_owned(),
            rollup: ShardRollup {
                total_bytes: 10_000,
                oldest_seq: 1,
                entry_count: EVICTION_FLOOR + 500,
            },
        };
        let coordinator = BudgetCoordinator::new(1_000);
        // If the guard is missing this call never returns (the test harness times
        // out); with it, it completes and reports still-over-budget.
        let outcome = coordinator.evict_to_budget(&[&stuck]);
        assert_eq!(outcome.evicted_bytes, 0);
        assert!(outcome.still_over_budget);
    }

    #[test]
    fn per_project_floor_protects_a_small_shards_newest_entries() {
        // Shard b is small: only FLOOR entries, all newest — must survive intact.
        let big: Vec<(u64, u64)> = (1..=1000).map(|s| (s, 100)).collect();
        let small: Vec<(u64, u64)> = (1..=EVICTION_FLOOR).map(|s| (2_000 + s, 100)).collect();
        let shard_big = FakeShard::new("big", big);
        let shard_small = FakeShard::new("small", small);

        // Tiny budget forces heavy eviction.
        let coordinator = BudgetCoordinator::new(10_000);
        coordinator.evict_to_budget(&[&shard_big, &shard_small]);

        assert_eq!(
            shard_small.entries.borrow().len(),
            EVICTION_FLOOR as usize,
            "a shard at or below the floor is never evicted"
        );
    }
}

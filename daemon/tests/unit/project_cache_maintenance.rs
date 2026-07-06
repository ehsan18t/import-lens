use super::{CompactableTarget, aggressive_compact_targets_if_physical_over_budget};
use std::cell::RefCell;

struct FakeCompactTarget {
    results: RefCell<Vec<bool>>,
    thresholds: RefCell<Vec<f64>>,
}

impl FakeCompactTarget {
    fn new(results: Vec<bool>) -> Self {
        Self {
            results: RefCell::new(results),
            thresholds: RefCell::new(Vec::new()),
        }
    }
}

impl CompactableTarget for FakeCompactTarget {
    fn compact_if_fragmented(&self, threshold: f64) -> bool {
        self.thresholds.borrow_mut().push(threshold);
        self.results.borrow_mut().remove(0)
    }
}

#[test]
fn aggressive_compaction_runs_only_when_aggregate_physical_bytes_remain_over_budget() {
    let target = FakeCompactTarget::new(vec![true]);

    let skipped = aggressive_compact_targets_if_physical_over_budget(&[&target], 10_000, 10_000);
    assert_eq!(
        skipped, 0,
        "at-budget physical size does not need another pass"
    );
    assert!(
        target.thresholds.borrow().is_empty(),
        "the aggressive pass should not run when aggregate physical bytes are within budget"
    );

    let compacted = aggressive_compact_targets_if_physical_over_budget(&[&target], 10_001, 10_000);
    assert_eq!(compacted, 1);
    assert_eq!(
        *target.thresholds.borrow(),
        vec![0.0],
        "aggregate over-budget compaction must use a zero threshold"
    );
}

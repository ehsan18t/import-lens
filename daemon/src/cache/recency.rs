use std::sync::atomic::{AtomicU64, Ordering};

// One process-global monotonic recency sequence. Each cache entry stores the
// `last_seq` at which it was last accessed interactively; the capacity evictor
// (Layer 2) treats the smallest `last_seq` as the least-recently-used entry.
//
// A monotonic counter replaces the old wall-clock `last_used_millis`: it has no
// ties (every access gets a distinct value), and a backward wall-clock jump
// (NTP, VM resume) can never make a new access look older than an old one.
//
// Starts at 1 so `0` is reserved for "never promoted" (legacy on-disk rows that
// predate the sequence decode to `last_seq = 0` and thus sort as oldest).
static RECENCY_CLOCK: AtomicU64 = AtomicU64::new(1);

/// Process-global monotonic recency clock. Zero-sized; all state is the static
/// counter above.
pub struct RecencyClock;

impl RecencyClock {
    /// Returns the next sequence value and advances the clock. Strictly
    /// increasing across all callers.
    pub fn next_seq() -> u64 {
        RECENCY_CLOCK.fetch_add(1, Ordering::Relaxed)
    }

    /// Advance the clock so the next `next_seq()` is strictly greater than
    /// `seq`. Called when an entry is hydrated from disk with a persisted
    /// `last_seq`: the counter resets to 1 on every process start, so without
    /// this a fresh post-restart access (small seq) would sort as *older* than
    /// a durable entry from the previous session (large seq), inverting the
    /// evictor's LRU order. Observing every hydrated seq keeps the live clock
    /// ahead of all persisted recency.
    pub fn observe(seq: u64) {
        RECENCY_CLOCK.fetch_max(seq.saturating_add(1), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::RecencyClock;

    #[test]
    fn next_seq_is_strictly_increasing() {
        let a = RecencyClock::next_seq();
        let b = RecencyClock::next_seq();
        let c = RecencyClock::next_seq();
        assert!(
            a < b && b < c,
            "sequence must strictly increase: {a} {b} {c}"
        );
    }

    #[test]
    fn observe_advances_the_clock_past_a_persisted_seq() {
        // Simulate hydrating an entry from a previous session with a large seq.
        let persisted = RecencyClock::next_seq() + 1_000_000;
        RecencyClock::observe(persisted);
        assert!(
            RecencyClock::next_seq() > persisted,
            "observe must keep the live clock ahead of persisted recency"
        );
    }

    #[test]
    fn observe_never_regresses_the_clock() {
        let high = RecencyClock::next_seq() + 1_000_000;
        RecencyClock::observe(high);
        // Observing an older seq must not lower the clock.
        RecencyClock::observe(1);
        assert!(RecencyClock::next_seq() > high);
    }
}

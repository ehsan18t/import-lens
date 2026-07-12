//! The engine time one *request* may spend (spec §9).
//!
//! `boundary::BUILD_TIMEOUT` bounds one **build**. It cannot bound a **request**, because a
//! permit is acquired *outside* it: builds that park do not merely run late, they *queue*.
//! Three imports of one broken package on a two-permit pool fill both permits, park, and time
//! out at 8s — and only then admit the third, which starts a **fresh** 8s clock. That is 16s of
//! engine time for one document. The extension gave up at 10s and rejected the entire
//! `AnalyzeDocumentResponse`, taking every import in that document *already answered from cache*
//! down with it — which is the exact failure the build timeout exists to prevent. More distinct
//! broken packages than permits produce the same serialization, so no per-*entry* memory of what
//! parked can fix it: only a bound on the request can.
//!
//! So the request carries the bound. A budget is a deadline stamped when the request arrives and
//! shared by every build the request triggers. Each build is capped at
//! `min(BUILD_TIMEOUT, deadline - now)`, and — the part that matters — the deadline is checked
//! again *after* the build acquires its permit, so a build that queued behind parked ones
//! abandons instead of starting a fresh clock. An import whose build never starts degrades to
//! the static fallback, which takes no permit and is fast, so the response is assembled inside
//! the client's deadline with every cached hit intact.
//!
//! ## The numbers
//!
//! Each budget must sit **under** the deadline of the client that is waiting for it.
//! **Changing a client timeout without changing the matching budget here re-opens the bug this
//! module exists to close**, so the two are named together below.

use std::time::{Duration, Instant};

/// Interactive requests: `AnalyzeDocument`, `AnalyzeSpecifiers`, `FileSizeDocument`,
/// `EnumerateExports`, `CompleteImportMembers`, and the batch/file-size messages.
///
/// The extension's client gives up after **10s** — every one of those request methods takes
/// `timeoutMs = 10000` by default (`extension/src/ipc/client.ts`) and
/// `extension/src/daemon/nativeTransport.ts` overrides none of them. `importlens check` uses the
/// same 10s for its `file_size_document` request (`cli/importlens.mjs`, `defaultIpcTimeoutMs`).
/// The 1s of headroom covers the hop onto the blocking pool and the non-engine tail of the
/// response (minify, compress, assemble), which no permit bounds.
const INTERACTIVE: Duration = Duration::from_secs(9);

/// `WorkspaceReport` and `AnalyzePackageJson`.
///
/// Both are explicitly raised to **300s** by the extension
/// (`extension/src/daemon/nativeTransport.ts`: `WORKSPACE_REPORT_TIMEOUT_MS` and
/// `PACKAGE_JSON_ANALYSIS_TIMEOUT_MS`): they size a whole workspace or a whole manifest, and a
/// bound that fits one document would fail them for being *big* rather than broken. The
/// package.json timer is additionally reset on every streamed partial, so 290s of total engine
/// time is a strictly tighter bound than the client's, which is what we want.
const LONG_RUNNING: Duration = Duration::from_secs(290);

/// The engine deadline a request carries.
///
/// Deliberately not a `Duration`: a duration would have to be re-based at every hand-off, and the
/// bug this closes is precisely time passing *between* hand-offs. An `Instant` stamped once is
/// the same answer no matter who asks or how long they took to ask.
#[derive(Debug, Clone, Copy)]
pub struct EngineBudget {
    /// `None` means *no client is waiting*: prewarm and background revalidation push their
    /// results whenever they are ready, so abandoning their builds would buy nobody anything.
    /// Their builds are still capped individually by `BUILD_TIMEOUT`, which is what keeps a
    /// parked one from holding a permit forever.
    deadline: Option<Instant>,
}

impl EngineBudget {
    /// Stamp the budget for an interactive request. Call this when the request *arrives*, not
    /// when a build is submitted — the queueing is the thing being bounded.
    pub fn interactive() -> Self {
        Self::expiring_in(INTERACTIVE)
    }

    /// Stamp the budget for a workspace report or a package.json analysis. Stamped **once** for
    /// the whole scan and shared by every file in it: the client's 300s covers the entire
    /// report, so a fresh budget per file would bound nothing.
    pub fn long_running() -> Self {
        Self::expiring_in(LONG_RUNNING)
    }

    /// Work no client is waiting on (prewarm, background SWR revalidation).
    pub fn background() -> Self {
        Self { deadline: None }
    }

    /// A budget that expires `remaining` from now.
    ///
    /// Public so a test can drive the boundary against a budget it can outlive in milliseconds
    /// instead of waiting out a real one; production stamps its budgets through the two named
    /// constructors above.
    #[doc(hidden)]
    pub fn expiring_in(remaining: Duration) -> Self {
        Self {
            deadline: Some(Instant::now() + remaining),
        }
    }

    /// How long one build may run, or `None` when the budget is spent and no build may start.
    ///
    /// `cap` is `BUILD_TIMEOUT`, the hard per-build limit; what is left of the request's budget
    /// can only make a build *shorter*, never longer. The boundary calls this twice for every
    /// build — once before queueing for a permit, once after acquiring one — because the time
    /// that passes in between is the entire failure this exists to bound.
    pub(crate) fn build_limit(self, cap: Duration) -> Option<Duration> {
        let Some(deadline) = self.deadline else {
            return Some(cap);
        };

        // `checked_duration_since` is `None` once the deadline has passed; a deadline reached to
        // the nanosecond yields zero, which is just as spent.
        let remaining = deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }

        Some(cap.min(remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The remaining budget shortens a build; it never lengthens one past the hard cap.
    #[test]
    fn a_build_is_capped_by_whichever_of_the_two_limits_is_shorter() {
        let cap = Duration::from_millis(500);

        let generous = EngineBudget::expiring_in(Duration::from_secs(60));
        assert_eq!(
            generous.build_limit(cap),
            Some(cap),
            "a budget with room to spare leaves the per-build cap in charge"
        );

        let tight = EngineBudget::expiring_in(Duration::from_millis(50));
        let limit = tight.build_limit(cap).expect("the budget still has time");
        assert!(
            limit <= Duration::from_millis(50),
            "a build must not be allowed to run past the request's deadline: {limit:?}"
        );

        assert_eq!(
            EngineBudget::background().build_limit(cap),
            Some(cap),
            "background work has no request deadline, only the per-build cap"
        );
    }

    /// A spent budget admits no build at all — the caller degrades instead of queueing.
    #[test]
    fn a_spent_budget_admits_no_build() {
        assert_eq!(
            EngineBudget::expiring_in(Duration::ZERO).build_limit(Duration::from_secs(8)),
            None,
            "a request that has spent its engine budget must not start another build"
        );
    }
}

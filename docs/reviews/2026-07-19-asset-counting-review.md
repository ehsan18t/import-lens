# Review: the non-JS asset counting feature (B2)

**Subject.** Commit `3da8d88` — "count the CSS, wasm and font bytes a package ships with Lightning CSS"
(PR #2), ~11,300 insertions across the daemon, the extension, the CLI and the oracle harness.

**Date.** 2026-07-19. Two passes: an initial review, then a full independent re-verification with fresh
agents that had not authored any finding.

**Outcome.** Everything the review found that showed a wrong number, could wedge, or fell below a
stated invariant has been fixed. What survives here is the part that is still true of the product: the
design risks nobody has acted on, the coverage that still does not exist, and the claims that were
adjudicated FALSE — kept so they are not rediscovered and "fixed" into a regression.

Fixed findings are not listed. They are in git, and each retired entry has a row in
[known-issues.md](../known-issues.md#resolved). Behaviours that were examined and deliberately left
alone live in `known-issues.md` as D24 and D28.

---

## 1. Design risks, still open

None of these is a defect today. Each is a claim about what the NEXT change to this area will get
wrong, which is why they are recorded rather than acted on.

### D-1 — The fallback ladder has no floor invariant *(weigh before the next asset feature)*

The promise is "never below pre-B2". It is implemented as a ladder — union bundle → per-sheet retry →
raw disclosure — where each rung independently reconstructs its own
`(read_paths, fingerprints, failed_paths, non_durable_stages)` and hands it upward, merged by hand at
five sites. Nothing in the type system requires a lower rung to carry at least what the rung above it
observed.

The completeness gate is a two-term boolean over independently-populated fields (`assets.rs`), and the
codebase has been bitten here before: a comment in place records that `css_dependency_omissions` was
originally routed to `imprecise_assets`, so `incomplete` never fired.

**What breaks and when.** The natural next features are all new rungs — a Sass/Less rung below
Lightning CSS, a partial-union rung that drops only the offending sheet, per-`@media` splitting. A rung
that forgets to propagate `failed_paths` yields a *complete-looking* measurement from a run that failed
to read a file, written durably as a baseline. The existing rungs are correct; the design does not make
the next one correct.

### D-2 — Freshness state is spread across seven structures

The `(read_paths, read_time_fingerprints)` pair — what decides whether a cached number is still true —
is declared independently on `TrackingProvider` (as two separate mutexes), `CssReadInputs`, `CssBundle`,
`BudgetState`, `ProcessedAssets`, `AssetBudgetFailure` and `AssetProcessingFailure`, and stitched by
`.extend()` at four sites. Adding an eighth discovery source means editing all four merge sites; missing
one drops a file out of freshness, which surfaces as a stale size and never as an error. This is
CLAUDE.md's "six correlated collections" shape applied to the thing the product's trust rests on.

### D-4 — The concurrency gate is not tested through the production path

The `#[cfg(test)]` module shadows both `executor()` and `execute()`, building a fresh `AssetExecutor`
per call, so the concurrency test proves only that *one* executor honours its own permits. No
integration test exercises the production entry, and there is no second gate upstream: the engine
releases its own permit *before* asset processing runs (`scheduling.rs`,
`MISS_DRAIN_WORKERS = ENGINE_PERMITS + 2`), so up to four workers can reach the asset gate at once.

*Partly mitigated.* A guard now asserts the production executor is shared, so de-globalizing it fails.
What remains untested is the permit count itself under real concurrency.

### D-5 — Three implementations of one reserve→read→reconcile protocol

`assets.rs` (`reserve`/`reconcile`), `asset_budget.rs` (`begin_css_read`/`finish_read`) and
`plugin.rs` (`try_reserve_source_bytes`/`release_source_bytes`/`reconcile_source_bytes`) each implement
the same protocol with the same stat-before-read rationale, and asset bytes are charged to the plugin's
too — so the *bytes* accounting crosses the load-hook seam even though the *semantics* correctly do
not. The alleged leak consequence was refuted (§3, R-1); the duplication is the finding, and it is the
exact class CLAUDE.md names from this repo's history.

### D-6b — `assets.rs` is three things

At ~1,590 production lines it holds the Lightning CSS adapter (which carries the `unsafe impl
Send/Sync` and a raw-pointer arena), the result model and disclosure taxonomy, and the orchestrator.
Cutting the adapter off is the high-value split: a reviewer of the disclosure taxonomy would never have
to reason about pointer lifetimes. Recorded, not acted on — a file split is churn without a forcing
function.

### D-7 — Seam placement is right (assessment, not a defect)

The load hook is the correct boundary and the coupling is one-directional: the bundler layer knows only
`AssetClass::{Counted(kind), Unmeasured}`. `asset_classifier.rs` records why interception must precede
the loader — a `.png` fails on `InvalidData`, an `.svg` is valid UTF-8 and gets parsed as JavaScript.
Classification is one pure function shared by both discovery boundaries, which is what stops a font
being caught on one path and missed on the other.

---

## 2. Coverage that still does not exist

- **Cycle canonicalization and the 256-file stylesheet bound are unit-tested only.** Both are refusal
  paths, and esbuild has no equivalent limit to agree or disagree with, so the accuracy oracle cannot
  cover them. The `@import` tree walk itself and shared-sheet dedup now ARE measured against the oracle.
- **The permit count under real concurrency** (D-4 above).
- **`docs/reviews/` has no harness run behind this review.** The module-by-module audit described in
  `docs/reviews/README.md` was not performed; this was a feature review, scoped to one commit.

---

## 3. Refuted claims — kept deliberately

Adjudicated FALSE. Recorded so they are not rediscovered and "fixed" into a regression.

- **R-1 — "The two ledgers leak a reservation and drift upward into a spurious terminal failure."**
  REFUTED. The shape is real (no `Drop`, the charge is additive) but the direction is inverted:
  `reconcile` errors *only* when the file grew, so the retained charge is `metadata_bytes`, strictly
  **smaller** than what was read. Confirmed by an executed search over 3,000,000 sampled states —
  500,485 error trials, **zero** in the over-charge direction. The residual effect is a marginally more
  permissive guard, and `css_work_bytes` reaches no user surface. Duplication survives as D-5.
- **R-2 — "The snapshot branch is missing a reconcile."** REFUTED, correct by construction. Reconcile
  replaces a metadata *estimate* with the actual length; a snapshot's bytes are exact at charge time,
  which is why `css_work_reserved_bytes` is `Some` for a disk read and `None` for a resource read.
- **R-3 — "A missing `url()` target is disclosed at a fabricated 0 bytes."** REFUTED — a 0-byte row
  renders as "of unknown size". The misclassification behind it was real and is fixed.
- **R-4 — "Admitting `imprecise_assets` into history is a defect."** REFUTED by the spec. FR-032a
  explicitly permits it, `stage.rs` repeats it, and a test in each language pins it with a comment
  forbidding durability and budgetability from being re-merged. Only the uncaveated delta was the
  defect.
- **R-5 — "The feature misses the dominant UI-kit shape."** REFUTED (the reviewer's own hypothesis). A
  bare `import "pkg/styles.css"` is fully supported end to end, and now has a test.
- **R-6 — "A production limit read from an environment variable violates the test-path rule."**
  REFUTED — `MAX_GRAPH_SOURCE_BYTES` is a test *limit*, which CLAUDE.md explicitly sanctions, because
  `#[cfg(test)]` cannot reach integration tests. The residual risk is only shell inheritance.
- **R-7 — "The 256-file stack-overflow guard cannot fire in production."** REFUTED, and the evidence
  behind the claim was misread. `begin_css_read` charges the ledger *before* `reserve` charges the
  256 bound, and both increment in lockstep, so the attempt bound hits 257 while the ledger sits at
  257/512. The "2N ≥ 512" arithmetic fails because a breaching union **aborts at read 257** rather than
  reading all N. The guard's test uses `AssetBudgetLimits::production()`, not `unbounded_css_work()`.
  Safety is intact and is delivered by the documented mechanism.
- **R-8 — "A >20 MB CSS-referenced font is a second entrance to the ledger-breach defect."** REFUTED as
  stated. The arm exists and fires, but the disclosure *is* constructed and dies later at the poisoned
  deadline check — not "before it can run" — and no real package ships a >20 MB `url()`-referenced
  font. It collapsed into the main finding.
- **R-9 — "A user's own `import 'pkg/font.woff2?url'` crashes the build."** REFUTED — the resolver
  rejects a query/fragment resolution with a clean scoped message.
- **R-10 — "CSS `url('./fa.woff2?v=4.7.0')` is affected."** REFUTED — `css_dependencies.rs` already
  strips `['?', '#']`. FontAwesome's shape was never broken.
- **R-11 — "Query-suffixed specifiers are ubiquitous in Vite projects."** REFUTED as a *reachability*
  claim: zero resolvable instances across two real project trees. `?url`/`?raw` is app-author
  vocabulary and app code takes the guarded path.

---

## 4. How much to trust this review

- Most findings were code traces. The exceptions, which were executed: R-1's sampled-state search,
  R-7's and R-8's `cargo test` runs, the Lightning CSS and `Path::has_root` probes behind two
  classification findings, oxc_resolver and filesystem probes for the loader-suffix chain, a cache-key
  test, a live `importlens check` repro, and rendered-output comparisons for the two surface findings.
- One diagnosis in this review was **wrong and acted on before it was checked**: a regression
  introduced during the fixes was attributed to Rolldown's proxy module ids, on inference rather than
  evidence. The real cause was a Windows verbatim path prefix (`\\?\`) being read as a loader query.
  Instrumenting the hook found it in two minutes. Treat any causal claim here that does not name its
  evidence as a hypothesis.
- The tree moved during the review (three commits, including a compiler-stack bump to rolldown 1.2.0 /
  oxc_resolver 11.24.2). Findings anchored in the changed files were re-verified at HEAD.
- Not reviewed as slices: `engine/adapter.rs`, `engine/scheduling.rs`, disk-cache compaction and
  eviction, and the webview report rendering — so FR-018c's *rendering* of module contributions is
  unverified beyond confirming assets reach the list that feeds it.

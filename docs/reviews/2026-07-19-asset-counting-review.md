# Review: the non-JS asset counting feature (B2)

**Subject.** Commit `3da8d88` — "count the CSS, wasm and font bytes a package ships with Lightning CSS"
(PR #2), ~11,300 insertions across the daemon, the extension, the CLI and the oracle harness.

**Date.** 2026-07-19. **Two passes:** an initial review, then a full independent re-verification with
fresh agents that had not authored any finding. Severities below are the post-verification ones; six
claims from the first pass were overturned and are recorded in §9 rather than deleted.

**Read this first — the tree moved during the review.** The review began at `3da8d88`; HEAD is now
`66e9b7d`, three commits later. `df9e5cd` bumped the compiler stack to **rolldown 1.2.0 / oxc_resolver
11.24.2** (the commit message for `3da8d88`, and the first pass of this review, refer to 1.1.5/11.23.0 —
those references are stale). `ANALYZER_REVISION` is now `rolldown-1.2.x+15`, not the `+12` that
`3da8d88` set, and the minor-line label was correctly carried across the bump. Everything below was
re-checked against current HEAD; findings whose anchors are in `plugin.rs`, `asset_classifier.rs`,
`resolver.rs` or `adapter.rs` matter here, because those files changed. The core asset modules
(`assets.rs`, `asset_budget.rs`, `css_dependencies.rs`, `file_size.rs`, the cache layer, the extension
analysis modules) did **not** change and their anchors are stable.

**Bar used for ranking.** This repo's own: a finding is release-blocking only if it (a) shows the user a
WRONG NUMBER, or (b) can WEDGE the system or lose data. Findings already recorded in
`docs/known-issues.md` (D7, D8, D9, D13, D14, D18, D19) were excluded as deliberate decisions. The
registry was re-read at HEAD after `known-issues.md` gained 89 lines mid-review; none of the new
entries covers anything below.

## Status: what has since been fixed

Seven fixes landed on `fix/asset-counting-review-findings` after this review. Each carries a test that
was **seen red first**, and the daemon changes bumped `ANALYZER_REVISION` twice (`+16`, `+17`).

| Finding | Outcome |
| --- | --- |
| A8, A6, A3, A16 | Fixed together — every `url()` now resolves to exactly one recorded outcome. |
| A2 | Fixed, narrowed to suffixes that reveal an asset (see D24 below for the rest). |
| A7 | Fixed — only a success is cached, enforced by the cell's type. |
| A10 | Fixed — strict freshness is chosen per fingerprint. |
| A5 | Fixed — the `unknown` visual is wired to the lookup. |
| A13 | Fixed — the CLI gained the missing `imprecise` axis; both surfaces claim neither direction. |
| A4 | Fixed — rows persist the flag and both delta labels caveat an upper bound. |
| D-4 | Fixed — a guard now asserts the production executor is shared. |
| A1, A12, A14, A9 | Recorded as **D23, D25, D26, D27** rather than fixed. |

The full reasoning for each deferral is in `known-issues.md`; the short version is that A1 has no
demonstrated trigger and shows no wrong number, A12 is an incomplete explanation of a correct number,
A14 costs one import one measurement in a narrow interleaving, and A9 pre-dates this feature.

One regression was caught during the work and is worth recording: the first version of the A2 fix
stripped loader suffixes unconditionally, which claimed Rolldown's own proxy and helper module ids and
broke six file-size tests. The narrowing to asset-classifying paths is why D24 exists.

## Verification levels

| Level | Meaning |
| --- | --- |
| **proven** | An independent adversarial pass tried to refute it and failed, with an end-to-end trace — in several cases with executed code or a live repro. |
| **plausible** | Mechanism proven link by link, but no demonstrated triggering input. |
| **lead-confirmed** | I read the code and confirmed it directly, re-checked at current HEAD. |
| **refuted** | Adjudicated false. Kept in §9 with the reasoning. |

Deterministic gates were green throughout: `tsc --noEmit`, `cargo fmt --check`, and the full Rust +
TypeScript + scripts suites. No finding below is gate-catchable, which is itself the point of §7.

---

## 1. Meets the fix-now bar

### A8 — A non-UTF-8 percent-escaped `url()` is dropped, silently, at High confidence
**proven** (verified by executing Lightning CSS at the pinned version). **The only confirmed wrong
number in this review.**

`url("Ubuntu-R%E9gular.woff2")` — a latin-1/CP-1252 `é`, what a design-tool export on a Windows-1252
system produces. Lightning CSS hands the string through **undecoded** (confirmed by execution), so
`String::from_utf8(decoded).ok()` at `css_dependencies.rs:255` yields `None`, `resource_path` returns
`None`, and `collect_referenced_assets` falls into a `None => {}` arm (`:84-86`) whose comment asserts
the only causes are a `data:` payload or a bare fragment — which is now false.

Why this is the worst finding here despite being the rarest. The reference leaves **no trace at all**:
nothing enters `omissions`, `uncounted` or `external`, so `has_uncounted_assets()` stays false,
`incomplete` stays false, and no diagnostic is emitted. The result is therefore fully **Measured at High
confidence**. The short total is cached, written to the no-TTL history as that file's baseline, and
judged against a budget. It never enters freshness fingerprints either — so **supplying or fixing the
font never invalidates it**. A whole font face (typically 15-100 kB) vanishes from the number with zero
user-visible signal.

Reachability is the honest weakness: a plain `url("Ubuntu-Régular.woff2")` in a UTF-8 sheet decodes
fine, and only legacy CP-1252 escapes hit this. The verifier could name the shape but not a specific
shipped package. That lowers priority, not validity — this is the only path in the module that produces
a silently short total, and it violates the module's own contract at `css_dependencies.rs:14-17`
("either counted, or disclosed with its bytes, or named as an omission — never dropped").

### A13 — Two surfaces make opposite directional claims about the same number
**proven** (reachability established by a shipped test; divergence executed).

When a result is both short and imprecise, single-branch precedence makes each surface pick one axis and
discard the other — and they pick differently:

- Extension: `fileCostQuantityName` returns **"File Cost floor"** — the true cost is *higher*.
- CLI: `unmeasurableLine` prints **"asset processing produced a disclosed upper bound"** — the true cost
  is *lower*.

Reachability is not theoretical. The per-sheet retry's stated purpose is to let one `.scss` fail while
others count (`assets.rs:1320-1358` returns `degraded` with `uncounted` non-empty), and the shipped
green test `one_unparseable_stylesheet_does_not_sink_the_rest_of_the_set` (`assets.rs:2022-2065`)
already constructs exactly that shape. `asset_diagnostics` then emits both stages from one
`ProcessedAssets`.

The clinching detail: **the code violates a rule written in its own file.**
`extension/src/analysis/fileCostQuality.ts:44-46` says "Folding an over-count into a floor would tell
the user the true cost is higher when it is lower." That is precisely what the `short`-before-`imprecise`
precedence at `:89-93` and `:121-125` does.

The existing drift check (`scripts/test/file-size-usability-coordination.test.mjs:184-200`) asserts only
that the CLI *contains* each sentence. Both sentences exist in both files, so it stays green and cannot
catch this.

### A4 — A history delta computed against a stored upper bound
**proven.** The verifier split this, and the split matters.

**The write is correct — do not "fix" it.** SRS FR-032a (`SRS:717`) states that `imprecise_assets` "is
deterministic and **may enter caches/history**, while budget gates must reject it". That divergence is
pinned by tests in both languages whose comments forbid re-merging the predicates.

**The delta is the defect.** `fileSize.ts:147-151` gates on `previous && current` only, and
`BundleImpactHistoryItem` (`history.ts:20-29`) stores a timestamp, a filename and five byte counts —
**no diagnostics field** — so re-validation is structurally impossible, not merely absent. Labelling is
asymmetric: imprecise-as-*current* is captioned; imprecise-as-*previous* with a sound current renders
`-25.0 kB br vs previous` with no caveat. The import axis is worse — `insights.ts:191-208` checks
neither side and has no caption at all.

Blast radius: both stores have no TTL and no revision key (zero `ANALYZER_REVISION` references in
`extension/src`); rows replace per file/identity. Only a manual key-suffix bump clears them, and
`history.ts:12-16` shows that remedy was already spent once (v2→v3) on the floor axis.

*Correction to the first pass:* it claimed "a saving that never happened". Flipping the union outcome
requires an input change, so the delta is **contaminated** (a real change plus the over-count
evaporating), not fabricated. Still wrong, but not invented.

The root asymmetry: `uncounted_assets` is caught structurally by the `incomplete` wire flag;
`imprecise_assets` has no corresponding flag. That is what leaves this axis open.

### A10 — A stale number served indefinitely, and the feature caused it
**proven** (routing established by the project's own executed test).

`cache_key_is_first_party` reads only `identity.entry_path`, so a `node_modules` entry sends its
**entire** fingerprint set to the non-strict check, and `check_fingerprints_strict`'s per-fingerprint
routing at `key.rs:550` is never reached. The pre-filter at `key.rs:493-496` is a genuine short-circuit
to `Fresh`, not a staleness-only fast path — the stored content hash at `:500-507` is consulted *only*
when len/mtime already differ.

The asset feature is what makes this reachable: a `url()` may resolve outside the package root (D18,
accepted), so `node_modules/ui-kit/dist/styles.css` referencing `../../../shared/fonts/Inter.woff2`
places a **workspace** file into a node_modules entry's fingerprint set. The hash that would catch a
change is computed and stored, and then never consulted.

**This is not inherited exposure.** The non-strict choice is justified in-comment by "node_modules deps
change only behind a NodeModulesChanged generation bump" (`memory.rs:283-285`) — a workspace file in
that set **falsifies the stated premise**. The feature widened it.

Duration: **indefinite.** Within TTL the entry skips checking; after TTL the non-strict check returns
Fresh and *restamps*, re-arming the window; a generation bump evicts nothing; the disk entry rehydrates
non-strict, so a daemon restart does not clear it. Cleared only by an analyzer-version change, an
explicit cache clear, eviction, or a later edit that moves len or mtime.

Trigger is narrow — a same-length, mtime-preserving replacement (`cp -p`, `rsync -a`, tar/CI cache
restore). `git checkout` does **not** qualify; it stamps a current mtime.

The repo's own test already documents the routing (`key.rs:1259-1268`: "the Fresh result below comes
from the routing choice, not from the fixture failing to change"). The routing is known; its new
consequence is not.

### A2 — A query-suffixed specifier fails the build, and the failure is cached
**proven for one route; two claimed routes refuted.** Re-verified against the versions actually
compiled (rolldown 1.2.0 / oxc_resolver 11.24.2), with executed output at both ends.

oxc_resolver re-appends the query in `full_path()` (executed: `./font.woff2?url` →
`full_path()=...\font.woff2?url`); rolldown_resolver builds `ResolvedId.id` from it; the `load` hook
classifies without stripping, so `Path::extension()` yields `"woff2?url"` and `classify_asset_class`
returns `None`; no `Empty` stub is emitted; Rolldown reads the id verbatim. Executed: `fs::metadata` on
that exact string returns `InvalidFilename` / `ERROR_INVALID_NAME` on Windows. A grep for query-stripping
across `rolldown-1.2.0`, `rolldown_plugin-1.2.0` and `rolldown_common-1.2.0` returned **zero hits** — no
interception exists anywhere.

The failure lands in `resolve` or `LINK`, both in `DURABLE_RESULT_STAGES`, so it is **cached**: the
package stays unmeasurable until its bytes change.

Two refutations narrow this sharply, and one correction widens it:

- **A user's own `import x from 'pkg/font.woff2?url'` is REFUTED** — `resolver.rs:1152-1156` rejects a
  resolution carrying a query or fragment with a clean scoped message. App code never reaches the crash.
- **CSS `url('./fa.woff2?v=4.7.0')` is REFUTED** — `css_dependencies.rs:203-213` already strips
  `['?', '#']`. The genuinely ubiquitous shape (FontAwesome) is handled.
- **My "ubiquitous in Vite projects" framing was wrong.** `?url`/`?raw` is app-author vocabulary; the
  verifier found zero resolvable query-suffixed specifiers across two real project trees. The only live
  route is a **published package's own internal** `./x.woff2?url`, which is rare.
- **Wider than assets, though:** `./Component.svelte?raw` or `./data.json?raw` fail identically, since
  `supported_asset_observation_candidate` returns `None` and Rolldown still produces the queried id.

It stays on this list because the consequence is a permanently unmeasurable, *cached* package. The
codebase already applies the correct idiom at three boundaries (`plugin.rs:330-343`,
`css_dependencies.rs:212`, `resolver.rs:1152`) and misses exactly one (`plugin.rs:802`) — the signature
of an oversight, not a decision. Test coverage is zero; the only `?url` in the repo asserts the
resolve-side helper and never follows it into `load`.

---

## 2. Confirmed, below the fix-now bar

### A1 — A ledger breach discards the JavaScript measurement
**plausible** — every mechanical link proven, no demonstrated trigger. **Downgraded from the first
pass, where it was ranked first.**

The asset ledger seeds `unique_files` from the JS graph (`asset_budget.rs:130-134`) and `reserve_unique`
(`:493`) tests that same map against `max_unique_files = MAX_GRAPH_MODULES` = 2000 — the same constant
and the same population the JS graph is capped by. Real headroom is `2000 - |loaded_paths|` and can be
zero. `assets.rs:1361-1363` then converts a live context failure into `Err`; no caller on the import
path degrades it (`file_size.rs:597-631` *does* degrade, but that is the File Cost aggregate);
`analyze.rs:417-422` propagates to `ImportResult::unmeasured`. `MODULE_GRAPH_LIMIT` is durable, so the
verdict is cached. Pre-`3da8d88` the same package measured its JS and disclosed the CSS, so SRS
FR-018a's "never below the pre-B2 behaviour" is violated.

Three corrections from the fresh pass:

- **My description was wrong.** The stylesheets are *not* successfully measured — `record_limit` poisons
  `check_available`, so `bundle_collected_css` dies at its next checkpoint and `bundled` is `Err`. What
  is actually discarded is the complete **JavaScript** measurement, computed before the asset call.
- **No trigger exists.** It needs a package landing in `(2000-K, 2000]`; above 2000 the build already
  fails identically. Neither verifier could find or construct one, though `candidate_matrix.rs:776`
  shows the band is constructible.
- **It does not meet the bar.** The user sees "Size unavailable" instead of a JS number plus disclosure.
  That is deterministic given the bytes, so caching it durably is self-consistent — not the ADR-0006
  "transient cached durably" disease. Neither a wrong number nor a wedge.

Real, and a genuine spec-conformance regression. Queue work, not a blocker.

### A16 — A poisoned context silently drops later `url()` resources
**proven** (surfaced while refuting A1's second entrance). Once the context is poisoned,
`should_continue_dependency_reads` (`assets.rs:301`) is `check_deadline().is_ok()`, so the loop at
`css_dependencies.rs:64-66` breaks and every *later* `url()` resource vanishes — **without even an
omission record**. Another breach of the never-dropped contract, on the failure path.

### A3 — A missing `url()` target is misclassified as transient I/O
**proven**, with one sub-claim refuted.

`CssDependencyFailure` carries no `ErrorKind`, so the caller cannot distinguish `NotFound` from a
permission error and hardcodes `missing: false` (`assets.rs:1417`, a literal). The result takes
`unverifiable_file_fingerprint` — `fingerprints_are_reusable()` is false forever, so the package rebuilds
on every keystroke — and an `asset_io` diagnostic tells the user the result "reflects a changing or
unavailable filesystem" about a stable package fact. The `@import` half gets this right
(`assets.rs:279-285` → `absent_file_fingerprint`), and `cache/key.rs:459` states the intent verbatim.

**Fixing line 1419 alone will not work:** `assets.rs:1397-1398` sets the `asset_io` stage via
`|| !bundle.referenced_failures.is_empty()`, unconditionally. Two independent causes.

*Refuted sub-claim:* the "fabricated 0 bytes" disclosure. A 0-byte row renders as "of unknown size" via
`engine/mod.rs:94-102`; no fake zero reaches a user.

This is the **third** occurrence of this bug class (D21 closed one surface, `assets.rs:468-479` records
another). Before patching site three, the question worth asking is whether `CssDependencyFailure` should
carry the `ErrorKind` — which closes the class instead of the instance.

### A11 — `importlens check` abandons remaining files and drops found violations
**proven by live repro.** Real daemon, three files, killed mid-run: `files attempted = 2 of 3`,
`lines written = []`, exit 2 — with file 1 a genuine violation, silently dropped.

`defaultIpcTimeoutMs = 10000` applies because `:335` passes no timeout; there is no `try`/`catch` in
`analyzeFileWithDaemon` or around the per-file loop; `violations` prints only at `:180-182`, after the
loop; `main` has no `catch`, so exit is 2 rather than the mandated `EXIT_COULD_NOT_MEASURE = 3`.

This violates a line in your own spec (`SRS:725`): "It must not throw: throwing abandons every other
file's budget mid-run and exits 2, which means 'the CLI broke'." The repo's other blocking client picks
60 s (`accuracy-compare.mjs:959`). The one unproven link — a >10 s slow build — is not load-bearing,
since an ordinary mid-run transport failure produces all three consequences.

### A5 — An unrecognized confidence value destroys the whole hover
**lead-confirmed**, re-checked at HEAD.

`confidenceVisualFor` (`confidenceVisuals.ts:54`) is an unguarded record lookup;
`tooltipMarkdown.ts:176` passes the raw wire value and `:182` reads `.badge`. A newer daemon adding a
fourth `ConfidenceLevel` yields `undefined` → `TypeError` → the hover renders **nothing**.

The fix is one line, and the fallback already exists: `ConfidenceTone` is
`ConfidenceLevel | "unknown"` (`:3`) with an `unknown` visual in the record (`:43-51`) — it was built and
never wired. The sibling asset-kind lookup does it correctly (`format.ts:45`,
`assetKindLabels[kind] ?? kind`), and its comment states the exact rule this path violates.

### A7 — A transient thread-pool failure permanently disables asset counting
**lead-confirmed.** `asset_boundary.rs:197-203` stores `Result<AssetExecutor, _>` in a `OnceLock`. The
`Ok` is legitimately once-only; the `Err` is a *machine state* (thread exhaustion, `RLIMIT_NPROC`, a
Windows handle spike) and caching it means every later request for any asset-bearing package returns
Unmeasured for the daemon's lifetime. Only a restart clears it.

### A6 — A protocol-relative CDN resource is disclosed as a missing local file
**proven** (Lightning CSS and `Path::has_root()` both executed on Windows). **Severity corrected
downward.**

`has_url_scheme` uses `split_once(':')`, which finds no colon in `//fonts.gstatic.com/...`, so the
External arm is skipped; `has_root()` is true on both platforms, so the reference becomes `Omitted`. The
`@import` sibling handles the identical form (`assets.rs:431-434` tests `starts_with("//")`), so
`@import "//cdn/x.css"` stays exact and budgetable while `url("//cdn/x.woff2")` does not.

**The byte total stays correct** — a CDN font is not shipped bytes either way. What breaks is that an
exact number is *labelled* a floor, loses its pass/fail verdict, and is refused from cache and from the
durable history baseline. A wrong qualifier, not a wrong figure. (The first pass called this a wrong
number; that was wrong.)

### A14 — A lost wakeup produces a spurious admission timeout
**proven**, with a legal interleaving. The loop-top exit at `asset_boundary.rs:93-99` returns before any
`Permit` is constructed, so there is no `Drop` and no re-notify; `release()` uses `notify_one`; and
`:107-108` discards the `WaitTimeoutResult`, so a notification-driven wake is indistinguishable from a
timeout and is dropped when the deadline has passed. W1 consumes the wake and exits; W2 sleeps to its own
deadline with a permit free the whole time.

No permit is leaked and the user gets a disclosed fallback, so this is neither a wrong number nor a
wedge.

### A12 — The status bar cannot say what its number is made of
**proven**, lowest priority of the confirmed set. `listener.ts:447` passes only
`quality: fileCostQuality(response)`, and the state type carries no asset field, so `asset_breakdown` is
discarded at that boundary. Executed: the status bar renders `IL: ~40.0 kB br` with a tooltip that never
mentions composition, while the on-demand command shows `... · CSS 12.3 kB`. Clicking the item routes to
`importLens.showLogs`, so there is no affordance either. The import hover and package.json hover both
satisfy FR-018c; this surface does not.

Nothing false is displayed — an incomplete explanation of a correct number.

---

## 3. Out of scope (real, but not this feature)

### A9 — The File Cost cache omits package manifests
**proven**, and **pre-existing.** The File Cost freshness set carries no manifests while the per-import
path hashes them via `first_party_manifests`, so editing a transitive first-party dep's `package.json`
leaves File Cost stale while the per-import number updates. Flipping `sideEffects` genuinely moves bytes
(`resolver.rs:1191`), and the watcher globs (`**/node_modules/*/package.json`) do not fire for a real
path under `packages/`.

Two bounds: the window is **≤30 s** and self-heals — `computed_at_millis` is stamped once at insert and
a hit updates only `last_used_millis`, so polling cannot extend it. And the asymmetry **predates the
asset feature**, which added `freshness_fingerprints()` to both paths symmetrically. Recorded here for
completeness; it belongs in your queue, not on this feature's charge sheet.

---

## 4. Design findings

### D-1 — The fallback ladder has no floor invariant *(the one to weigh before the next asset feature)*

The promise is "never below pre-B2". It is implemented as a ladder — union → per-sheet retry → raw
disclosure — where each rung independently reconstructs its own
`(read_paths, fingerprints, failed_paths, non_durable_stages)` and hands it upward, merged by hand at
five sites. Nothing in the type system requires a lower rung to carry at least what the rung above saw.

The completeness gate is a two-term boolean over independently-populated fields (`assets.rs:899`), and
the codebase has already been bitten here: `assets.rs:859-862` records that `css_dependency_omissions`
was originally routed to `imprecise_assets`, so `incomplete` never fired.

The natural next features are all new rungs — a Sass/Less rung below Lightning CSS, a partial-union rung,
per-`@media` splitting. A rung that forgets to propagate `failed_paths` yields a *complete-looking*
measurement from a run that failed to read a file, written durably as a baseline. Note that A8 and A16
are both live instances of exactly this shape, and A4's missing `imprecise` wire flag is its sibling.

### D-2 — Freshness state is spread across seven structures

The `(read_paths, read_time_fingerprints)` pair — what decides whether a cached number is still true —
is declared independently on `TrackingProvider` (two separate mutexes), `CssReadInputs`, `CssBundle`,
`BudgetState`, `ProcessedAssets`, `AssetBudgetFailure` and `AssetProcessingFailure`, and stitched by
`.extend()` at four sites; `process_assets` merges three sources in thirty lines. Missing one merge site
drops a file out of freshness, which surfaces as a stale size and never as an error. This is CLAUDE.md's
"six correlated collections" shape applied to the thing the product's trust rests on.

### D-4 — The concurrency gate is never tested through the production path
**survives verification, and got stronger.**

The `#[cfg(test)]` module shadows both `executor()` and `execute()`, building a fresh `AssetExecutor` per
call, so `asset_processing_never_exceeds_two_concurrent_jobs` proves only that *one* executor honours its
own permits. The verifier found no integration test on the production entry
(`daemon/tests/asset_freshness.rs` calls it three times sequentially, no `thread::spawn`), and — the new
part — **no second gate upstream**: the engine's own permit is released *before* asset processing runs
(`scheduling.rs:15-20`, `MISS_DRAIN_WORKERS = ENGINE_PERMITS + 2`), so up to four workers can reach the
asset gate concurrently. The singleton is the sole bound on asset concurrency, and de-globalizing it
would leave every test green.

Contrast the correct pattern elsewhere: `compress_bundle_with` and `process_binary_kind_with` inject a
`&dyn Fn` — test *data* through the production path.

### D-5 — Three implementations of one reserve→read→reconcile protocol

`assets.rs` (`reserve`/`reconcile`), `asset_budget.rs` (`begin_css_read`/`finish_read`) and
`plugin.rs:310-345` each implement the same protocol with the same stat-before-read rationale; asset
bytes are charged to the plugin one too, so the *bytes* accounting crosses the load-hook seam even though
the *semantics* correctly do not. The alleged leak consequence was **refuted** (§9, R-1); the duplication
is the finding — the exact class CLAUDE.md names from this repo's history.

### D-6 — Smaller design items

- **`absorb_asset_breakdown` is implemented twice** (`file_size.rs:236-253` and `:828-843`), both
  hand-enumerating five size fields. Add a sixth metric to one and the breakdown rows stop summing to the
  total beside them.
- **`assets.rs` is three things** (~1,590 production lines): the Lightning CSS adapter (68-830, carrying
  the `unsafe impl Send/Sync` and a raw-pointer arena), the result model and disclosure taxonomy
  (831-1,099), and the orchestrator (1,101-1,592). Cutting the adapter off means a reviewer of the
  disclosure taxonomy never reasons about pointer lifetimes. `file_size.rs` is two things and is closer
  to justified.
- **Speculative surface:** `ProcessedAssets::is_empty` (no callers), `snapshot_if_present`
  (`pub(crate)`, called only in its own module), `classify_asset` (single caller that filters to
  `Wasm | Font`, making its `Css` arm unreachable at that call site).
- **`diagnostic_stage` has no `ALL`**, so it sits outside the property test `engine_stage` enjoys
  (`stage.rs:188`) — including both stages this feature routes new channels through. The `stages!` macro
  closes this with an existing mechanism.
- **`uncounted_assets` is a many-to-one channel** — two producers, different `details` semantics. Safe
  today because every consumer uses `.any(...)`; the first `.find(...)` loses half the disclosure.

### D-7 — Seam placement is right (assessment)

The load hook is the correct boundary and the coupling is one-directional: the bundler layer knows only
`AssetClass::{Counted(kind), Unmeasured}`. `asset_classifier.rs:24-28` records why interception must
precede the loader (a `.png` fails on `InvalidData`; an `.svg` is valid UTF-8 and gets parsed as
JavaScript). Classification is one pure function shared by both discovery boundaries, which is what stops
a font being caught on one path and missed on the other.

---

## 5. What the design got right

Recorded because it is load-bearing and easy to lose in a later refactor:

- **The omission/over-count split is structurally enforced, not conventional** — separate fields,
  triggers and diagnostics, with the bug that forced the split recorded in place. Verification confirmed
  `imprecise_assets` being durable-but-not-budgetable is spec'd, tested in both languages, and correct.
- **The four-constructor duplication was deleted, not added beside.** `assets.rs:160-169` names what it
  replaced, and the `Option<Context>` escape hatch that made production safety bypassable is gone.
- **New disclosure channels reuse existing stages** rather than minting new ones, each justified in
  comment.
- **`asset_diagnostics` takes the whole list**, so a caller cannot fold in bytes and forget a disclosure.
- **The stack-overflow guard works as documented** — and my first pass was wrong to doubt it (§9, R-7).
  The 256-file bound fires first because `begin_css_read` and `reserve` increment in lockstep.
- **The oracle harness is unusually disciplined**: a refactor guard that fails if the CSS fixture ever
  drops its `import`, the minified axis held because brotli cannot gate what was counted, and a comment
  recording that the fixture "would have stayed green with asset counting deleted outright".
- **The brotli 4→9 decision is costed, not asserted** — 16.0% vs 7.5% high, +33 ms, with q11 rejected at
  +928 ms.
- **Asset-kind forward compatibility is done correctly** (`format.ts:45`; `asset-kind-contract.test.mjs`
  pins the union to the Rust enum by derivation) — which is what makes A5 look like an oversight.
- **The README's floor/upper-bound table** explains both directions in user language.
- **`ANALYZER_REVISION` discipline held across the mid-review compiler bump** — `df9e5cd` correctly
  moved the minor line to `rolldown-1.2.x+14` rather than leaving a stale label on a changed engine.

---

## 6. Performance

Neither is a wrong number or a wedge. **No wall-clock measurement was taken for either**, so no magnitude
is claimed — only what is countable from the code.

- **P-1 — redundant `canonicalize`/`stat` per `url()` reference.** Counts verified exact: 3 canonicalize
  + 2 stat for a first-time counted resource (`css_dependencies.rs:156-157`, then `asset_budget.rs:291`,
  `:259`, `:265`). Dedup happens *after*, so 50 references cost 50 canonicalize + 50 stat unconditionally,
  re-run for the union and again for each per-sheet retry. `plugin.rs:63-66` documents that
  `canonicalize` is a file-handle open on Windows and memoizes it for exactly this reason; this path has
  no memo. *Two corrections to the first pass:* repeat references to an already-snapshotted file
  short-circuit to 2 syscalls, and `Uncounted` kinds (`.svg`, `.png`) return early and are the **cheaper**
  path, not an aggravator.
- **P-2 — the load hook clones every JS module's buffer.** `plugin.rs:994` does
  `String::from_utf8(bytes.clone())` purely to keep the bytes for hashing, while `:998` passes `&bytes` to
  `record_read_time`, which needs only a slice. Reordering those two lines removes the clone. This arm is
  the fall-through for every ordinary JavaScript module. (Re-confirmed at current HEAD after `plugin.rs`
  changed by 269 lines.)

---

## 7. Coverage and process gaps

- **C-1 — the `@import` tree walk has zero independent-oracle coverage, and the oracle says so.**
  `accuracy-compare.mjs:34-39`: the CSS fixture's reachable graph contains **zero** `@import` statements,
  so "the `@import` tree walk, the cycle canonicalization and the synthetic entry — the most intricate
  part of B2 — are covered by the unit tests in `assets.rs`, and by nothing here."
- **C-2 — the dominant real-world shape works but is untested.** A bare
  `import "bootstrap/dist/css/bootstrap.min.css"` is detected (no extension filter anywhere), becomes
  `ImportKind::Namespace` rather than a zero-binding short-circuit, resolves through `exports`, and is
  counted as a `css` asset; wasm and fonts behave identically. This is how most UI kits actually ship CSS
  and is the strongest evidence the scope call was right — but no test exercises it.
- **C-3 — no test covers a query-suffixed specifier** (A2). The only `?url` in the repo asserts the
  resolve-side helper and never follows it into `load`.
- **C-4 — the design document was deleted by the commit that shipped it.**
  `docs/asset-counting-design.md` exists only in `3da8d88^`. Removing shipped plans is your stated
  policy, so this is not a rule break — but three "Open questions to settle at implementation" went with
  it: (1) cross-sheet rule dedup, answered only for the degraded path via D8/D9; (2) the binary shipping
  model, which the commit message *does* answer (16,435 vs esbuild's 16,480); (3) per-runtime
  interaction, which I did not find resolved anywhere. Worth landing (1) and (3) in the SRS.
- **C-5 — an env-inheritable production ceiling** (recorded for accuracy, **not** a rule break).
  `MAX_GRAPH_SOURCE_BYTES` reads `IMPORT_LENS_MAX_GRAPH_SOURCE_BYTES` in shipped builds.
  `limits.rs:11-22` justifies it correctly as a test *limit*, not a test *path*, since `#[cfg(test)]`
  cannot reach integration tests. Residual risk is only shell inheritance.
- **C-6 — the guard test does not discriminate which bound fired.**
  `a_stylesheet_tree_past_the_file_budget...` asserts `error.contains("limit")`, which both limit
  messages satisfy. Test-precision nit, surfaced while refuting R-7.

---

## 8. Suggested order

Re-ordered after verification. A1 is no longer first.

1. **A8** — the only confirmed wrong number. Rare, but silent, cached, budgeted and never invalidated.
2. **A13** — two surfaces asserting opposite directions about one number, with reachability already
   proven by a shipped test, against a rule the file itself states.
3. **A5 + A7** — one-liners with disproportionate consequences (`?? confidenceVisuals.unknown`; do not
   cache the `Err`).
4. **A3 + A6 + A16 together** — all are `css_dependencies.rs` losing information the caller needs (the
   `ErrorKind`, the protocol-relative scheme, the resources after a poisoning). Consider whether
   `CssDependencyFailure` carrying the `ErrorKind` closes the class; this is the third occurrence.
5. **A4** — needs a decision, not a patch: either persist enough quality to caveat the delta, or refuse
   to render one whose baseline was an upper bound. Do **not** stop writing the row.
6. **A2** — strip query/fragment in the load hook, mirroring the three boundaries that already do.
7. **A10, A11** — the indefinite staleness and the CLI abandonment; both violate stated invariants.
8. **A1, A12, A14** — real, below the bar, queue normally.
9. **D-4** — route the concurrency test through the production `executor()`.
10. **D-1** — weigh before the *next* asset feature, not now. A8 and A16 are live instances of it.

---

## 9. Refuted claims (kept deliberately)

Adjudicated false. Recorded so they are not rediscovered and "fixed" into a regression.

- **R-1 — "The two ledgers leak a reservation and drift upward into a spurious terminal failure."**
  REFUTED. The shape is real (no `Drop`, the charge is additive) but the direction is inverted:
  `reconcile` errors *only* when the file grew, so the retained charge is `metadata_bytes`, strictly
  **smaller** than what was read. Confirmed by an executed search over 3,000,000 sampled states —
  500,485 error trials, **zero** in the over-charge direction. The residual effect is a marginally more
  permissive guard, and `css_work_bytes` reaches no user surface. Duplication survives as D-5.
- **R-2 — "The snapshot branch is missing a reconcile."** REFUTED, correct by construction. Reconcile
  replaces a metadata *estimate* with the actual length; a snapshot's bytes are exact at charge time,
  which is why `css_work_reserved_bytes` is `Some` for a disk read and `None` for a resource read.
- **R-3 — "A missing `url()` target is disclosed at a fabricated 0 bytes."** REFUTED — renders as "of
  unknown size". The rest of A3 stands.
- **R-4 — "Admitting `imprecise_assets` into history is a defect."** REFUTED by the spec (FR-032a),
  repeated at `stage.rs:169-170`, pinned by a test in each language whose comment forbids re-merging
  durability with budgetability. Only the delta (A4) is the defect.
- **R-5 — "The feature misses the dominant UI-kit shape."** REFUTED (my own hypothesis). Bare
  `import "pkg/styles.css"` is fully supported end to end.
- **R-6 — "A production limit read from an env var violates the test-path rule."** REFUTED — a test
  *limit*, explicitly sanctioned. Retained as C-5.
- **R-7 — "The 256-file stack-overflow guard cannot fire in production."** REFUTED, and my evidence was
  misread. `begin_css_read` (ledger) charges *before* `reserve` (the 256 bound) and both increment in
  lockstep, so the attempt bound hits 257 while the ledger is at 257/512. The "2N ≥ 512" arithmetic fails
  because a breaching union **aborts at read 257** rather than reading all N. And the guard's test uses
  `AssetBudgetLimits::production()`, **not** `unbounded_css_work()` — executed green at production
  limits. Safety is intact and delivered by the documented mechanism. Only C-6 survives.
- **R-8 — "A >20 MB CSS-referenced font is a second entrance to A1."** REFUTED as stated. The arm exists
  and fires (proven by executed test), but the `UncountedAsset` disclosure *is* constructed and dies
  later at the poisoned deadline check — not "before it can run" — and no real package ships a >20 MB
  `url()`-referenced font. It collapses into A1. Refuting it surfaced A16.
- **R-9 — "A user's own `import 'pkg/font.woff2?url'` crashes the build."** REFUTED —
  `resolver.rs:1152-1156` rejects a query/fragment resolution with a clean scoped message.
- **R-10 — "CSS `url('./fa.woff2?v=4.7.0')` is affected."** REFUTED — `css_dependencies.rs:203-213`
  already strips `['?', '#']`. FontAwesome's shape is handled.
- **R-11 — "Query-suffixed specifiers are ubiquitous in Vite projects."** REFUTED as a *reachability*
  claim: zero resolvable instances found across two real project trees. `?url`/`?raw` is app-author
  vocabulary and app code takes the guarded path. A2 survives only via published-package-internal usage.

---

## Coverage limits of this review

Stated so the green areas are not read as cleared:

- **The tree moved mid-review** (three commits, including a compiler-stack minor bump). Findings anchored
  in `plugin.rs`, `asset_classifier.rs`, `resolver.rs` and `adapter.rs` were re-verified at HEAD; the
  core asset modules did not change. First-pass version references (rolldown 1.1.5, oxc_resolver 11.23.0)
  are stale — the compiled versions are 1.2.0 and 11.24.2.
- Most findings are code traces. The exceptions, which *were* executed: R-1's sampled-state search, R-7's
  and R-8's `cargo test` runs, A6's and A8's Lightning CSS runs, A2's oxc_resolver and filesystem probes,
  A10's cache-key test, A11's live CLI repro, and A12/A13's rendered-output comparisons.
- No performance claim carries a measurement. P-1 and P-2 assert syscall and allocation counts only.
- Test bodies were read for intent, not audited, except where a test *is* the finding (D-4, C-6).
- Not reviewed as slices: `engine/adapter.rs`, `engine/scheduling.rs`, disk-cache compaction and
  eviction, the webview report rendering (so FR-018c requirement 2's *rendering* is unverified — only
  that assets reach the contribution list feeding `module_breakdown`), and `insights.ts` beyond the
  history delta.
- Lens coverage over the assigned surface was complete: code-defect on every slice, spec-conformance
  wherever the spec mapped onto a slice, design-critique once over the seams. No lens×slice pair was
  silently dropped.

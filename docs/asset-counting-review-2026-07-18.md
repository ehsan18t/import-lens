# Asset-counting review: design, implementation, and cost

- **Reviewed:** 2026-07-18
- **Branch:** `bundler-b2-asset-counting` (26 commits ahead of `main`)
- **Scope:** the asset-counting design, the shipped implementation including the AC-01..AC-04 fixes, and the
  size of what was built.
- **Gate status:** `pnpm test` green (exit 0, all Rust + TypeScript + script suites). Every finding below is
  something no deterministic gate catches.

Each finding carries a **verification level**: `proof-confirmed` (adversarially verified against the code,
survived), `refuted` (adjudicated false — kept and marked, not silently dropped), `finder-claim` (reported by
one reviewer, not independently verified), `already-recorded` (an accepted limit in `known-issues.md`).

---

## Executive summary

The core architecture is right and the AC-01..AC-04 fixes are real. Two reviewers independently validated the
things most likely to have been faked: asset dedup by canonical path, metadata reservation before read, the
deadline covering admission, and an abandoned worker retaining its permit. Those hold as written.

Two defects produce a **wrong number the user is never told about**, and both are new — neither is a
restatement of D7-D12:

1. **A1** — an image or SVG referenced from a *counted* stylesheet is silently dropped: not counted, not
   disclosed, result stays **High confidence**, and because it never reaches `uncounted_assets` the result is
   also cacheable *and* budget-evaluated.
2. **A2** — one ordinary CSS custom property containing a `url()` disables `url()` discovery for **every sheet
   in the union**, and the resulting omission is disclosed under `imprecise_assets` — the stage whose contract
   means the number reads *high*. The direction of the error is inverted, so `incomplete` never fires and the
   short total is written to the no-TTL history as that file's permanent baseline.

Both are the same root shape: **the asset taxonomy is closed at CSS/wasm/font, and everything outside it exits
through a `None` arm that records nothing.** The design gave unsupported kinds no disclosure path at all.

On size: `docs/asset-counting-refactor-plan.md` already diagnoses the ~8,900-line growth correctly and sets
sound targets. It has one dangerous interaction with this review, called out in §5.

---

## 1. Release-blocking findings

### A1 — Images and SVG referenced from counted CSS vanish silently at High confidence

**Verification: `proof-confirmed`** (adversarial pass run under a default-suspect burden — presumed real,
required proof of *disclosure* to be dismissed; none was found).

`classify_asset` returns `None` for `.png/.jpg/.svg` (`daemon/src/engine/asset_classifier.rs:22`), so
`collect_supported_asset` returns `None` (`daemon/src/pipeline/css_dependencies.rs:99`) and the caller's
`None => {}` arm (`css_dependencies.rs:57`) records nothing.

The verification was exhaustive rather than sampled — it enumerated every exit rather than failing to find one:

- `CssDependencyAssets` has exactly three exits (`css_dependencies.rs:67-71`); all three are unreachable for an
  image.
- All four `uncounted` push sites are failure-driven and structurally unreachable for a file that was never
  collected (`assets.rs:1267`, `:1352`, `:1373`, `:1488`).
- All five diagnostic sources return `None` (`assets.rs:1013-1024`).
- `AssetKind` is structurally closed — `{Css, Wasm, Font}`, no `Other` (`daemon/src/engine/mod.rs:92-96`).

**Ordering proof.** The classify gate at `css_dependencies.rs:99` *precedes* the external/unresolved gate at
`:106-112`. So even `url(https://cdn/bg.png)` returns `None` before `SupportedAsset::Unresolved` could fire.
Images are outside both the counted set and the unresolved-disclosure set.

Confidence stays High because `engine_confidence` is a bare `diagnostics.is_empty()` check
(`daemon/src/pipeline/analyze.rs:670`).

**Aggravator.** Because the bytes never reach `uncounted_assets`, `incomplete` is never set — so unlike every
other omission shape, this one is *cacheable* and *is evaluated against budgets*. FR-032a (SRS:694) only
refuses a number carrying `incomplete` or `imprecise_assets`. This is a false budget PASS.

**Executed corroboration.** `cargo test --test analyze
analyze_local_assets_referenced_by_css_are_counted_in_the_import_cost` passes. That fixture
(`daemon/tests/analyze.rs:2723-2829`) is already the finding's exact shape — `sideEffects:["*.css"]`,
`import './styles.css'`, `background-image: url('./probe.wasm')` — and asserts High confidence with no
diagnostic. Swapping `probe.wasm` → `probe.png` changes exactly one step: `classify_asset` returns `None`, and
the bytes leave the headline with no channel replacing them.

**Smallest fixture**

```
node_modules/ui-kit/package.json  {"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}
node_modules/ui-kit/index.js      import './styles.css'; export const widget = () => 'widget';
node_modules/ui-kit/styles.css    .widget { background-image: url('./bg.png'); }
node_modules/ui-kit/bg.png        64 KiB
src/index.ts                      import { widget } from 'ui-kit';
```

Expected today: `asset_breakdown` has a CSS row only, headline = JS + CSS, diagnostics empty, confidence High,
64 KiB absent and unmentioned.

**Not already recorded.** `grep -i "image\|png\|svg\|sprite" docs/known-issues.md` → **0 hits**. Zero across
`docs/reviews/` and `docs/adr/`. The prior audit only *recommended* the record be written
(`asset-counting-audit.md:72`); it never was. SRS:52's out-of-scope list names CSS — whose bytes *are* counted
today — so it demonstrably does not govern asset bytes inside a measured package.

**Why this is worse than pre-B2.** Before B2 the headline did not claim to include assets. It now does, while
being short by an undisclosed, unbounded amount for the most common non-JS payload after CSS.

**The asymmetry worth noting.** The same `.png` fails *loudly* when imported directly from JS (see A10) and
vanishes *silently* when referenced from CSS into a High-confidence number. That asymmetry appears in no doc.

**Recommended change.** An `AssetKind::Unsupported` arm that counts zero but emits an `UncountedAsset` with the
raw bytes. That preserves ADR-0006's disclose-don't-fabricate rule and restores `incomplete`, which
automatically re-closes the cache and budget paths. It costs a Medium badge on many CSS-shipping packages —
which is the honest reading, and the design already accepted exactly that trade for processor failures.

---

### A2 — An asset omission is disclosed under the over-count stage, inverting its meaning

**Verification: `proof-confirmed`.** Independently reported by two reviewers working different lenses with no
shared context (a code-defect pass and a spec-conformance pass), then adversarially verified.

`unresolved_css_dependencies_diagnostic` emits `stage: IMPRECISE_ASSETS` with the message *"the stylesheet was
measured, but some CSS resource references could not be inspected, so this size may omit referenced font or
wasm artifacts"* (`daemon/src/pipeline/assets.rs:964-976`). That is an **under**-count reported under the stage
the contract reserves for **over**-counts.

**The trigger is ordinary CSS, and the verification found it in the pinned crate.** Dependency analysis is a
*third* print pass with `analyze_dependencies` enabled (`assets.rs:746-758`); the two prints that produce the
measured bytes (`:708`, `:790`) run with it **disabled**. Upstream lightningcss has an error gated on exactly
that asymmetry — `lightningcss-1.0.0-alpha.71/src/properties/custom.rs:501-507`:

```rust
if dest.dependencies.is_some() && is_custom_property && !url.is_absolute() {
    return Err(dest.error(PrinterErrorKind::AmbiguousUrlInCustomProperty { .. }))
}
```

Upstream's own test pins the trigger input (`lib.rs:29041`): `.foo { --test: url("foo.png") }`. So a package
shipping `:root { --icon-font: url(./fonts/icons.woff2) }` — ordinary theming CSS — makes the dependency print
fail while both measuring prints succeed. **The union is printed once, so one such declaration anywhere drops
`url()` discovery for every sheet in the union.**

The same inversion applies to the public-root case (`resource.has_root()` → `SupportedAsset::Unresolved`) and
to the `lightningcss could not inspect resource URLs` fallback at `assets.rs:765-774`.

**Consequence chain.** Those bytes land in `css_dependency_failures`, not `uncounted`, so
`has_uncounted_assets()` is false (`assets.rs:865`), so `totals.incomplete |= ...` never fires
(`daemon/src/pipeline/file_size.rs:628`). The result then passes `isDurableFileSize`
(`extension/src/analysis/transience.ts:106-112`) and is written to the no-TTL bundle-impact history v3
(`extension/src/analysis/history.ts:64-66`) as the file's permanent baseline — becoming a fake "regression" on
the next honest sizing. It renders as "File Cost", not "File Cost floor"
(`extension/src/analysis/fileCostQuality.ts:86-89`).

**Severity is correctly capped.** `imprecise_assets` *is* in `NON_BUDGETABLE_RESULT_STAGES`, so this produces
no false budget PASS. The harm is the permanent history baseline and the mislabeled quantity — which is
precisely the D12 defect reopened through a channel D12's one-meaning rule does not cover.

**Not already recorded.** D8 is a *parse* failure that falls back to `uncounted` — the correct channel. D9 is
the per-sheet union degradation, a genuine over-count with every sheet counted. Neither covers "sheet counted,
its `url()` graph unknown." Grep for `css_dependency` / `custom propert` in `known-issues.md` → nothing.

**Recommended change.** Route this channel to `uncounted_assets`. The stage taxonomy is right; the wire is
crossed.

---

## 2. High-severity findings

### A3 — A transient asset observation erases an unrelated durable failure

**Verification: `finder-claim`.**

`classify_failure` takes the `asset_io_diagnostic` early return at `daemon/src/engine/adapter.rs:386-396`
*before* reaching `state.take_breach()` at `:408-416`. So a package that both (a) has a genuinely missing
optional CSS import and (b) breaches `MAX_GRAPH_MODULES` reports the transient message *"retry after the
filesystem settles"* for a permanent, deterministic property of the package — and the real
`"module graph exceeds the 2000 internal module limit"` is discarded.

Because `ASSET_IO` is not in `DURABLE_RESULT_STAGES` while `MODULE_GRAPH_LIMIT` is, the failure is refused by
every durable store and **the 2000-module graph is rebuilt on every request** instead of once. Same root cause
at `:311-321`, where the durable `OUTPUT_SHAPE` is relabelled `ASSET_IO`.

The strongest evidence is in the file itself: the ordering comment at `adapter.rs:397-407` asserts that a
breach "preempts every diagnostic below" — and the asset arm was inserted *above* it.

### A4 — A remote `@import` permanently removes the budget verdict from an exact number

**Verification: `finder-claim`.**

A package shipping `@import url("https://fonts.googleapis.com/css2?family=Inter")` is measured *exactly* —
`TrackingProvider::resolve` correctly returns `ResolveResult::External` (`assets.rs:451-458`), lightningcss
leaves the rule in place, and the bytes are right. But the dependency print then yields a `Dependency::Import`
that `unresolved_import` reports as unmeasurable (`css_dependencies.rs:59-64`), tagging the result
`imprecise_assets` — which is in `NON_BUDGETABLE_RESULT_STAGES`.

So every budget surface returns "could not evaluate" instead of a pass/fail, and because `IMPRECISE_ASSETS` is
*also* durable, that verdict-less result is cached. **Two code paths contradict each other on the same
specifier**: `resolve` says "external, correctly not ours"; `unresolved_import` says "cannot be measured." No
test covers this path.

Google Fonts in a UI kit is not an exotic shape. This silently disables budgeting for a large class of real
packages.

### A5 — `unhashed_paths` are fingerprinted by a post-analysis re-read

**Verification: `finder-claim`, weak reachability — the reporting reviewer flagged the weakness itself.**

A module Rolldown loads but our `load` hook returned `Ok(None)` for gets no read-time fingerprint
(`plugin.rs:82-87`). Rolldown reads bytes v1 and the chunk is measured as S(v1); if the file is rewritten to v2
inside the analysis window, `dependency_fingerprints` (`daemon/src/service.rs:3128-3130`) later hashes v2 and
stores it against S(v1). Every later probe matches and answers `Fresh` — serving S(v1) until the file changes
*again*.

The rest of the codebase already treats this set as untrustworthy: `file_size.rs:608-613` maps it to
`unverifiable_file_fingerprint`, and `analyze.rs:499` refuses a full-package memo when it is non-empty. Only
the per-import result cache admits it as a normal hashed fingerprint.

Reachability is the weak leg — Rolldown's own loader also fails on most non-UTF-8 modules — which is why this
sits below A1/A2 rather than beside them.

---

## 3. Medium findings — the number moved, the explanation did not follow it

### A6 — `asset_breakdown` has exactly one reader

**Verification: `finder-claim`** (converged from the design and spec lenses).

The hover tooltip (`extension/src/ui/tooltipMarkdown.ts:98-119`) is the only consumer. Inlay hints, inline
decorations, the status bar, the workspace report, and package.json decorations never mention it. And
`FileSizeDocumentResponse` (`daemon/src/ipc/protocol.rs:653-690`) carries **no** `asset_breakdown` at all — so
the status-bar / "Show Current File Size" headline silently changed meaning between `rolldown-1.1.x+4` and `+5`
with no surface disclosing it.

Deeper: `module_breakdown` derives from JS-graph *rendered* bytes, and CSS is `ModuleType::Empty`. For a
CSS-dominant package, the report's own "top modules" list sums to a small fraction of the headline sitting next
to it, and `build_duplicate_module_groups` can never surface a shared stylesheet.

### A7 — No editor surface can say a number is an upper bound

**Verification: `finder-claim`.**

The CLI has the sentence verbatim — *"asset processing produced a disclosed upper bound, so budgets were not
evaluated"* (`cli/importlens.mjs:226-229`). The extension has no corresponding case, so "Show Current File
Size" prints a bare `File Cost: 120.4 kB br` while the configured per-file budget silently returns
`not-evaluated` (`extension/src/analysis/budgets.ts:85-96`).

The one degradation the daemon added an entire stage in order to *say* is unsayable in the editor. Relatedly,
`tooltipForResultMarkdown` renders `confidence_reasons` but never the diagnostic messages themselves on a
measured result, so the human-readable sentences written at `assets.rs:904-976` never reach the hover — the
user sees the raw stage token `imprecise_assets` instead.

### A8 — Combined Import Cost multiplies stylesheet bytes with no sharing story

**Verification: `finder-claim`.**

ADR-0004 sanctions counting a shared dependency N times *because* the report tells the sharing story through
`shared_modules` / `duplicate_imports`. Assets are invisible to that machinery
(`daemon/src/report/model.rs:290-313`), so a UI kit imported from 20 files contributes its stylesheet 20 times
with nothing saying those 20 are one file. Per D9's own measurements CSS compresses harder than JS, so the
multiplier is *worse* for CSS.

Nothing architectural is given up by fixing this — ADR-0004's design already had the slot; the asset work did
not fill it.

### A9 — A per-import `uncounted_assets` floor is budget-comparable (spec defect)

**Verification: `proof-confirmed` — partially survives; severity reduced by verification.**

`NON_BUDGETABLE_RESULT_STAGES` contains only `IMPRECISE_ASSETS` (`daemon/src/pipeline/stage.rs:170`), so a
per-import floor is compared against `perImportBrotliBytes` on JS-only bytes.

**The asymmetry decides the severity.** Budgeting a floor produces **false PASSes only, never false FAILs**: a
floor F ≤ true cost T, so a violation fires only when F > limit, which forces T > limit — every reported FAIL is
true. This is the opposite of the rationale the code was built on: `protocol.rs:469-471` justifies
`NON_BUDGETABLE` purely as *"comparing it with a threshold can produce a false failure"* — the over-count
hazard, which does not apply to a floor at all. The list was built for over-counts and never asked the
under-count question.

The CLI half of the original claim was **refuted**: `incomplete` propagation at `file_size.rs:628` makes
`isUsableFileSize` reject the file, so `importlens check` returns `EXIT_COULD_NOT_MEASURE` (3), not 0. The
history half is **already-recorded** under D12.

The code matches the SRS enumeration exactly, so **the defect is in the spec**: SRS:694's per-import
enumeration contradicts its own headline ("a gate that cannot measure must never report success") and ADR-0006
invariant 5. Genuine residual: the workspace report's `budgetViolationCount`
(`extension/src/ui/budgetDiagnostics.ts:36`) holds no File Cost and has no compensating gate — an undercounted
violation counter.

---

## 4. Documentation and contract drift

The SRS is the repository's stated source of truth and currently **states the opposite of shipped behavior**.

| Location | Problem |
| --- | --- |
| `SRS:431-435` (FR-018a) | "A package's non-JavaScript bytes are **disclosed, never counted** … hold the result at Medium confidence." Pre-B2; contradicts shipped behavior. |
| `SRS:52` | Out-of-scope list still excludes CSS/asset imports. |
| `SRS:801-803` | Error-handling row still says disclose + hold Medium. |
| `SRS:1616` (§10.7) | Describes disclosure as the outcome rather than the failure fallback. |
| SRS (whole file) | Zero occurrences of `asset_breakdown`, `AssetContribution`, or Lightning CSS. The wire model shipped undocumented. Zero occurrences of "font" — there is no asset taxonomy section, which is *why* A1 has no scope record to point at. |
| `README.md` | Zero occurrences of CSS / wasm / font / asset. A user cannot learn the headline can include stylesheet bytes. |
| `asset-counting-design.md:3-5` | Still says "not yet implemented" and "RELEASE BLOCKER". |
| `asset-counting-design.md:99-101` | Lists the CSS-combination model as an open question; it was settled and measured against the oracle (`known-issues.md:735-753`). Stale text, not an undecided design. |
| `asset-counting-design.md` "Consequences" | Says lightningcss joins the rolldown fingerprint closure; the implementation deliberately made it a **standalone** exact pin. The implementation is right; the design should record the refined decision. |
| `cli/importlens.mjs:14` | `protocolVersion = 6` while daemon and extension are at **7** (verified inline). Daemon accepts `1..=7` so nothing breaks today, but this is a third uncoordinated copy with no Drift test binding it. |

---

## 5. On the ~9,000 lines

`docs/asset-counting-refactor-plan.md` (untracked) already measures this honestly (+8,926 net; 3,680 in six
asset modules) and diagnoses it correctly — four provider constructors, two parallel budget mechanisms,
`Option<AssetProcessingContext>` threading a test-only bypass through production, correlated issue vectors
re-interpreted by three separate functions. Its target architecture (deep `artifact_measurement` seam outside,
minimal asset module inside) is sound and its stop conditions are well chosen. I am not re-deriving that work.

Two things this review adds to it:

**The largest single cost driver is a design choice, not sloppy code.** *(`finder-claim`)* Choosing
`lightningcss::Bundler` as the seam imported a recursive, self-driving file walker. Roughly 1,600 lines exist
solely to fence it in and none of them measure anything: it recurses on rayon workers whose stacks the daemon
does not own (forcing `asset_boundary.rs`'s pool + semaphore + deadline + `catch_unwind`, and a 256-*file*
bound standing in for a *depth* bound); it performs its own reads (forcing the ~360-line `TrackingProvider` to
intercept fingerprints); and those reads escape every engine limit (forcing `asset_budget.rs`'s separate
ledger). The alternative — parse each sheet with `StyleSheet::parse`, walk `@import` children with an explicit
iterative worklist the daemon owns, splice, minify once — produces the same artifact with a loop counter
instead of a stack proxy, fingerprints falling out for free, and no panic/deadline pool. It gives up
media/`supports`-qualified `@import` wrapping semantics. **Worth evaluating before Commit 6 of the refactor
plan, not after** — that commit fixes the current seam in place.

**⚠ The refactor plan's behavior freeze would freeze A1 and A2.** The plan requires behavior "byte-for-byte and
stage-for-stage equivalent" to `8fe2342`, and item 5 of its freeze explicitly blesses the current `url()`
policy. A1 and A2 are defects inside that frozen surface. **Fix A1 and A2 first, re-freeze on the corrected
behavior, then refactor.** Otherwise the refactor's characterization matrix will pin the bugs as correct and
make them much more expensive to remove later.

---

## 6. What the design and implementation got right

Worth recording so the report is calibrated — this is not a failing branch.

- **Per-artifact compression with summation, and the refusal to concatenate before compressing** — ADR-0005
  applied consistently to a new artifact class (`assets.rs:1319-1336`).
- **Binding bytes and fingerprint into one immutable value** (`engine/asset_input.rs:16-44`) so post-build
  processing cannot pair fresh bytes with a stale fingerprint. The right invariant, enforced by *type* rather
  than by discipline.
- **Running dependency analysis on a separate metadata-only print** (`assets.rs:741-752`) so lightningcss's
  placeholder-substituted output can never become the measured artifact. A genuinely easy trap, correctly
  avoided. *(That this same separation is what makes A2 reachable is an irony, not a reversal — the separation
  is correct; the missing piece is handling its failure.)*
- **Analyzing `url()` dependencies *after* minification** so a resource dropped from shipped CSS is not counted.
- **Remote `@import` returned as `External`** rather than a resolve failure (`assets.rs:451-458`) — correct, and
  A4 is a downstream bug, not a reversal of this decision.
- **Giving the union→per-sheet degradation its own state field** rather than a line in `failures`, on the
  explicit reasoning that `failures` goes silent when nothing is uncounted (`assets.rs:827-834`). The design
  correctly identifying that a channel with no trigger is not a channel.
- **Splitting request-local causes from deterministic ones structurally** rather than inferring durability from
  `io::ErrorKind` or error text.
- **`css_string_escape`** over a blanket `\`→`/` rewrite, with the Windows verbatim-prefix reasoning written
  down (`assets.rs:673-687`).

**AC-01 and AC-03 validated as correctly implemented** *(independent code-defect pass)*:

- AC-01 dedup holds — a font reachable both as a direct graph asset and via CSS `url()` is counted once, because
  `engine/plugin.rs:151` and `css_dependencies.rs:115` canonicalize to the same key consumed by `assets_by_path`
  (`assets.rs:1106-1121`). A font referenced by two sheets in degraded mode also dedups there.
- AC-03 holds — stubbed assets reserve metadata length *before* the read (`plugin.rs:665-675`), are seeded into
  `graph_files`/`input_bytes`, and `finish_read` correctly skips re-charging (`asset_budget.rs:405`).
- Resource-safety invariants hold as written: the per-attempt 256/8 MiB tree bound is fresh per
  `TrackingProvider` while `charge_css_work` on the shared context is never reset across union+retries
  (`asset_budget.rs:486-511`); the 8-second deadline is created before admission and covers both `acquire` and
  `recv_timeout` (`asset_boundary.rs:154-194`); the permit is moved into the spawned closure so an abandoned
  worker keeps it (`:166-167`); no mutex is held across blocking IO in `TrackingProvider::read`.
- Confidence rule conformant: a cleanly-processed stylesheet emits no diagnostic and does leave Medium; a
  genuine remaining `uncounted_assets` keeps it at Medium.
- Wire encode/decode of `AssetContribution` is an exact match across field names, types, enum casing, and
  msgpack ordering. D11 shows no regression in any of its three mirrors.

---

## 7. Findings adjudicated FALSE

Kept and marked, per the rule that a refuted finding is never silently dropped.

**The CSS union is a silent under-report that violates ADR-0005 — REFUTED.** The claim was that unioning all
reachable CSS into one compression window hides cross-artifact redundancy whenever a real build emits multiple
CSS chunks. ADR-0005's boundary is the **runtime**, not the source stylesheet, and its extension clause says
assets are separate artifacts *from the JavaScript chunk* — it never asserts each sheet is its own artifact.
Assets are in fact compressed per runtime group (`file_size.rs:274-293`, `:447-456`). Decisively, this tool
builds with **code splitting disabled** — more than one chunk is a typed `output_shape` failure (SRS:428) — and
Vite's per-async-chunk CSS splitting is a *consequence* of JS code splitting. One JS chunk ⇒ one CSS sheet is
the model-consistent artifact, verified against the esbuild oracle within 1%. The multi-sheet model the finding
wanted is the project-bundle model ADR-0004 explicitly refuses.

**Classifying `.scss`/`.less` as `AssetKind::Css` is a design error — REFUTED, and already-recorded as D8.**
The fallback is not guaranteed: a `.scss` file using no preprocessor-only syntax is valid CSS and bundles
normally (the repo's own tests must *manufacture* `$brand`/`@mixin` syntax to force failure,
`assets.rs:1793-1799`). The classification is not an assertion of parseability — it is what keeps the JS number
measurable, since a `.css` module reaching Rolldown fails the *entire* build with `UNSUPPORTED_FEATURE`
(SRS:433). Returning `None` would trade a disclosed fallback for an Unmeasured package. And the "floor" claim
is factually wrong about the code: the raw bytes enter no size and are disclosed as *"this size does not
include them"* (`assets.rs:924-931`). The residual — that a `.scss` source's byte count is not the byte count of
the CSS it compiles to — reaches only a diagnostic string, so it is a disclosure-wording nuance.

---

## 8. Lower-severity notes

- **A10 — a direct `import logo from './logo.png'` fails the whole package build** *(`proof-confirmed` in
  mechanism; `already-recorded` as D6's class)*. PNG: `String::from_utf8` fails, plugin declines, rolldown's
  default module-type map has no `png` entry, `fs::read_to_string` hits `InvalidData`, LOAD fails. SVG differs
  and still fails: it *is* valid UTF-8, so it is returned as code with `module_type: None`, and OXC parses
  `<svg …>` as JavaScript. D6's deferral rationale ("no wrong number, cannot wedge — it is the anti-fabricator
  working") holds verbatim. Loud, not silent — the honest counterpart to A1.
- **Per-sheet retry is O(n²) in path allocation** *(`finder-claim`)*. Each retry calls
  `TrackingProvider::new_bounded` → `context.snapshots()`, deep-cloning every `CollectedAsset` accumulated so
  far into a fresh `BTreeMap` under `lock_state()` (`assets.rs:163-174`, `asset_budget.rs:278-280`). For 100
  sheets that is ~10⁴–10⁵ `PathBuf`/`FileFingerprint` clones on a path already under an 8-second deadline. Bytes
  are `Arc<[u8]>` so the payload is shared; the metadata is not.
- **Unbounded, mostly-discarded stats** *(`finder-claim`)*. `collect_supported_asset` stats every `url()`
  resource and uses the value only in the `Err` arm (`css_dependencies.rs:114-124`) — 500 references means 500
  `fs::metadata` syscalls charged to **neither** the read ledger nor the deadline.
- **A missing `url()` target is disclosed as "0 bytes"** — `fs::metadata` fails → `raw_bytes = 0` → the message
  reads *"1 non-JavaScript asset(s) totalling 0 bytes that could not be processed"*.
- **`assetKindLabels[contribution.kind]` has no fallback** (`tooltipMarkdown.ts:109`) — a fourth `AssetKind`
  from a newer daemon renders `- undefined: 4.2 kB`. Practically unreachable (the daemon ships hash-pinned
  inside the VSIX).
- **`record_limit` keeps the lexicographically smallest message**, not the first cause
  (`asset_budget.rs:588-596`), so the reported limit may not name the breach that happened first. Cosmetic.
- **Nested rayon** — `compress.rs:20`'s `rayon::join` and lightningcss's internal rayon run inside the 2-thread
  asset pool. No deadlock was constructible, but one job's 8-second deadline can be consumed executing a stolen
  sibling. Explicitly labeled a guess by the reporting reviewer; no failing case.
- **`url("../../../../..")` traversal outside the package root is followed and counted**
  (`css_dependencies.rs:114`) with no containment check, though `node_modules` is treated as untrusted input in
  the same file's own reasoning. Off-lens; no wrong-number case.
- **Source maps and CSS-in-JS are absent from the design surface**, so their exclusion is indistinguishable from
  an oversight. CSS-in-JS matters for comparability: emotion/styled-components styles are already *inside* the
  JS number, so the CSS breakdown row is present for one styling architecture and structurally absent for
  another.

---

## 9. Recommended order

1. **A1** — add the unsupported-kind disclosure path. Smallest fix, largest correctness gain; automatically
   re-closes the cache and budget paths because `incomplete` starts firing again.
2. **A2** — route CSS-dependency omissions to `uncounted_assets`. Add the `--var: url(...)` fixture; it is a
   one-line CSS trigger.
3. **A3** — reorder `classify_failure` so a durable breach preempts the transient asset arm, matching the
   file's own documented ordering rule.
4. **A4** — stop treating a deliberately-external `@import` as an unmeasurable dependency.
5. Re-freeze behavior, then execute the refactor plan — evaluating the `StyleSheet::parse` worklist alternative
   before its Commit 6 rather than after.
6. **A6/A7** — carry `asset_breakdown` on `FileSizeDocumentResponse`; give the extension the upper-bound
   sentence the CLI already has.
7. SRS, README, and design-status updates in the same change that settles A1/A2, since they define which asset
   edges are counted.
8. **A9** — decide whether the spec or the code moves, and record it. A5 and A8 as scoped follow-ups.

## 10. Method and coverage

Five review passes (grounding, two code-defect slices, spec-conformance, design-critique) plus three
adversarial verification passes. `pnpm test` green throughout.

**Not covered:** `cache/disk.rs` and `cache/memory.rs` beyond their admission gates (shard layout, eviction,
flush path); `extension/src/ui/reportContent.ts`, `listener.ts`, `insights.ts`; the oracle and performance
harnesses; the `#[cfg(test)]` tails of the asset modules. Findings A3, A4, A5, A6, A7, A8 and everything in §8
are static traces tagged `finder-claim` — they were not independently verified, and the two findings that *were*
sent to verification with comparable confidence both came back **refuted**, so treat that tag as a real
confidence discount rather than a formality.

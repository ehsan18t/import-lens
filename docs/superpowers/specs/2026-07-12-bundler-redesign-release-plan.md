# Bundler Redesign — Release Plan

Status: **agreed, not yet implemented.** Settled in a design interview on 2026-07-12 against
`2026-07-12-bundler-redesign-release-review.md` (the findings) and the shipped code on
`bundler-redesign`.

The review listed 3 blockers, 12 should-fixes, 3 aggregate defects and 22 improvements. The
interview found they were not 40 independent items: most were symptoms of four unstated
decisions, now recorded as ADRs. It also surfaced **two defects the review missed**, one of
which the review's own recommended fix would have created.

Governing decisions:

- [ADR-0001](../../adr/0001-measure-a-neutral-build.md) — measure a neutral build
- [ADR-0002](../../adr/0002-upstream-owns-everything-it-can-answer.md) — upstream owns everything it can answer
- [ADR-0003](../../adr/0003-no-size-without-a-build.md) — if Rolldown did not build it, we report no size
- [ADR-0004](../../adr/0004-import-lens-measures-imports-not-bundles.md) — imports, not bundles
- [ADR-0005](../../adr/0005-a-runtime-is-an-artifact-boundary.md) — a runtime is an artifact boundary

Vocabulary is in [CONTEXT.md](../../../CONTEXT.md). The terms **Import Cost**, **Combined
Import Cost**, **File Cost**, **Side-Effectful**, **Truly Tree-Shakeable**, **Shared Module**
and **Unmeasured** are load-bearing below.

## Found during the interview, not in the review

1. **A CSS-importing package cannot be built at all.** `adapter.rs:266` fails any build that
   emits an asset ("expected exactly one chunk and no assets"), and `plugin.rs:276` lets
   Rolldown infer module type from the extension — so a `.css` module becomes an asset and the
   build fails. Today this is masked: the failure degrades to the entry-file fallback and shows
   a plausible number. **Under ADR-0003 those packages go blank.** The review's §6b.5 filed
   uncounted CSS as a post-release idea; ADR-0003 promotes it to a release floor. Affects
   `swiper`, `react-datepicker`, `react-toastify` and every CSS-shipping UI kit.
2. **The side-effects glob matcher is hand-rolled** (`resolver.rs:747-766` — brace expansion,
   segment matching, ~80 lines; no glob crate in `daemon/Cargo.toml`), while `fast-glob` — the
   matcher Rolldown itself uses via `rolldown_utils::pattern_filter` — is already in
   `Cargo.lock`. This was harmless only because `|| is_array()` threw its answer away. Task 4
   makes it load-bearing for a user-facing badge on a large fraction of real packages, so it
   must be replaced in the same change, not after it.

Also corrected: SF-11 claims "the protocol has no runtime to pass". `CompleteImportMembers`
already carries `source` and `cursor_offset`, and `script_regions.rs:123-150` already
classifies runtime from a document offset. Only `EnumerateExports` lacks the input.

## Sequence

Ordered by dependency. The instrument comes before the changes it must judge.

### 1. RB-1 — contain panics at the engine boundary

Wrap the build future in `AssertUnwindSafe(...).catch_unwind()` inside `with_permit`; map a
panic to `BundleFailure { stage: "panic", .. }` and let the existing §12 fallback arm handle
it. Today `boundary.rs:81-89` blocks on `recv().expect(...)`, so a Rolldown/OXC panic panics
the *calling analysis thread*, unwinds through `thread::scope` → `drain_classified` →
`handle_batch`, and turns an entire batch — **including every import already answered from
cache** — into one "analysis worker failed" error. Making the release profile unwind (6707baf)
made the daemon survive; it did not isolate the failure. The interactive paths (`Batch`,
`AnalyzeDocument`, `FileSize`, `AnalyzePackageJson`) have zero engine panic isolation.

Companion: `IN_FLIGHT.fetch_sub` (`boundary.rs:71`) is skipped on unwind, so the counter leaks
and `PEAK_IN_FLIGHT` latches. `peak_in_flight()` is the **only** assertion of the §9 two-build
invariant (`engine_boundary.rs:69`), so after two panicking builds the daemon's sole
concurrency check reports garbage. Use a drop-guard, as the semaphore permit already does.

### 2. RB-2 + SF-12 — build the instruments

One job; they share fixture plumbing that already exists.

- **RB-2.** `candidate_performance.rs` is `#[ignore]`d and **nothing invokes it** — not CI, not
  `package.json`. The trap: `validate.yml:150` runs `pnpm test:performance`, which is the
  *legacy* synthetic suite, a different file. `validate.yml:124-144` already installs real
  fixtures and runs `candidate_packages` with `-- --ignored`; add `candidate_performance`
  alongside it, gated on the §10.6 absolute numbers.
- **SF-12.** Nothing anywhere baselines a **badge** on a real package: `scripts/` never mentions
  `truly_treeshakeable`, and `candidate_packages.rs` stops at the engine boundary and never
  produces an `ImportResult`. The accuracy oracle checks bytes, never claims. Build a
  pipeline-level harness over the pinned real packages asserting `side_effects`,
  `truly_treeshakeable` and confidence.

This must land before tasks 3, 4 and 8, each of which moves badges or bytes on real packages.

### 3. SF-2 → SF-1 — the entry module belongs to its package

Write the failing row first; it is the proof the hole is real.

`plugin.rs:189-191` returns `HookResolveIdOutput::from_id(target)` with no `package_json_path`,
so for a plugin-resolved id Rolldown builds `ResolvedId.package_json = None` and the **entry
module** — the file every measurement is rooted at — falls back to pure source analysis.
Transitive modules, resolved by Rolldown itself, are unaffected.

It stayed invisible because every side-effects matrix row (`candidate_matrix.rs:950-961`) writes
a workspace-root `entry.js` doing `import 'testpkg'` — making `testpkg` *transitive*. Production
is the opposite shape: the user imports `date-fns`, so the entry **is** `node_modules/date-fns/…`,
resolved by our plugin, on the exact path that loses its metadata. All seven rows proving
"Rolldown owns sideEffects" exercise the one path production never takes.

- Add a row whose `BundleEntry` points **into** a `node_modules` package declaring
  `"sideEffects": false`, with an impure-looking top-level statement in its entry; assert the
  statement is dropped. It fails today.
- Supply `package_json_path = package_root/package.json`. `BundleEntry.package_root` is already
  carried (`engine/mod.rs:32`) and used only as `cwd`. Per ADR-0002 this is **metadata supply,
  not a semantic override**.

### 4. SF-3 + fast-glob — array `sideEffects`

`analyze.rs:318` reads
`side_effects_mode.has_side_effects() || side_effects_mode.is_array()`. `has_side_effects()`
**already answers correctly for arrays** (`resolver.rs:36-41` consults the matched patterns);
the `|| is_array()` overrides it with `true` unconditionally. `analyze.rs:339` then gates the
full-package comparison on `!side_effects`, so `truly_treeshakeable` is `false` **by
construction** — the comparison build never runs — and confidence drops to Medium.
`"sideEffects": ["**/*.css"]` is an everyday declaration; every such package is reported
side-effectful and never tree-shakeable, even where Rolldown demonstrably tree-shook it. The
code's justification ("glob matching unavailable from public bundler metadata") was **retracted
by the 2026-07-12 spec amendment**; the conservatism it bought was not.

- Drop `|| is_array()`. **Side-Effectful becomes a property of the import**: does the entry match
  a side-effect pattern? For `["**/*.css"]` and a JS entry that is `false`, and that is true.
- Replace the hand-rolled matcher with `fast_glob::glob_match`, and **exact-pin `fast-glob` into
  the compiler stack and its fingerprint** (ADR-0002). Two glob engines reading one `sideEffects`
  array can disagree; using Rolldown's own makes that impossible.
- Pin the array semantics with a test. `daemon/tests/analyze.rs:2009-2046` covers only the string
  form.

This is a **behaviour change, not a refactor** — expect badges to move on real packages, and
expect task 2's baseline to show exactly where.

### 5. ADR-0003 — Unmeasured

Three fallbacks fabricate a number where no build succeeded. All three go.

| `analyze.rs:157` | manifest unreadable | package's size **on disk** — overstates (tests, maps, unused files) |
| `analyze.rs:227` | entry over `MAX_MODULE_SOURCE_BYTES` | **entry file alone** — understates by ignoring the graph |
| `analyze.rs:260` | engine build failed (post-RB-1: also panicked) | **entry file alone** |

An import Rolldown could not build reports **no size** and says why. Deletes
`approximate_directory_size` and `estimate_minified_source` (a hand-written minifier estimator
used when OXC's minify fails — ADR-0002: if OXC cannot minify it, we do not guess).
`fallback.rs` drops from 146 lines to `source_excerpt_detail`. `analyze_static_entry`'s only two
callers are 227 and 260, so it goes with them.

Rolldown's warnings are **unrecoverable on a failed build** in 1.1.5 — `HookBuildEndArgs` carries
`errors` only, and warnings reach us solely via `BundleOutput.warnings`, which does not exist when
the build fails. Record this in the code so it is not mistaken for our own carelessness.

### 6. Q16 floor — assets stop failing the build

`adapter.rs:266` rejects any build emitting an asset. Keep the single-**chunk** guard (it is what
stops code-splitting from silently under-reporting); drop the "no assets" clause; measure the JS
chunk as today; **emit a diagnostic naming the uncounted non-JS bytes**. Without this, task 5
blanks every CSS-shipping package. Counting those bytes is deferred (below) — disclosing them is
not.

### 7. SF-4 + SF-15 — deterministic failure stage

`adapter.rs:297-301` reports the *first non-`link` diagnostic in Rolldown's vector*, accumulated
from module tasks running concurrently — so identical inputs can report `parse` on one run and
`resolve` on the next. The value is user-visible **and cached**, and after task 5 it is the
*primary* thing a user sees when there is no number.

- Rank stages by **pipeline order, earliest wins** (`resolve` → `load` → `parse` → `transform` →
  `link` → `generate`): deterministic, needs no judgement to maintain, and names causes rather
  than symptoms. All error diagnostics are already retained in `BundleFailure.message` and
  `.diagnostics`; only the *label* was lossy.
- `adapter.rs:358-366` stamps **every** warning with `stage: "generate"`, so an unresolved-import
  warning is labelled `generate`. One line.
- **Logging (per owner direction): an Unmeasured import logs its full diagnostic vector at
  `warn`, not `debug`.** After task 5, "error and no measured size" is the entire failure path, and
  the diagnostics *are* the answer — requiring a log-level flip plus a reproduction is how one ends
  up debugging from partial evidence. Results that produced a size keep debug-level detail. The
  existing per-`(request_id, specifier, error)` dedup (FR-039c) bounds the noise. Update
  `docs/logging-policy.md`.

### 8. ADR-0005 — the runtime partition

- **Compress per runtime group and sum.** `file_size.rs:210-222` joins the groups' minified output
  and compresses the concatenation once, so redundancy between two artifacts that ship separately
  is compressed away — a lower bound, with no diagnostic. After task 9 this is the number the
  budget gates on.
- **Partition sharing by runtime.** `file_size.rs:27-46` counts a module as shared across *every*
  result with no runtime partition, and `insights.ts:112-137` renders it as a savings insight — so
  a package imported from both Astro frontmatter and a client script is sold as a shared dependency
  when each runtime genuinely ships its own copy.
- Record the per-runtime build in the design doc. §6.3 still says the adapter "must not concatenate
  independently generated package bundles"; the code does exactly that, correctly, and the rationale
  has lived only in a code comment citing a deleted document (`file_size.rs:67`, `:161` cite "spec
  I15"/"I14" — the findings doc removed in 76ca304).

### 9. ADR-0004 — the aggregates

- **EXT-2 (editor).** `budgets.ts:67-99` sums per-import `brotli_bytes` into a file total, while
  `listener.ts:206-249` already fetches the deduplicated **File Cost** and `currentFileSize.ts:97`
  already displays it. Feed the budget the result the controller already has. Today a file with five
  `@mui/material` subpath imports sharing most of their graph is warned as 2–3× over budget while the
  status bar, one line away, shows it inside budget.
- **EXT-1 (report).** `model.rs:71` sums per-import brotli into a metric rendered as **"Total
  Brotli"** (`report.ts:137`). The arithmetic stays; the label becomes **Combined Import Cost**, and
  the report states that a dependency shared across files is counted at each site. Treemap
  percentages (`model.rs:354`) become "share of Combined Import Cost". The duplicate-imports table
  (`model.rs:274`) becomes correct by label: `react` across fifty files *does* have a combined import
  cost of fifty Reacts, and that is the point of the panel.
- **EXT-3 (tooltip).** `insights.ts:177-193` indexes shared modules by **specifier**; the daemon
  computes `shared_bytes` by **result**. `import React, { useState } from "react"` is one specifier
  and two results — so the daemon reports non-zero shared bytes while the extension finds no shared
  module to name and tells the user they are "outside the public top-module breakdown", which is
  false. Index by result.

### 10. SF-11 → derive the enumeration runtime server-side

`service.rs:1614` hardcodes `runtime: ImportRuntime::Component`. Component/Client resolve with
`alias_fields = ["browser"]`; Server resolves with node conditions (`resolver.rs:249,578`). So in
Astro frontmatter, a package whose `exports` map diverges across `node`/`browser` is **enumerated
under browser conditions while its size is computed under Server** — completions omit names the file
can import and offer names it cannot.

Per ADR-0002, one classifier, not two: `CompleteImportMembers` already carries `source` and
`cursor_offset`, so run `script_regions` over them — **no protocol change**. `EnumerateExports` gains
one field (the offset). The enumeration memo is already keyed by runtime but production only ever
writes the `Component` key; that dimension comes alive here, so pre-fix cached enumerations must not
be trusted after it.

### 11. SF-5 — the enumeration memo must expire

`export_list.rs:37-45` stores only source-module fingerprints and has **no TTL**. The size path
deliberately adds the root and first-party manifests (`analyze.rs:410-412`); enumeration does not. A
first-party package under development whose `package.json` flips `"type": "module"` (or edits
`exports`/`sideEffects`) moves no source file, bumps no generation — and the completion popup serves
the **old export list indefinitely**. Include the manifests, per §8.3.

### 12. SF-7 — `deps:update:safe` must restore what it recorded

`deps-update-safe.mjs:41-48` builds its restore pins from the **11 direct crates**, while the recorded
set (`compiler-stack.fingerprint.json`) is **52**. Rolldown's workspace siblings are *caret* ranges in
the 1.1.5 registry manifest, so a general `cargo update` moving `rolldown_utils`/`rolldown_plugin_*`
within their carets is not merely possible but inevitable on the next upstream patch: the restore
fixes 11, the fingerprint still mismatches, and the command **fails for the case §4.4 says it should
have restored**, leaving a mutated `Cargo.lock` with no recovery but `git checkout`. Derive the
`--precise` restore loop from the fingerprint's package list. Task 4 adds `fast-glob` to that
fingerprint, so this stops being theoretical.

### 13. §6.1 — raise `ENGINE_PERMITS`

From 2 to `available_parallelism().clamp(2, 4)`. The 20-import batch peaks at **78 MB against a
400 MB gate** — 5× headroom — while 20 misses serialize into 10 rounds. §10.7 explicitly authorizes
one bounded tuning pass on the build-concurrency limit. Sequenced **after task 2** so the gate that
has never run is what judges it.

### 14. RB-3 — package

`pnpm package:win32-x64`, refresh `knownHashes.generated.ts`, VSIX size check. **Last** — every task
above changes the daemon binary.

## Deferred, deliberately

- **Count non-JavaScript bytes** (Q16 proper): fold CSS/wasm/font bytes into the Import Cost, with
  per-artifact compression (ADR-0005), a CSS matrix row, and the esbuild oracle configured to emit
  CSS. Touches the engine contract, the adapter, both pipelines and the accuracy harness, and moves
  numbers on a whole category of packages. Task 6 discloses the gap in the meantime.
- **An honest lower bound on failed builds** ("at least 4 MB; graph limit exceeded") — the intended
  successor to ADR-0003. The engine discards the partial graph on failure, so it needs plumbing
  through the engine boundary; it does not belong inside a stability fix.
- **§6 improvements 2–22.** Real, but they are performance and hygiene on a path already meeting its
  gates. Shipping them beside this many semantic changes means an unexplained number movement has too
  many candidate causes.
- **Marginal Cost / a project-level bundle model** — ADR-0004. A different product, decided on its own
  merits.

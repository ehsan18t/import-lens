# Known Issues

Everything we found, decided **not** to fix, and did not want to lose. This is a tracker, not
an archive — it is maintained.

## How to use this file

**Add an entry when you decide not to fix something.** Not when you find something — when you
*decide*. A finding you fixed does not belong here; a finding you deferred does.

Every entry states **what actually happens** and **why it is not fixed**. An entry with no
failure scenario is a rumour, and a rumour in a tracker is worse than nothing.

### The rule we use to decide

> **Fix it now only if it (a) shows the user a WRONG NUMBER, or (b) can WEDGE the system or
> lose data. Everything else gets an entry here and goes back in the queue.**

This exists because a real finding is not the same as a blocking one. A chain of four rounds on
a *conservative* edge case once ran while eleven plan tasks sat untouched. "Real" was never the
right bar.

### Status values

| Status | Meaning |
| --- | --- |
| **Accepted** | We know, we are not fixing it, and we are content. Revisit only if the blast radius changes. |
| **Deferred** | Worth doing, not now. Should become a task. |
| **Watch** | Not a defect today. Becomes one if some condition changes. |
| **Unverified** | Shipped without the review we normally require. Not known to be wrong. |

---

## Environment

### E1 — `cargo test` fails at full parallelism on the primary dev machine
**Status: Deferred** · Blocks: `pnpm test`, and therefore the **pre-push hook**

`cargo test` reproducibly fails with `can't find crate for import_lens_daemon` /
`required to be available in rlib format`. It survives `cargo clean` and a fresh target
directory. **`-j 2` builds and passes cleanly.**

Almost certainly something else touching `target/` concurrently — rust-analyzer running its own
`cargo check`, or antivirus. Not a code defect, but it will bite anyone trying to push.

**Workaround:** `cargo test -j 2`.

---

## Path aliases

All four degrade to a **floor** (the file's total is flagged incomplete, is not cached, and
`importlens check` declines to judge it). A floor is conservative: **it is never a wrong
number.** That is why none of them is fixed.

### A1 — An alias declared *only* in a Vite/webpack/Rollup config is not seen
**Status: Accepted** · The only one with real-world reach

We read `paths` from `tsconfig.json` / `jsconfig.json` (and their `references` and `extends`).
An alias configured **only** in a bundler config is invisible → the file is a floor.

Narrow in practice: a TypeScript project must mirror aliases into tsconfig anyway or the editor
breaks. **A JavaScript-only Vite project with no `jsconfig.json` is the real exposure.**

**Repair for a user:** mirror the alias into `tsconfig`/`jsconfig` `paths`.

### A2 — More than 24 reachable configs: the tail is not walked
**Status: Accepted** · `MAX_REACHABLE_ALIAS_CONFIGS = 24`

The `references` walk caps at 24 configs. Beyond that, an alias declared in the 25th is not
seen → floor. The nearest config is normally a package's own, so a huge solution-style root is
rarely the one walked.

### A3 — Cross-project alias contamination
**Status: Accepted** · A deliberate consequence of the design

We ask **every** reachable `paths` table, so an alias declared only in `tsconfig.node.json` will
resolve for a document governed by `tsconfig.app.json`.

This is the price of making the answer **document-independent**, which is what fixed the
`.vue`/`.svelte`/`.astro` breakage: asking "which project owns this document?" is exactly the
question that kept producing regressions. It errs toward **"flag nothing"** and cannot invent a
number.

### A4 — A tsconfig edited while the VS Code watcher is not running is not seen
**Status: Accepted**

Alias-table invalidation rides the extension's file watcher. A tsconfig changed outside a
running VS Code session is stale until the daemon restarts. `importlens check` is unaffected —
the CLI spawns a fresh daemon per run.

---

## Unverified

### U1 — `a6cae06` did not get an adversarial review
**Status: Unverified** · To be covered by the final whole-branch review

The commit that hoists the alias resolver construction out of the per-specifier loop
(a 7× interactive-path regression fix: 27.96 ms → 7.27 ms at 20 aliased imports) was dispatched
without the independent verify pass every other commit on this branch received.

It self-proved the property that matters — the sticky-floor test still goes **red** if the
resolvers are re-memoised — and the full suite passes. But **every other round on this branch
had a Critical found by independent verification**, so this is recorded rather than waved
through.

---

## Engine and concurrency

### C1 — A package that reliably parks the bundler re-parks on every analysis
**Status: Accepted** · Bounded, and the alternative was worse

A build can park forever (Rolldown spawns its module tasks; the async runtime **swallows** their
panics, so the loader waits for a completion message that never arrives). `BUILD_TIMEOUT` (8s)
stops it holding an engine permit for good.

Its `timeout` result is — correctly — **never cached** (a transient failure must not become a
durable answer), so a package that reliably parks pays 8s again on each analysis. Two such
packages can hold both engine permits while the user types; other documents' imports wait, but
**no response is ever late**, because imports stream.

A per-entry circuit breaker was tried and **deleted**: it durably condemned *healthy* packages
that had merely been slow once. Do not reintroduce it.

### C2 — A cancelled build's module graph outlives its permit
**Status: Accepted**

On timeout the future is dropped and the permit released immediately, but Rolldown's already-
spawned module tasks keep running and hold the parsed graph. So peak RSS can briefly reach
~3 graphs rather than the 2 the permit count implies. Bounded (the tasks do complete) and it
cannot wedge or corrupt.

### C3 — `AnalyzeSpecifiers` still blocks on engine misses
**Status: Accepted** · Recorded as SRS FR-004b

The Compare-imports command and named-export candidates are one-shot commands with no
`AnalysisStore` rows for a streamed push to merge into, so streaming them would hand the UI an
empty list with nowhere for late results to land. They block, and with `EngineBudget` deleted
they carry no total time bound.

A fabricated comparison would be worse than "comparison failed".

### C4 — Cross-request response ordering is no longer guaranteed
**Status: Accepted** · A consequence of the multiplexing connection loop

Two pipelined requests may now be answered out of order. Nothing in the extension depends on it
(every response is routed by `request_id`), but it is a protocol-level behaviour change.

### C5 — Shutdown can take up to `BUILD_TIMEOUT`
**Status: Accepted**

Shutdown joins in-flight handlers under a bounded deadline, then flushes the cache
unconditionally. A build already inside Rolldown cannot be cancelled, so a parked one can hold
shutdown to its 8s limit. A task still running at the deadline is abandoned and its result is
not persisted — stated in the SRS rather than papered over.

### C6 — A nested `"type"` does not reach the pre-resolved entry (dual-package layouts)
**Status: Accepted** · One field, two lookups — no fix exists at the current upstream API

The plugin supplies the **package-root** `package.json` for the entry it pre-resolves
(`HookResolveIdOutput::package_json_path`). Rolldown then makes **two different** lookups against
it, and the field can only be right for one:

| lookup | manifest Rolldown wants | our supply |
| --- | --- | --- |
| `sideEffects` | the **topmost** manifest before the `node_modules` boundary (`find_package_json_for_a_package`) — the package root | **correct** |
| `"type"` (module format) | the **NEAREST** manifest above the file (`esm_file_format`) | correct **only when no manifest intervenes** |

**What actually happens.** Take the standard dual-package layout — root `package.json` is
`{"main":"./esm/index.js"}` with no `"type"`, and a nested `esm/package.json` is
`{"type":"module"}` — whose entry statically imports a CJS dependency. The same package emits two
different chunks depending on how it is reached (measured in-repo, unminified chunk):

```js
// reached TRANSITIVELY (Rolldown resolves the file, finds esm/package.json): 1333 B
var import_dep = /* @__PURE__ */ __toESM(require_dep(), 1);

// reached as the PRE-RESOLVED ENTRY — the production shape: 1330 B
var import_dep = /* @__PURE__ */ __toESM(require_dep());
```

The `isNodeMode` flag is what makes the namespace's `default` the whole `module.exports` object,
which is what Node does for an ES module importing CommonJS. Without it the entry is finalized as
a CommonJS importer: a **different `default` binding and a different measured size**.

**It is not a regression.** With no manifest supplied at all (the pre-`f2bdc17` behaviour) this
layout emits the identical 1330 B chunk — the entry's format was `Unknown` then and is decided
from a `"type"`-less root manifest now. Supplying the root manifest closed the `sideEffects` half
of the hole and left this half exactly where it was.

**Why it is not fixed.** Swapping in the nearest manifest would break the `sideEffects` half —
the half that stops a `"sideEffects": false` package's entry keeping statements Rollup and webpack
drop — which is a strictly larger error on a far more common layout. There is no third option
through this API.

**What would fix it:** an upstream Rolldown resolve-hook field that accepts the nearest manifest
separately from the package-root one; or resolving the entry **through** Rolldown instead of
pre-resolving it, which FR-017/§6.1 forbids (the engine must never re-resolve the bare specifier).
Recorded in SRS §10.7.

---

## Instrumentation honesty

### G0 — The legacy `performance.rs` smoke suite still claims to gate the NFR numbers, at 8× loose
**Status: Deferred** · Not an active hole — but a second suite that *appears* to gate what it does not

`daemon/tests/performance.rs` (the pre-existing synthetic-fixture smoke suite) asserts the **literal
NFR numbers** — `threshold_ms(500)` for a cache miss and `threshold_ms(50)` for a cache hit — with a
default multiplier of **6**, and CI's `pnpm test:performance` step sets **8**. So it enforces a
4000 ms "cache miss" and a **400 ms "cache hit" against a hard 50 ms Critical requirement**.

**This is not a coverage hole today.** `candidate_performance` now genuinely gates NFR-002 at an
absolute, unscaled 50 ms on every PR — proven by mutation (an 80 ms sleep on the cache-hit path
turns it red at 89 ms). The real gate works.

But it is **exactly the shape of the trap that hid the dark gate for months**: a suite whose name
and thresholds suggest it enforces a requirement, which in fact enforces something 8× looser. The
next person to read it will believe it.

**Fix:** either stop it naming the NFR numbers (they are its own smoke thresholds, not the
requirements), or delete it now that a real gate exists.

### G1 — The negative-`error` Guard catches 14 of 18 spellings
**Status: Accepted** · The number is machine-pinned, not claimed

The Guard bans the `!result.error` usability check — the single root cause of the
"transient becomes durable" defect that recurred **seven times** (see
[ADR-0006](adr/0006-the-result-model.md)).

It catches **14 of 18** planted spellings. The four misses are named in the test file with
reasons (destructured `const { error } = result`; a ternary; a bare `== null` expression; Rust
`let Some(_) = … else`). The count is **asserted**, so a future change that silently weakens it
fails the test.

**Static analysis is the second line here, not the first.** The real enforcement is that a
degraded result **has no size to misuse** — the size fields are `Option`, and the durability
gate lives **inside** each store.

---

## Deferred product work

These are real, and they are queued — not abandoned.

### D1 — Non-JavaScript bytes are not counted
**Status: Deferred** · Disclosed, never silently omitted

CSS, wasm and fonts a package ships are real bytes in a real bundle. Today the JS chunk is
measured and the uncounted asset bytes are **disclosed on the result**. Folding them into the
Import Cost changes the engine contract, both pipelines, the module breakdown and the esbuild
oracle, and moves numbers on a whole category of packages.

Compression must follow the artifact rule: an asset is a separate artifact from the JS chunk, so
it is compressed on its own and summed
([ADR-0005](adr/0005-a-runtime-is-an-artifact-boundary.md)).

### D2 — An honest lower bound on a failed build
**Status: Deferred** · The intended successor to ADR-0003

Today an unbuildable import reports **no size**. A graph-limit breach means much of the graph
*was* loaded before we stopped, so a real floor exists — *"at least 4 MB; graph limit exceeded"*
is strictly better than a blank. The engine currently discards the partial graph on failure, so
this needs plumbing through the engine boundary.

### D3 — Marginal cost / a project-level bundle model
**Status: Deferred** · A different product, decided on its own merits

*"Adding `zod` here costs nothing — it's already in your bundle."* Import Lens measures
**imports, not bundles** ([ADR-0004](adr/0004-import-lens-measures-imports-not-bundles.md)) and
has no model of what is already in the bundle. Answering this means building that union model.
It is the highest-value idea absent from the design, and it must be a deliberate decision, not
smuggled in as a bug fix.

### D4 — A file with one unmeasurable import can never cache its total
**Status: Deferred** · A performance cost of an invariant we want

An aggregate missing a contributor's bytes is a **floor**, and a floor is never cached. So a
file containing one permanently-broken import re-runs its (fast-failing) combined build on every
size request.

The honest fix is a **build memo for the deterministic build failure** — a failure caused by the
package's bytes is a fact about those bytes, and the cache is already keyed by their
fingerprints. Not caching the *total* is right; re-doing the *build* is waste.

### D5 — `importlens check` exit 3 will become common
**Status: Watch**

Any changed file with an unmeasurable import now exits **3 — "could not measure"** rather than
silently passing. That is deliberate: a gate that cannot measure must never report success, and
a silent pass **merges the regression**. But it is a real workflow cost, and if it proves noisy
the answer is to make fewer imports unmeasurable — not to make the gate lie.

---

## Deferred engine performance

From the release review's improvement list. All real; none blocking. Each is a **known cost**,
not a defect.

| # | Item |
| --- | --- |
| P1 | **Prewarm priority inversion** — a user typing an import can queue behind two in-progress prewarm builds. Reserve an interactive permit. |
| P2 | **Answer `CacheProbe::Unresolved` in the classify pass** — types-only, node-builtin and unresolvable imports construct no bundler, yet route through the engine drain. |
| P3 | **Drop the per-module source clone** — the graph's source is copied once per build for nothing. Hash first, then move the buffer. |
| P4 | **Avoid copying the linked chunk** — a multi-megabyte `clone()` purely to move it into the artifact. |
| P5 | **LRU the dependency-path index** — capped at 32 entries with an *arbitrary* eviction victim; a monorepo thrashes it and first-party freshness degrades nondeterministically. |
| P6 | **`drain_ordered` uses 2 workers where `drain_classified` uses 4** — package.json analysis and both prefetch drains idle a permit with work queued. |
| P7 | **Rebuild fixed option data once, not per build** — ~180 `String`s allocated per build; `LazyLock` candidates. |
| P8 | **The miss drain spawns fresh OS threads per call** — a single cache miss spawns a thread to do work the caller could do inline; a 500-file report can perform hundreds of thread creations. |
| P9 | **The completion path re-verifies a whole package graph on every popup** — re-reads and re-hashes every non-`node_modules` file per keystroke inside an import's braces. |
| P10 | **`ENGINE_PERMITS` is 2 against a 5× memory headroom** — 20 misses serialise into 10 rounds. (This one **is** a plan task; it is listed here only so the set is complete.) |

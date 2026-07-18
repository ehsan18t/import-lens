# Design: counting non-JS asset bytes (CSS, wasm, fonts)

**Status: IMPLEMENTED (2026-07-18). Superseded as a spec by
[FR-018a/FR-018b](ImportLens-SRS.md) — this document is kept for its rationale, not as the
contract.** B2 is closed. Where this design and the SRS disagree, the SRS wins.

Two decisions below were refined during implementation and are recorded here so the drift is not
mistaken for an oversight:

- **`lightningcss` did NOT join the Rolldown dependency-graph fingerprint closure.** The
  "Consequences" section calls for that, but the crate's version is chosen independently and is not
  reachable from rolldown/oxc, so forcing it into that closure would couple two unrelated things. It
  is a **standalone exact pin** with its own drift test instead.
- **The "CSS combination / dedup model" open question below is settled, not open.** All reachable
  CSS becomes one artifact, measured against the esbuild oracle on `@uiw/react-md-editor` and
  agreeing within 1%. This is model-consistent because the tool builds with code splitting disabled
  — more than one chunk is a typed `output_shape` failure — so one JS chunk implies one CSS sheet.
- **The asset taxonomy needed a disclosure path for kinds it does not count.** The design named
  CSS/wasm/font and left everything else implicit, which shipped as a silent drop. See FR-018b: an
  unsupported-but-shipped resource is now disclosed at its real size rather than dropped.

**Why it blocks the release.** Today the engine measures a package's JS chunk and only discloses its non-JS
asset bytes (CSS, wasm, fonts) beside the result, never folding them into the Import Cost. So the headline
number shown under the "Import Cost" label is materially smaller than what the import really ships, for a whole
category of packages (every UI kit that ships CSS, every package that ships wasm or fonts). Disclosing the
shortfall does not make the headline correct. That is a wrong number, so this is a correctness fix, not the
coverage nicety it was first filed as, and it must land before release.

## Why

A package's real cost is not only its JavaScript. UI kits ship CSS; some packages ship wasm or
fonts. Today the engine measures the JS chunk and records the **raw** bytes of a reachable
stylesheet as an `uncounted_asset` — disclosed on the result, but never folded into the Import Cost
and never processed as it would actually ship. So a CSS-heavy package under-reports, and the badge
reads "unavailable"-adjacent rather than "here is what it costs".

This respects [ADR-0004](adr/0004-import-lens-measures-imports-not-bundles.md): counting a package's
own CSS/wasm/font bytes is still measuring *that import's* cost, not modelling a project bundle.

## What the engine does today

`daemon/src/engine/plugin.rs` `load` hook: rolldown 1.1.5 refuses to bundle CSS
(`UNSUPPORTED_FEATURE` at LINK), so a reachable `import "./x.css"` is stubbed to `ModuleType::Empty`
and its raw file length is inserted into `state.uncounted_assets`. The JS graph then measures
cleanly and the CSS is disclosed but uncounted. Binary modules (wasm, fonts) are non-UTF-8, so `load`
hands them back to rolldown untouched.

## Decisions (2026-07-15)

1. **Fold all asset bytes into the headline Import Cost**, shown together as one number — with a
   **per-type breakdown line in the details / inlay panel** (`CSS`, `wasm`, `fonts`) whenever that
   type is present. Composition stays legible without cluttering the badge.
2. **Count all asset types at once**, not CSS-first. CSS needs a processor; wasm and fonts do not
   (their shipped size is the raw bytes, compressed), so the marginal cost over CSS-only is small.

## The design, end to end

1. **Classify at the boundary (`plugin.rs load`).** Replace the narrow `is_stylesheet` branch with a
   general asset classifier: **stylesheet**, **wasm**, **font**, else pass through to rolldown.
   Collect each asset's path, fingerprint, and raw bytes into a typed set on the plugin state. CSS is
   still stubbed to `Empty` (rolldown cannot link it); binary assets are stubbed the same way so the
   JS number stays exact.

2. **Process, post-build.**
   - **CSS** goes to **Lightning CSS** (the `lightningcss` Rust crate: Parcel's engine, and what the JS
     `@tsdown/css` plugin is itself built on). The JS `@tsdown/css` wrapper is not usable here, because the
     daemon embeds the Rust rolldown crate rather than the JS tsdown toolchain, so we call `lightningcss`
     directly at the interception point the plugin already has. Its `Bundler` resolves and inlines `@import`s
     (rolldown never traversed them, since CSS was stubbed), then minifies. All reachable CSS becomes **one**
     stylesheet artifact, mirroring how CSS ships (a single file per entry), and letting Lightning CSS dedupe
     shared `@import`s.
   - **wasm / fonts** → no processor. The shipped size is the raw file bytes. (woff2 is already
     brotli-internally; it will barely shrink further, which is correct.)

3. **Compress per artifact and sum ([ADR-0005](adr/0005-a-runtime-is-an-artifact-boundary.md)).**
   Each artifact — the JS chunk, the combined CSS stylesheet, each wasm/font file — is compressed on
   its own (min/gz/br/zstd) and the compressed sizes are **summed**. Never concatenate JS+CSS before
   compressing; they are separate files that ship separately. For a binary asset "min" is just its
   raw size (nothing to minify).

4. **Flow through all three quantities.** Import Cost = its JS + its assets. File Cost = the combined
   JS bundle + the combined asset set (one deduped CSS stylesheet across all imports). Combined
   Import Cost = the sum of per-import (JS + assets). The quantity model from the Task 9 naming work
   is unchanged; assets are just additional artifacts in each sum.

5. **`uncounted_assets` becomes a counted breakdown.** The engine result carries a per-type
   contribution (`{ kind: Css | Wasm | Font, min, gz, br, zstd }`). This crosses the wire
   (daemon protocol → extension) and reaches the inlay/hover renderer, which draws one line per type
   present. The residual "uncounted" shrinks to genuine failures only.

6. **Failure falls back, never below today.** A Lightning CSS parse/bundle failure on a stylesheet
   reverts that asset to today's raw-byte disclosure plus a diagnostic. Per
   [ADR-0006](adr/0006-the-result-model.md) we never fabricate; a floor beats a blank, and today's
   behaviour is the floor we must not regress under.

## Consequences that are not optional

- **`lightningcss` joins the exact-pinned compiler stack.** It processes bytes that determine the
  reported number, so a version bump can silently move CSS sizes — it is exact-pinned (`=x.y.z`),
  version-tested, and added to `compiler-stack.fingerprint.json` / `deps:update:compiler` / Task 12's
  restore set, exactly like rolldown / oxc / oxc_resolver / fast-glob.
- **The esbuild oracle and badge baselines re-baseline.** The oracle harness must build and count
  assets too. Lightning CSS's minifier ≠ esbuild's, so CSS-heavy packages will show a delta that must
  fit the accuracy tolerance — measured, not assumed (the same situation as oxc_minifier vs esbuild
  for JS today).
- **Cache freshness must include the assets.** The CSS entry *and* the `@import` children Lightning
  CSS reads (captured via its `SourceProvider`), plus each wasm/font, feed `read_time_fingerprints`,
  or an asset edit would not invalidate. Same discipline as the size path and the Task 11 memo.
- **Confidence rules shift.** A package whose only uncounted bytes were assets, now counted, can
  leave "Medium"; one with a genuine remaining `uncounted` (a Lightning CSS failure) stays Medium.

## Open questions to settle at implementation

- **CSS combination / dedup model.** All reachable CSS → one stylesheet is the plan. Whether to
  dedupe identical rules across independently-reached stylesheets (a real bundler with CSS
  code-splitting may or may not) needs a decision measured against the oracle.
- **Binary asset shipping model.** Count emitted-file bytes (compressed), not base64-inlined bytes —
  inlining is a consumer-config choice that inflates ~33% and should not be modelled. Confirm against
  what esbuild's oracle emits so the two agree.
- **Per-runtime interaction (Task 8).** Assets are compressed per artifact within each runtime group
  and summed, consistent with the runtime-as-artifact-boundary rule.

# Plan: implementing B2 (counting non-JS asset bytes)

**Status: implementation plan for [known issue B2](known-issues.md), branch `bundler-b2-asset-counting`.** The
shape is fixed by [asset-counting-design.md](asset-counting-design.md); this is the ordered build, with the
exact surfaces, the decisions the design left open, and the risks. It lands as its own pull request.

## The dependency: `lightningcss`

Pin it into the exact-pinned compiler stack, exactly like rolldown / oxc / oxc_resolver / fast-glob:

```toml
lightningcss = { version = "=1.0.0-alpha.71", default-features = false, features = ["bundler"] }
```

- `1.0.0-alpha.71` is the latest published; the crate has only ever shipped `1.0.0-alpha.*`, and the API breaks
  between alphas, so it is exact-pinned and upgrade-gated like the rest of the stack.
- `bundler` is what we need (parse + resolve `@import`s + minify + print). It pulls `dashmap` + `rayon`; `rayon`
  is already in the tree via rolldown, so the marginal weight is small. `browserslist` stays **off** (it embeds
  the caniuse dataset and we want deterministic, target-free output).
- No arena/allocator to thread through (unlike oxc).

## Architecture: classify at the boundary, process post-build, compress per artifact, sum

Mirrors how the JS chunk is already sized, and honors [ADR-0005](adr/0005-a-runtime-is-an-artifact-boundary.md)
(each artifact compressed on its own, then summed) and [ADR-0006](adr/0006-the-result-model.md) (a failure
falls back to today's disclosure, never below it).

1. **Classify (`engine/plugin.rs` `load`).** Replace the narrow `is_stylesheet` branch with a general
   classifier: **stylesheet | wasm | font | passthrough**. Stub CSS **and** the binary assets to
   `ModuleType::Empty` (today wasm/fonts pass through to rolldown, which can perturb or fail the JS build), so
   the JS number stays exact. Collect each asset's `{ path, kind }` into a typed set on `BuildState` (replacing
   the `uncounted_assets: HashMap<PathBuf, u64>`), and keep capturing the read-time fingerprint of the entry as
   today.

2. **Process, post-build (new `engine` step, consumed in `pipeline/analyze.rs`).**
   - **CSS** → Lightning CSS. Combine **all** reachable stylesheet imports into **one** artifact to match how
     CSS ships and how the esbuild oracle emits a single `.css`. Use a custom `SourceProvider`
     (`TrackingProvider`) wrapping `FileProvider`:
     - `read()` records each opened path (entry + every resolved `@import` child) into a `Mutex<HashSet<PathBuf>>`
       (canonicalized) — this is the freshness watch set.
     - `resolve()` delegates to `FileProvider` for relative imports. (An `oxc_resolver` fallback for a bare
       `@import "pkg"` was planned here and **not built** — such a sheet falls back to raw-byte disclosure on its
       own, which is the pre-B2 floor. See Risks, and D8 in known-issues.) **Decision:** for >1 reachable top-level CSS import, bundle via a
       synthetic entry that `@import`s each real path (so Lightning CSS inlines and dedupes into one sheet);
       for exactly one, bundle it directly. The provider must outlive the `StyleSheet`, and must be `Send + Sync`
       (the bundler uses `rayon`); do all consumption (bytes + captured paths) inside that scope.
     - Minify with `Targets::default()` and print with `minify: true` → the shipped CSS bytes.
   - **wasm / fonts** → no processor. Shipped size is the raw file bytes (woff2 is already brotli-internally; it
     barely shrinks, which is correct).
   - **Fallback:** any Lightning CSS error (parse, unsupported `@import`, bare specifier, IO) reverts that asset
     to today's raw-byte disclosure plus a diagnostic. Never below current behavior.

3. **Compress per artifact and sum.** Each asset artifact — the combined CSS, each wasm/font — is compressed on
   its own (`compress_all`: min/gz/br/zstd; for a binary asset "min" is its raw size) and the sizes are **added
   into the five `MeasuredSizes` fields**. Two integration points, because assets are additional artifacts in
   each sum:
   - **Single import** — `pipeline/analyze.rs::analyze_with_rolldown_engine` (~line 520): today it sets the five
     sizes from the JS chunk alone; add the per-asset compressed sizes.
   - **Per runtime (File Cost)** — `pipeline/file_size.rs` per-runtime loop (~450-555): the combined-CSS + each
     wasm/font compressed and added into `totals` beside the JS sums.

4. **Counted breakdown to the result + wire + UI.** Replace the `uncounted_assets` disclosure with a **counted
   per-type contribution** on the result: a structured `Vec<AssetContribution { kind: Css | Wasm | Font, min,
   gz, br, zstd }>` on `ImportResult` (Rust `protocol.rs` + TS mirror `protocol.ts`, plain `Option`/serde-safe
   per the positional-msgpack note). Render one line per present type on the tooltip / inlay surfaces
   (`resultDiagnostics.ts` gains an accessor; `tooltipMarkdown.ts`, `packageJsonTooltip.ts`,
   `format.ts`/`packageJsonLabels.ts` render it). A residual `uncounted_assets` diagnostic remains only for the
   Lightning CSS **failure** fallback.

5. **Cache freshness.** Feed the `TrackingProvider` read-path set (CSS entry + every `@import` child) and each
   wasm/font path into `read_time_fingerprints` / `stat_paths` (`pipeline/analyze.rs` ~504-518), so an edit to
   any asset or its `@import` child invalidates the cached size. Same discipline as the JS module path.

6. **Confidence.** A package whose only previously-uncounted bytes were assets, now counted, can leave Medium
   (`engine::diagnostic_stage` rationale). One with a genuine remaining `uncounted` (a Lightning CSS failure)
   stays Medium.

## Oracle + badge re-baseline (required, not optional)

- **`scripts/accuracy-compare.mjs` `esbuildNamedSize`:** stop trusting `outputFiles[0]`; classify by extension,
  compress each output on its own, sum JS + CSS (esbuild emits a sibling `.css` under `bundle:true`). Generalizes
  to the 0-CSS case with no change to existing benchmarks.
- **Fixture:** add `@uiw/react-md-editor` (`named: "headingExecute"`) to `realFixtures` — the only real package
  whose published ESM entry (`esm/index.js`) actually `import`s CSS. NOT react-loading-skeleton (its `import`
  condition ships no CSS import). It is already pinned in `scripts/accuracy-fixtures/package.json@4.0.8`; verify
  `react-dom` (its peer) is in the committed lockfile. Add a Drift guard asserting its ESM entry still contains
  `import "./index.css"`.
- **Tolerance (measured 2026-07-17):** the CSS benchmark lands at **24.8%**, and it is **not** a counting error:
  the MINIFIED totals agree within 1% (1,118,802 ours vs 1,127,883 esbuild's), so both sides fold in the same
  stylesheet exactly once — a double count or a missed `@import` would move that uncompressed number too. Only
  the compressed figure diverges, because the daemon compresses brotli at **quality 4** (it runs per keystroke)
  while the oracle uses **quality 11**; that asymmetry costs every JS benchmark 2.6-15% and is amplified on
  highly-compressible CSS. So the global 25% stays (it still has to gate the JS set against a real regression)
  and the CSS fixture carries its own documented 35%, exactly as this plan required rather than loosening the
  shared gate. The JS worst case moved 13.0% -> 15.0% (refractor) for unrelated reasons and is recorded.
- **`outdir` is required, not cosmetic:** esbuild REFUSES to bundle a CSS-importing graph without an output path
  ("Cannot import ... into a JavaScript file without an output path configured"), and without one even a pure-JS
  build names its output `<stdout>`, so there is nothing to classify by extension. Setting it (with
  `write: false`) is what makes the sibling `entry.js` + `entry.css` pair appear.
- **Badge tests (`daemon/tests/candidate_badges.rs`):** flip
  `a_css_shipping_real_package_is_measured_and_discloses_its_stylesheet` from asserting an `uncounted_assets`
  diagnostic to asserting the counted CSS contribution; re-check the `@uiw/react-md-editor` row (Medium may move
  to High once the CSS is counted).

## Tests

- **Unit:** the classifier (stylesheet/wasm/font/passthrough); Lightning CSS bundling of a CSS entry with an
  `@import` child (bytes > 0, child path captured); the raw-byte fallback on a deliberately broken stylesheet.
- **End to end (`daemon/tests/analyze.rs`):** a CSS-shipping fixture package folds its CSS into the Import Cost
  (size strictly greater than the JS-only size) and carries a counted `Css` contribution, not an
  `uncounted_assets` diagnostic; a wasm/font fixture folds its raw bytes; a broken stylesheet falls back to
  disclosure at no less than today.
- **Freshness:** editing a reachable `@import` child invalidates the cached size.
- **Aggregate:** a file importing a CSS-shipping package has a File Cost that includes the combined CSS once.

## Revision

B2 moves measured numbers, so bump `ANALYZER_REVISION` to `rolldown-1.1.x+5` (this PR is separate from the
`+4` batch that shipped B1 + B3). Add a doc paragraph.

## Ordered phases (each: implement inline, narrow gate, then move on; adversarial review on the risky commits)

1. Pin `lightningcss` (Cargo.toml + `compiler-stack.config.mjs` + regenerate `compiler-stack.fingerprint.json`
   + the drift assertion in `compiler-stack-coordination.test.mjs`).
2. Classify + collect the typed asset set at `plugin.rs`; stub CSS and binary assets.
3. Process (CSS via Lightning CSS + `TrackingProvider`; wasm/fonts raw) + compress-per-artifact + sum in
   `analyze.rs` and `file_size.rs`.
4. Counted breakdown: protocol field (Rust + TS) + the four render surfaces.
5. Freshness: read-path set + binary asset stat paths into the fingerprint source.
6. Oracle + badge re-baseline; tests; `ANALYZER_REVISION` → `+5`.

## Risks

- **Alpha dependency.** `lightningcss` is `1.0.0-alpha.*`; exact-pinned and upgrade-gated, so its instability is
  contained the same way rolldown/oxc are.
- **Empirical tolerance.** The Lightning-CSS-vs-esbuild CSS delta is measured, not assumed; the tolerance may
  need re-derivation or a `cssTolerance`.
- **Bare `@import`.** Published dist CSS almost always uses relative imports. The provider does NOT resolve a
  bare specifier (an `oxc_resolver` fallback was planned and not built): such a sheet falls back to raw-byte
  disclosure on its own, which is the pre-B2 floor. Recorded as D8 rather than left as an unmet promise here.
- **Build cost.** One more compiled crate; `rayon`/`dashmap` are modest and partly shared. Watch the build-memory
  budget the recent `debuginfo` change protects.

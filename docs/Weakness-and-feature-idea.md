## Code Weakness

**Rust Daemon**

Weaknesses: `bundle.rs` is doing a lot of manual source-level text surgery (span-based replacements, object shorthand detection by walking backward through bytes) that's inherently fragile. The `is_object_shorthand_occurrence` heuristic with `enclosing_delimiter` is the kind of thing that will misfire on pathological inputs. Also `analyze.rs` is quite large and has grown some complexity around fallback chains.


`bundle.rs` is the biggest concern. The `enclosing_delimiter` function walks backward through raw bytes to detect object shorthand — this is the kind of heuristic that silently misfires on template literals containing `{`, regex literals, or multi-byte Unicode. A pathological input like `` const x = `{${'nested'}}`  `` could confuse it. The `apply_replacements` sort/dedup logic also has a subtle ordering bug: when two replacements share the same `start`, sorting by `end` descending and then `value.len()` ascending can produce non-deterministic output for equal-length replacements. This should use a stable sort with a total order.

`analyze.rs` has grown to ~500 lines and the fallback chain (`approximate_manifest_fallback` → `analyze_with_cjs_graph` → `analyze_with_oxc_pipeline` → `analyze_static_entry`) reads like a decision tree that should be an explicit enum or state machine. The nested `if let Some` and early-return pattern makes the control flow hard to audit for completeness.

`graph.rs` at ~900 lines is doing too many things. The OXC AST traversal logic for exports (`collect_statement_bindings`, `collect_declaration_bindings`, etc.) could be a separate `ast_walk.rs` module. The `ModuleGraphBuilder` pattern is good but the `loading_paths` / `circular_edges` / `visited` triple-set logic is subtle and a bug magnet.

The `prefetch.rs` `OnceLock<rayon::ThreadPool>` for the prewarm pool is fine, but there's no way to shut it down cleanly during the recycle sequence. Jobs running in the pool at recycle time will complete against a service that's being torn down. The `CancellationToken` generation counter helps but doesn't block the thread pool from holding a strong `Arc<ImportLensService>` longer than the daemon lifecycle expects.


**TypeScript Extension**

Weaknesses: `listener.ts` is doing quite a lot — it's both the analysis orchestrator and the partial response applier. The `analyze()` method has grown to ~100 lines with multiple early returns and mutable `currentStates`. The `formatWarningSuffix` function in `format.ts` has a subtle bug — if `runtime === "server"` AND `is_cjs` is true, you get `" · server · CJS"` which is arguably wrong (server imports can't really be CJS-meaningful to the user). Also the `isProcessedAstroScript` regex in `scriptRegions.ts` is slightly wrong — it only allows `src=...` but the spec says no *extra* attributes beyond processing; a plain `<script>` with no attributes should also be included, which it is, but `<script type="module">` would be incorrectly excluded.


`listener.ts` `analyze()` is too long (~150 lines) and mutates `currentStates` in a partial-response closure while the outer function holds a different reference to `states`. The aliasing between `states`, `currentStates`, and `responseStates` is confusing. If a partial response races with the final response processing, the final `store.set` will overwrite partial enrichment from streaming frames. This is a real correctness issue in the streaming path.

`format.ts` `formatWarningSuffix` has a logical issue: if `runtime === "server"` AND `is_cjs` is true, you get `" · server · CJS"`. Astro server imports literally cannot be CJS-meaningful to the client, so the CJS suffix is noise. The function should early-return `runtimeSuffix` when runtime is server.

`scriptRegions.ts` `isProcessedAstroScript` uses `^src\s*=\s*(?:...)\s*$` — this correctly allows `src=...` as the only attribute. But `<script type="module">` has no `src` attribute and the trimmed attributes string is `type="module"`, which doesn't match and so the script is excluded. Astro's docs say `<script type="module">` *is* processed — this is a bug. The guard should check for the absence of `is:inline`, `is:raw`, and similar opt-out attributes rather than checking for a specific opt-in form.

`workspaceScanner.ts` `analyzeScannedImports` sends batches grouped per source file which is correct, but the `fallbackRequestId` initialization from `Date.now()` means two rapid workspace scans can produce colliding request IDs. The freshness tracker in `listener.ts` uses a monotonic counter — this should too.


---

## Feature Ideas

### 3. Duplicate Dependency Detection

The daemon already computes `shared_bytes` and `module_breakdown`. With that data, you could detect when two different packages vendor the same dependency (e.g. both `react-query` and some other library ship their own copy of a utility). Surface this as an insight: "3 of your imports include `tslib` — consider hoisting it." The graph already has all the module paths; you just need to look for the same canonical path appearing across multiple `internal_contributions` lists.

### 1. `.d.ts`-Only Package Detection

Some packages (`@types/*`, pure type packages) ship only declarations and have zero runtime cost. Right now the daemon probably errors or returns a small size for these. Detecting and explicitly labeling them as "types only — 0 bytes runtime" would avoid confusing "unavailable" decorations and would actually be useful information.

### 2. Treemap Visualization for the Workspace Report

The current report is a sortable table. A treemap (packages as rectangles, sized by brotli bytes, colored by whether they're tree-shakeable) would make it immediately obvious which dependencies dominate bundle size. This is purely a webview enhancement — all the data is already in `WorkspaceReportRow`. Libraries like `d3` are available in React artifacts if you wanted to prototype it.

### 3. ESM/CJS Migration Assistant

For packages marked `is_cjs: true`, the daemon could check whether a newer version of that package ships ESM (by looking at its npm metadata or the `module` field in package.json). Surface a hint: "lodash@4.18 is CJS (~70kB). lodash-es@4.17 is ESM and tree-shakeable (~2kB for this import)." This requires a network fetch to npm registry (which the daemon explicitly prohibits), but it could be done in the extension host as an opt-in background check.

### 4. Streaming inlay hint updates

Currently partial batch responses update the store but the `InlayHintsProvider` still shows `…` until the full response. Wire partial responses directly into individual hint updates so each import resolves independently as the daemon finishes it, rather than all waiting for the batch. The `indexes` field in partial responses already provides the data; it just needs to trigger individual hint refreshes rather than full-document re-renders.

## Known Bugs That Needs to be Fixed

- Currently the fixtures in inside `daemon/tests/fixtures/packages` doesn't get removed after the test is done which keep unnecessary files in the repository. We should clean up those files after the test is done. Also, if possible to evaluate the same thing or similar impact without these packages, please do that. I don't like this package, also the test is not even working properly, we need to suppress to make the build pass.
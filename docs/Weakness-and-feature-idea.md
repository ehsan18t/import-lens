## Code Weakness

**Rust Daemon**

Weaknesses: `bundle.rs` is doing a lot of manual source-level text surgery (span-based replacements, object shorthand detection by walking backward through bytes) that's inherently fragile. The `is_object_shorthand_occurrence` heuristic with `enclosing_delimiter` is the kind of thing that will misfire on pathological inputs. Also `analyze.rs` is quite large and has grown some complexity around fallback chains.

**TypeScript Extension**

Weaknesses: `listener.ts` is doing quite a lot — it's both the analysis orchestrator and the partial response applier. The `analyze()` method has grown to ~100 lines with multiple early returns and mutable `currentStates`. The `formatWarningSuffix` function in `format.ts` has a subtle bug — if `runtime === "server"` AND `is_cjs` is true, you get `" · server · CJS"` which is arguably wrong (server imports can't really be CJS-meaningful to the user). Also the `isProcessedAstroScript` regex in `scriptRegions.ts` is slightly wrong — it only allows `src=...` but the spec says no *extra* attributes beyond processing; a plain `<script>` with no attributes should also be included, which it is, but `<script type="module">` would be incorrectly excluded.

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


## Known Bugs That Needs to be Fixed

- When we run build with docker it builds all file which is fine. but the issue is it saves the hash of only 1 build and that is the latest one. The purpose of our saving the hash is so that dev don't get error on their first project open is defeated here. 1 solution I can think of is, we can save the hashes in txt file in a separate file and scan those the ts file where we are saving hash currently. IF YOU CAN THINK ANYTHING BETTER, PROCEED WITH THAT INSTEAD.
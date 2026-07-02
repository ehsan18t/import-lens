# Daemon Review Fixes Implementation Plan (v2 — cross-checked)

> **STATUS: AWAITING EXPLICIT APPROVAL. No implementation, no commits, until the user signs off.**
>
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every verified defect found in the deep review of `daemon/` (crash, wrong-size, broken-feature bugs), remove verified dead code, consolidate duplicated helpers, and land low-risk performance wins — one commit per task.

**Architecture:** The daemon is a Rust IPC server (`ipc/`) delegating to `service.rs`, which orchestrates a static-analysis pipeline (`pipeline/`: resolver → module graph → reachability → bundle → minify → compress) with a per-project cache (`cache/`), npm registry hints (`registry/`), workspace reports (`report/`), and background prewarm (`prefetch.rs`). The TypeScript extension (`extension/src/`) is the only production client: it spawns the daemon (`daemon/nativeTransport.ts`), speaks length-prefixed MessagePack (`ipc/codec.ts` ↔ `daemon/src/ipc/codec.rs`), and drives `analyze_document` on a 300 ms debounce per keystroke (`listener.ts`).

**Tech Stack:** Rust 2024, oxc 0.138 (parser/semantic/transformer/minifier; resolver 11.22), tokio, rayon, papaya, redb, rmp-serde, ureq 3. Extension: TS + @msgpack/msgpack.

## Global Constraints

- Every task ends with `cargo test -p import-lens-daemon` green and `cargo clippy -p import-lens-daemon --all-targets` introducing no new warnings.
- Protocol wire format (`ipc/protocol.rs` serde output) must not change — the extension pins `protocolVersion = 7` (`extension/src/ipc/protocol.ts:1`).
- Public daemon functions used only by `daemon/tests/*.rs` are the integration-test seam — do not remove them (`FrameDecoder`, `encode_frame`, `bundle_reachable_modules`, `build_workspace_report`, `package_json_prewarm_requests`, `registry_hints_for_tests`, …).
- Commit messages: conventional commits (`fix:`, `feat:`, `refactor:`, `perf:`, `test:`, `chore:`) with a body explaining the user-visible symptom where applicable.
- `daemon/tests/review_repros.rs` (untracked) pins today's broken behavior for every bug below; each fix task converts its repro into a proper regression test, and the file is deleted at the end.

---

## Part A — Verification report

Every finding was verified twice: first by reading the Rust, then either (a) an executable repro in `daemon/tests/review_repros.rs` run against the pristine tree, or (b) exact-line grep evidence, plus (c) a cross-check against the extension TypeScript to confirm real user impact. Items that did not survive re-verification are listed as **withdrawn/downgraded** below — they are NOT in the task list.

### Confirmed bugs (all have executable repros; run `cargo test -p import-lens-daemon --test review_repros`)

| ID | Claim | Daemon evidence | Extension cross-check |
|----|-------|-----------------|----------------------|
| B1 | Star-export cycle → unbounded recursion in `resolve_export_binding` (`bundle.rs:518` has no visited set, unlike `graph_exports_name` in `analyze.rs:697` which has one) → **daemon process aborts** | `repro_star_cycle_stack_overflow` (run with `-- --ignored star_cycle`) exits `0xc00000fd STATUS_STACK_OVERFLOW` | Crash triggers extension restart policy (`restartPolicy.ts`); 3 crashes in 60 s ⇒ ImportLens goes unavailable (`nativeTransport.ts:262-275`) — a poisoned package produces a crash-loop into degraded mode |
| B2 | `mark_export` star handling (`reachability.rs:143-160`) only checks the star target's **direct local** exports; names provided via `export * → export {x} from` or nested `export *` are missed ⇒ empty bundle ⇒ silent full-bundle fallback (`analyze.rs:289-302`, adds **no diagnostic**) ⇒ **full-package size at HIGH confidence** for tree-shakeable barrel imports | `repro_star_reexport_chain_is_missed_by_reachability` (bundle comes out empty) + `control_star_direct_export_is_reached` proves the adjacent case works | Size renders as the primary inline hint (`importHintParts.ts:62-75`); confidence tone comes from `result.confidence` — user sees a confidently wrong number |
| B3 | Two modules importing the same local name from different externals emit duplicate top-level bindings in the synthetic import header (`bundle.rs:87-135`, last-writer-wins at `:97/:99`) ⇒ semantic error in minify ⇒ silent fallback to LOW-confidence static-entry sizing (`analyze.rs:163-172`) | `repro_external_local_name_collision_breaks_minify` (minify returns Err) | Fallback size + "Static entry sizing is a fallback" reason surface in tooltip/report |
| B4 | Member completion aborts when any earlier import statement has no braces: `?` on `named_import_member_range` inside the group loop returns `None` for the whole document (`completion.rs:61`) | `repro_completion_bails_on_earlier_braceless_import` (control without the default import works) | `ImportMemberCompletionProvider` is registered for `{`/`,` triggers (`extension.ts:130`); `import React from 'react'` on line 1 is the canonical layout ⇒ completions never appear |
| B5 | **(found by extension cross-check)** Extension sends `document.offsetAt(position)` — UTF-16 code units (`completions.ts:43`) — but the daemon compares `cursor_offset` against **byte** offsets from oxc spans (`completion.rs:90-101`) | `repro_completion_cursor_offset_is_bytes_but_client_sends_utf16` (two `€` before the import shift the offsets apart; UTF-16 offset misses, byte offset hits) | Any non-ASCII char (emoji in comments, non-Latin strings) before the cursor silently kills completion |
| B6 | Completion parses the **whole document** as hardcoded TSX (`completion.rs:18-24`) ⇒ always fails in .vue/.svelte/.astro (template markup is not TSX) and breaks valid `.ts` angle-bracket assertions (`<string>x` starts a JSX tag in TSX) | Verified by reading + oxc source-type table (`oxc_span-0.138.0/source_type.rs:192-215`: `.ts` = TypeScript/Standard, TSX = Jsx variant); the region machinery that import analysis uses (`script_regions.rs`) is simply not used here | Completion provider is registered for svelte/astro/vue too (`languages.ts:13-21` + `extension.ts:130`) ⇒ feature is dead in three advertised languages |
| B7 | Bare Node builtin subpaths (`fs/promises`, `path/posix`, `timers/promises`, …) are not in `is_node_builtin_specifier` (`graph.rs:1345`) ⇒ classified as an npm package named `fs` ⇒ resolution fails | `repro_builtin_subpath_treated_as_package` (`node:fs/promises` control already filtered by the URL-scheme rule) | `status: "missing"` renders an inline **"Package not found"** hint (`importHintParts.ts:50-56`) on perfectly valid Node imports |
| B8 | JSON module with an `eval`/`arguments` key synthesizes `export const eval = …` — a strict-mode SyntaxError (`graph.rs:602`, `is_safe_js_identifier` blocklist misses both) ⇒ whole module graph fails | `repro_json_eval_key_breaks_graph` | Import falls to error/static fallback; any package importing such JSON degrades |
| B9 | **(found by oxc cross-check)** `.js` documents parse without the JSX variant (`SourceType::from_path` → JavaScript/Standard), so CRA-style JSX in `.js` fails `analyze_imports` entirely (`imports.rs:36-51` propagates region parse errors) | `repro_jsx_in_js_documents_fails_analysis` (`App.js` fails, `App.jsx` control passes) | `.js` files get languageId `javascript` ⇒ in scope (`languages.ts`); on parse failure `listener.ts:122-127` clears the store ⇒ **zero hints in the whole file**, silently |
| B10 | `server_writes_package_json_partial_frame_before_final_response` is flaky: 200 ms first-partial timeout (`tests/server.rs:649-654`) races analysis startup under parallel suite load | Failed once on a cold full-suite run; passes 3/3 isolated and 3/3 repeated after warmup. The real assertion (first frame has `indexes == Some([0,1])`) is order-based, not time-based | n/a (test-only) |

### Dead code (grep-verified, zero callers)

| ID | Item | Evidence |
|----|------|----------|
| D1 | `script_regions.rs:49 language_from_filename` + `:312 _language_from_filename_for_tests` | Only caller of the former is the latter; the latter has zero callers in src+tests |
| D2 | `minify.rs:100-111` call-expression marker branch (`__importLensUse(...)`) | `usage_markers` (`bundle.rs:662-679`) only ever emits `export { .. as __importLensUse_.. }`; grep for `__importLensUse(` finds no generator |
| D3 | `analyze.rs:157-162` `matches!(request.import_kind, Named\|Default\|Namespace\|Dynamic)` | Lists every `ImportKind` variant (`protocol.rs:8-13`) — always true |
| D4 | `cjs_scan.rs:456 is_boundary` | Pass-through wrapper for `is_identifier_boundary` |
| D5 | `ignore.rs:94 glob_matches` | Pass-through wrapper for `glob_matches_exact` |

### DRY violations (exact lines)

| ID | Item | Copies |
|----|------|--------|
| R1 | unix-millis helpers | `cache/disk.rs:470`, `cache/project.rs:606`, `ipc/server.rs:103` (`current_time_millis`), `service.rs:1236` (`current_time_millis`), `lifecycle.rs:95` (`unix_millis(SystemTime)`) |
| R2 | protocol version check `(1..=PROTOCOL_VERSION).contains` | `ipc/server.rs:62`, `ipc/server.rs:986` (`is_supported_hello_version`), `service.rs:1456` |
| R3 | single-diagnostic constructors | `ipc/server.rs:69` (`protocol_diagnostics_for_stage`), `ipc/server.rs:978` (`protocol_diagnostics`), `service.rs:1349` (`protocol_diagnostics`) |
| R4 | empty `WorkspaceReportSummary` built by hand | `ipc/server.rs:84-95`, `service.rs:1357-1369` — all fields are `u64`/`Vec`, `Default` is derivable |
| R5 | resolve→key→probe→analyze→alias→insert tail | `service.rs:1014-1046` (`prewarm_import`), `:1048-1077` (`prewarm_resolved_import`), `:1142-1173` (`analyze_with_cache`) |
| R6 | `graph.rs:1080-1116 statement_span` 36-arm match | `impl GetSpan for Statement` exists (`oxc_ast-0.138.0/src/generated/derive_get_span.rs:583`) |

### Inefficiencies (read-verified, low-risk fixes in scope)

| ID | Item | Evidence |
|----|------|----------|
| P1 | New `ureq::Agent` per registry request → no connection reuse, one TLS handshake per package | `registry/client.rs:26-30` builds the agent inside `get_package_metadata` |
| P2 | `flush_to_disk` rewrites **every** memory entry as its own committed redb txn on recycle, though `insert_with_fingerprints` already persisted them synchronously | `cache/memory.rs:139-154` + `disk.rs:103-141`; only *failed* inserts need replay. Existing test `cache_disk.rs:406` (`flush_to_disk_persists_memory_entries_for_reload`) stays green under dirty-only flush because the insert itself persisted |
| P3 | `ProjectCacheRegistry::cleanup` walks the whole cache dir tree 3× | `project.rs:158` (`list_shards`), `:171` (`list_shards` again), `:203` (`total_size_bytes` → `list_shards`); each computes recursive `directory_size` per shard |
| P4 | `GRAPH_CACHE` is unbounded and each entry retains full prepared module sources (limit: 100 MB/graph, `MAX_GRAPH_SOURCE_BYTES`) | `graph.rs:26-31`; eviction only via invalidation/fingerprints — long multi-package sessions can hold GBs |
| P5 | `position_at` rescans the document from byte 0 per lookup; each detected import needs 6 lookups (`line`, `quote_end`, 2×2 range ends) | `positions.rs:3-37`, callers `imports.rs:381-398`, `package_json.rs`; runs per keystroke (300 ms debounce, `listener.ts:72` + `config.ts:33`) |
| P6 | Redundant `fs::canonicalize` (an open-file syscall on Windows): importer re-normalized though the caller normalized it one frame earlier; CJS queue entries re-normalized though every enqueued path is already canonical | `graph.rs:334+339`, `cjs.rs:30+42` |
| P7 | Recent-prewarm decodes every cache key twice (`decode_cache_identity` + `cached_import_request_from_key` which decodes again) | `prefetch.rs:251-254` + `:294-305` |
| P8 | 7 pre-existing clippy warnings | 6 collapsible-if (`server.rs:274,485,486`; `registry/service.rs:62,74,175`), 1 manual checked division (`report/model.rs:354`) |

### Withdrawn or downgraded after cross-check (NOT in the task list)

| Item | Why withdrawn/downgraded |
|------|--------------------------|
| "Daemon `--storage` vs `hello.storage_path` might point at different dirs and break legacy-cache removal" | They **do** differ by design: `--storage` = `globalStorageUri` (lifecycle: recycle stamps, legacy central `importlens.redb`), `hello.storage_path` = per-workspace `daemon-cache` (shards + registry metadata) — `storagePaths.ts:17-27`. Every daemon use is consistent with that split. Not a bug. |
| "Disk-cache hits skip fingerprint verification" | False — `DiskCache::get_entry` verifies (`disk.rs:91-96`). Withdrawn. |
| "Removing the current project's cache fails on Windows because the redb file is open" | Drop-order analysis: `remove_shard_by_id` takes the shard out of the map, `clear()`s, and drops the map's `Arc` **before** `remove_dir_all` (`project.rs:320-370`), so the DB closes first in the common case. A failure needs a *concurrently running* analysis/prewarm holding the Arc — a transient race that the UI reports and a retry fixes. Downgraded to deferred watch-item DF-9. |
| "`is_import_lens_marker_statement`'s export-form check might also be dead" | No — export-form markers ARE emitted (`bundle.rs:662-679`). Only the call-form branch (D2) is dead. |
| "`import_statement_spans.contains` is O(n²)" | n = import statements per module; negligible. Dropped. |
| "`package_json_dependency_entries`/`_sections` double-parse the JSON" | package.json is KBs; not worth an API change. Dropped (noted in DF-12). |
| Extension `sendBatch`/`requestFileSize` chains are product-dead | True (only `extension/test/**` uses them), **but** they are the `AnalysisTransport` contract surface designed for a WASM fallback transport that was planned in `docs/superpowers/plans/2026-05-29-incomplete-feature-completion.md:805-861` and never built. Removal is a product decision — parked as DF-11 for the user to decide, not a unilateral cleanup. |

---

## Part B — Tasks (in execution order; one commit each)

### Task T1: Fix stack overflow in `resolve_export_binding` (star-export cycles) — B1

**Files:**
- Modify: `daemon/src/pipeline/bundle.rs:518-552`
- Test: `daemon/tests/bundle.rs` (append)

**Interfaces:**
- Produces: `resolve_export_binding(graph, module_id, exported_name, visited: &mut HashSet<(ModuleId, String)>) -> Option<String>` (private; signature gains `visited`).

- [ ] **Step 1: Write the failing regression test** (append to `daemon/tests/bundle.rs`)

```rust
#[test]
fn bundle_survives_star_export_cycles_without_stack_overflow() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { x } from './a.js';\nexport const value = x;",
    );
    write_source(&root, "a.js", "export * from './b.js';");
    write_source(&root, "b.js", "export * from './a.js';");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("cyclic star exports should still bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("__il_m0_value"), "{}", bundled.source);
}
```

- [ ] **Step 2: Run to verify it aborts today**

Run: `cargo test -p import-lens-daemon --test bundle bundle_survives_star_export_cycles`
Expected: process aborts with `STATUS_STACK_OVERFLOW` (0xc00000fd).

- [ ] **Step 3: Add the visited set**

```rust
fn resolve_export_binding(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> Option<String> {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return None;
    }

    let module = graph.module_by_id(module_id)?;
    if let Some(export) = module
        .exports
        .iter()
        .find(|export| export.exported_name == exported_name)
    {
        return Some(module_binding_name(module_id, &export.local_name));
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && let Some(binding) =
                resolve_export_binding(graph, target_id, &reexport.imported_name, visited)
        {
            return Some(binding);
        }
    }

    for star_export in &module.star_exports {
        let target_id = graph.module_id_by_path(&star_export.resolved_path)?;
        if let Some(binding) = resolve_export_binding(graph, target_id, exported_name, visited) {
            return Some(binding);
        }
    }

    None
}
```

Update the single call site in `rename_map`:

```rust
            let target_name = resolve_export_binding(
                graph,
                target_id,
                &binding.imported_name,
                &mut HashSet::new(),
            )
            .unwrap_or_else(|| module_binding_name(target_id, &binding.imported_name));
```

- [ ] **Step 4: Run** `cargo test -p import-lens-daemon --test bundle` → all pass.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/bundle.rs daemon/tests/bundle.rs
git commit -m "fix(bundle): prevent stack overflow on star-export cycles" -m "resolve_export_binding recursed through re-export and star-export edges with no visited set, so a star-export cycle (legal ESM, common in barrel files) overflowed the stack and killed the daemon with STATUS_STACK_OVERFLOW whenever an imported name could not be resolved through the cycle. Track visited (module, name) pairs like graph_exports_name already does."
```

---

### Task T2: Fix reachability miss for names provided through `export *` chains — B2

**Files:**
- Modify: `daemon/src/pipeline/graph.rs` (add shared `module_provides_export`)
- Modify: `daemon/src/pipeline/reachability.rs:143-160` (star loop in `mark_export`)
- Modify: `daemon/src/pipeline/analyze.rs:697-744` (`graph_exports_name` → delegate to shared helper)
- Test: `daemon/tests/bundle.rs` (append)

**Interfaces:**
- Produces: `pub fn module_provides_export(graph: &ModuleGraph, module_id: ModuleId, exported_name: &str, visited: &mut HashSet<(ModuleId, String)>) -> bool` in `graph.rs` — consumed by `reachability.rs` and `analyze.rs`.

- [ ] **Step 1: Write failing tests** (append to `daemon/tests/bundle.rs`)

```rust
#[test]
fn reachability_follows_star_export_to_reexport_chains() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './b.js';");
    write_source(&root, "b.js", "export { x } from './c.js';");
    write_source(
        &root,
        "c.js",
        "export const x = 1;\nexport const y = 'HEAVY_UNUSED_PAYLOAD';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["x".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("__il_m2_x"), "{}", bundled.source);
    assert!(
        !bundled.source.contains("HEAVY_UNUSED_PAYLOAD"),
        "{}",
        bundled.source
    );
}

#[test]
fn reachability_follows_nested_star_export_chains() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './b.js';");
    write_source(&root, "b.js", "export * from './c.js';");
    write_source(
        &root,
        "c.js",
        "export const x = 1;\nexport const y = 'HEAVY_UNUSED_PAYLOAD';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["x".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("__il_m2_x"), "{}", bundled.source);
    assert!(
        !bundled.source.contains("HEAVY_UNUSED_PAYLOAD"),
        "{}",
        bundled.source
    );
}
```

- [ ] **Step 2: Run to verify both fail** (empty bundle today)

Run: `cargo test -p import-lens-daemon --test bundle reachability_follows`
Expected: FAIL — bundle source does not contain `__il_m2_x`.

- [ ] **Step 3: Add shared provider check in `graph.rs`** (below `is_node_builtin_specifier`)

```rust
pub fn module_provides_export(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> bool {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return false;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return false;
    };

    if module
        .exports
        .iter()
        .any(|export| export.exported_name == exported_name)
    {
        return true;
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if reexport.imported_name == "*" {
            return true;
        }

        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && module_provides_export(graph, target_id, &reexport.imported_name, visited)
        {
            return true;
        }
    }

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path)
            && module_provides_export(graph, target_id, exported_name, visited)
        {
            return true;
        }
    }

    false
}
```

- [ ] **Step 4: Use it in `reachability.rs::mark_export`** — replace the star loop (which currently pre-checks only `target.exports`):

```rust
        for star_export in &module.star_exports {
            let Some(target_id) = self.graph.module_id_by_path(&star_export.resolved_path) else {
                continue;
            };
            if module_provides_export(self.graph, target_id, exported_name, &mut HashSet::new()) {
                self.reachable
                    .symbols
                    .insert((module.path.clone(), exported_name.to_owned()));
                self.mark_export(target_id, exported_name);
            }
        }
```

Add `module_provides_export` to the `graph` imports in `reachability.rs`. (Recursing `mark_export` unconditionally would over-include: `mark_export` marks the target module reachable before probing, which pulls the whole import closure of unrelated star targets — hence the provider pre-check.)

- [ ] **Step 5: Delete `analyze.rs::graph_exports_name`; delegate to the shared helper**

In `missing_export_diagnostics`, replace the filter closure body with:

```rust
            !module_provides_export(graph, graph.entry_id, exported_name, &mut HashSet::new())
```

and import `module_provides_export` from `graph`. Remove imports that become unused.

- [ ] **Step 6: Run full suite** — `cargo test -p import-lens-daemon` → green. Delete `repro_star_reexport_chain_is_missed_by_reachability` from `review_repros.rs` (it asserts the broken behavior and would now fail).

- [ ] **Step 7: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/src/pipeline/reachability.rs daemon/src/pipeline/analyze.rs daemon/tests/bundle.rs daemon/tests/review_repros.rs
git commit -m "fix(reachability): follow star-export chains through re-exports" -m "mark_export only recognised a star-export target when the name was one of the target's own local exports, so names provided through export * -> export {x} from or nested export * chains (standard barrel layouts) were never marked reachable. The bundle came out empty and the pipeline silently fell back to full-package sizing at HIGH confidence. Share the transitive provider walk (module_provides_export) between reachability and the missing-export diagnostics so both agree."
```

---

### Task T3: Fix external import local-name collisions in bundles — B3

**Files:**
- Modify: `daemon/src/pipeline/bundle.rs` (synthetic import emission ~lines 46-135, `rewrite_module`, `rename_map`)
- Test: `daemon/tests/bundle.rs` (append; existing `bundle_hoists_and_deduplicates_external_imports` at `:206` may pin old local names — update its assertions to the canonical names if it fails)

**Interfaces:**
- Produces (private): `external_binding_name(index: usize, imported_name: &str) -> String`; `rename_map(graph, module, external_indexes: &HashMap<String, usize>)`; `rewrite_module(graph, module, reachable, keep_all_exports, external_indexes)`.

- [ ] **Step 1: Write the failing test** (append to `daemon/tests/bundle.rs`; add `minify_source_with_markers` to the `minify` imports at the top)

```rust
#[test]
fn bundle_renames_colliding_external_import_locals() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import a from './m1.js';\nimport b from './m2.js';\nexport const value = [a, b];",
    );
    write_source(
        &root,
        "m1.js",
        "import shared from 'ext-one';\nexport default shared;",
    );
    write_source(
        &root,
        "m2.js",
        "import shared from 'ext-two';\nexport default shared;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    assert!(bundled.source.contains("from 'ext-one'"), "{}", bundled.source);
    assert!(bundled.source.contains("from 'ext-two'"), "{}", bundled.source);
    assert_semantic_valid(&bundled.source);
    let minified = minify_source_with_markers(&bundled.minifier_source, false)
        .expect("bundle with colliding external locals should minify");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(!minified.is_empty());
}
```

- [ ] **Step 2: Run to verify it fails** (duplicate `shared` binding → semantic error)

Run: `cargo test -p import-lens-daemon --test bundle bundle_renames_colliding_external`
Expected: FAIL at `assert_semantic_valid` or the minify `.expect`.

- [ ] **Step 3: Implement canonical external bindings**

1. Helper next to `module_binding_name`:

```rust
fn external_binding_name(index: usize, imported_name: &str) -> String {
    let suffix = match imported_name {
        "default" => "default".to_owned(),
        "*" => "ns".to_owned(),
        name => sanitize_identifier(name),
    };
    format!("__il_ext{index}_{suffix}")
}
```

2. In `bundle_reachable_modules_with_metadata`, build a deterministic per-specifier index before the module loop:

```rust
    let included_modules = graph
        .modules
        .iter()
        .filter(|module| included.contains_key(&module.id))
        .collect::<Vec<_>>();
    let mut external_specifiers = included_modules
        .iter()
        .flat_map(|module| module.external_imports.iter())
        .map(|ext| ext.specifier.clone())
        .collect::<Vec<_>>();
    external_specifiers.sort_unstable();
    external_specifiers.dedup();
    let external_indexes = external_specifiers
        .iter()
        .enumerate()
        .map(|(index, specifier)| (specifier.clone(), index))
        .collect::<HashMap<String, usize>>();
```

Iterate `included_modules` in the existing loop (replacing the inline filter) and thread `&external_indexes` through `rewrite_module` into `rename_map`.

3. In `rename_map`, after the internal-import loop:

```rust
    for ext in &module.external_imports {
        if ext.local_name.is_empty() {
            continue;
        }
        if let Some(index) = external_indexes.get(&ext.specifier) {
            renames.insert(
                ext.local_name.clone(),
                external_binding_name(*index, &ext.imported_name),
            );
        }
    }
```

4. Replace the synthetic-import emission body (drop `default_local`/`namespace_local` last-writer-wins):

```rust
    for specifier in specifiers {
        let index = external_indexes[specifier];
        let edges = deduplicated_external_imports.get(specifier).unwrap();
        let mut has_default = false;
        let mut has_namespace = false;
        let mut named_imports = Vec::new();
        let mut has_bindings = false;

        for edge in edges {
            match edge.imported_name.as_str() {
                "" => {}
                "default" => has_default = true,
                "*" => has_namespace = true,
                name => named_imports.push(name.to_owned()),
            }
            has_bindings |= !edge.imported_name.is_empty();
        }

        if has_default {
            writeln!(
                synthetic_imports,
                "import {} from '{specifier}';",
                external_binding_name(index, "default")
            )
            .expect("writing to String should not fail");
        }
        if has_namespace {
            writeln!(
                synthetic_imports,
                "import * as {} from '{specifier}';",
                external_binding_name(index, "*")
            )
            .expect("writing to String should not fail");
        }
        if !named_imports.is_empty() {
            named_imports.sort_unstable();
            named_imports.dedup();
            let named = named_imports
                .iter()
                .map(|name| format!("{name} as {}", external_binding_name(index, name)))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(synthetic_imports, "import {{ {named} }} from '{specifier}';")
                .expect("writing to String should not fail");
        }
        if !has_bindings {
            writeln!(synthetic_imports, "import '{specifier}';")
                .expect("writing to String should not fail");
        }
    }
```

- [ ] **Step 4: Run the full suite**; update `bundle_hoists_and_deduplicates_external_imports` (and any other assertion pinning old external local names) to the canonical `__il_ext{N}_` names; note every changed assertion in the commit body.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/bundle.rs daemon/tests/bundle.rs daemon/tests/review_repros.rs
git commit -m "fix(bundle): give external imports canonical collision-free locals" -m "Two modules importing the same local name from different external packages produced duplicate top-level bindings in the synthetic import header, so semantic validation failed and analysis silently fell back to low-confidence static-entry sizing. Distinct default/namespace locals for one external also lost all but the last binding. Derive one canonical local per (external specifier, imported name) and rename module locals to it."
```

(Also delete `repro_external_local_name_collision_breaks_minify` from `review_repros.rs` in this commit.)

---

### Task T4: Fix completion bailing on earlier brace-less imports — B4

**Files:**
- Modify: `daemon/src/document/completion.rs:60-74`
- Test: `daemon/src/document/completion.rs` (new `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing unit test**

```rust
#[cfg(test)]
mod tests {
    use super::named_import_completion_context;

    #[test]
    fn completion_skips_earlier_imports_without_braces() {
        let source = "import React from 'react';\nimport { map } from 'lodash';\n";
        let cursor = source.rfind('{').expect("brace should exist") + 1;

        let context =
            named_import_completion_context(source, cursor).expect("completion context");

        assert_eq!(context.specifier, "lodash");
        assert_eq!(context.imported_names, vec!["map"]);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --lib completion_skips_earlier_imports` → FAIL (returns `None`).

- [ ] **Step 3: Replace the `?` early-return with `continue`**

```rust
    for mut group in groups {
        let Some(range) = named_import_member_range(source, group.statement_span) else {
            continue;
        };
        if offset < range.start || offset > range.end {
            continue;
        }
        ...
    }
```

- [ ] **Step 4: Run suite**; delete `repro_completion_bails_on_earlier_braceless_import` from `review_repros.rs`.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/document/completion.rs daemon/tests/review_repros.rs
git commit -m "fix(document): don't abort member completion on brace-less imports" -m "completion_context_from_module_record used ? on named_import_member_range inside its group loop, so any earlier default or namespace import statement (import React from 'react') returned None for the whole document and suppressed member completion for every later import."
```

---

### Task T5: Region-aware completion with UTF-16 cursor conversion — B5 + B6

**Files:**
- Modify: `daemon/src/document/completion.rs` (signature + region loop + offset conversion)
- Modify: `daemon/src/service.rs:712` (caller passes the document path)
- Test: `daemon/src/document/completion.rs` tests module

**Interfaces:**
- Produces: `pub fn named_import_completion_context(filename: &str, source: &str, utf16_cursor_offset: usize) -> Option<NamedImportCompletionContext>` (gains `filename`; the offset parameter is now explicitly UTF-16 code units, matching what `extension/src/ui/completions.ts:43` sends via `document.offsetAt`).
- Consumes: `super::script_regions::{ScriptRegion, script_regions_for_document}`.

- [ ] **Step 1: Write failing tests** (extend the T4 test module; update the T4 test to the new signature with filename `"main.tsx"`)

```rust
    #[test]
    fn completion_works_inside_vue_script_blocks() {
        let source = "<template><div /></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue';\n</script>\n";
        let cursor = source.rfind('{').expect("brace should exist") + 1;

        let context = named_import_completion_context("component.vue", source, cursor)
            .expect("completion context inside script block");

        assert_eq!(context.specifier, "vue");
        assert_eq!(context.imported_names, vec!["ref"]);
    }

    #[test]
    fn completion_parses_ts_documents_as_typescript_not_tsx() {
        let source = "import { map } from 'lodash';\nconst value = <string>JSON.parse('\"x\"');\n";
        let cursor = source.find('{').expect("brace should exist") + 1;

        let context = named_import_completion_context("main.ts", source, cursor)
            .expect("angle-bracket assertion should not break completion");

        assert_eq!(context.specifier, "lodash");
    }

    #[test]
    fn completion_accepts_utf16_cursor_offsets() {
        let source = "const s = '\u{20AC}\u{20AC}';\nimport { map } from 'lodash';\n";
        let byte_cursor = source.rfind('{').expect("brace should exist") + 1;
        let utf16_cursor: usize = source[..byte_cursor].chars().map(char::len_utf16).sum();
        assert_ne!(byte_cursor, utf16_cursor);

        let context = named_import_completion_context("main.ts", source, utf16_cursor)
            .expect("UTF-16 offset should resolve");

        assert_eq!(context.specifier, "lodash");
    }
```

- [ ] **Step 2: Run** — all three FAIL today (vue: TSX parse of markup; ts: TSX parse of assertion; utf16: byte comparison misses).

- [ ] **Step 3: Implement**

```rust
use super::script_regions::{ScriptRegion, script_regions_for_document};

pub fn named_import_completion_context(
    filename: &str,
    source: &str,
    utf16_cursor_offset: usize,
) -> Option<NamedImportCompletionContext> {
    let offset = byte_offset_for_utf16(source, utf16_cursor_offset);

    for region in script_regions_for_document(filename, source) {
        let region_end = region.offset + region.source.len();
        if offset < region.offset || offset > region_end {
            continue;
        }

        if let Some(context) = region_completion_context(&region, offset - region.offset) {
            return Some(context);
        }
    }

    None
}

fn byte_offset_for_utf16(source: &str, utf16_offset: usize) -> usize {
    let mut utf16_seen = 0;

    for (byte_index, char) in source.char_indices() {
        if utf16_seen >= utf16_offset {
            return byte_index;
        }
        utf16_seen += char.len_utf16();
    }

    source.len()
}

fn region_completion_context(
    region: &ScriptRegion<'_>,
    offset: usize,
) -> Option<NamedImportCompletionContext> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(Path::new(&region.filename))
        .unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, region.source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return None;
    }

    completion_context_from_module_record(region.source, offset, &parsed.module_record)
}
```

(Note: for pure-ASCII sources `byte_offset_for_utf16` is the identity, so existing behavior is preserved. The old whole-document TSX parse is replaced by per-region `from_path` — plain files produce one region carrying the real filename, `script_regions.rs:41-46`.)

Update `service.rs::complete_import_members`:

```rust
        let Some(context) = named_import_completion_context(
            &request.active_document_path,
            &request.source,
            request.cursor_offset,
        ) else { ... };
```

- [ ] **Step 4: Run the full suite** (`service_completes_import_members_from_document_context` covers the service path). Delete `repro_completion_cursor_offset_is_bytes_but_client_sends_utf16` from `review_repros.rs`.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/document/completion.rs daemon/src/service.rs daemon/tests/review_repros.rs
git commit -m "feat(document): complete import members inside component script blocks" -m "Completion parsed the entire document as TSX with a hardcoded synthetic filename, so it never produced results in .vue/.svelte/.astro files (which the extension registers completion for) and broke on valid .ts angle-bracket type assertions. It also compared the client cursor (UTF-16 code units from VS Code's offsetAt) against byte offsets, silently missing whenever non-ASCII text preceded the cursor. Reuse the script-region extraction the import analyzer already uses, parse each region with its real SourceType, and convert the cursor to a byte offset first."
```

---

### Task T6: Parse JSX in plain-JS document regions — B9

**Files:**
- Modify: `daemon/src/document/imports.rs:36-37` (region source type)
- Test: `daemon/tests/document_analysis.rs` (append)

**Interfaces:**
- Produces (private, in `imports.rs`): `fn region_source_type(filename: &str) -> SourceType` — also reused conceptually by T5's completion (completion keeps its own copy local to `completion.rs` or imports this one if visibility allows; prefer moving the helper to `script_regions.rs` as `pub(super) fn source_type_for_region(filename: &str) -> SourceType` and using it from both).

- [ ] **Step 1: Write the failing test** (append to `daemon/tests/document_analysis.rs`, following that file's existing helpers for calling `analyze_imports`)

```rust
#[test]
fn jsx_in_plain_js_documents_still_analyzes() {
    let imports = import_lens_daemon::document::analyze_imports(
        "App.js",
        "import { useState } from 'react';\nexport const App = () => <div />;\n",
    )
    .expect("JSX in .js should analyze");

    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].specifier, "react");
}

#[test]
fn comparison_chains_in_plain_js_still_parse_with_jsx_enabled() {
    let imports = import_lens_daemon::document::analyze_imports(
        "math.js",
        "import { clamp } from 'lodash';\nexport const inRange = (a, b, c) => a < b && b > c;\n",
    )
    .expect("comparison operators must keep parsing");

    assert_eq!(imports.len(), 1);
}
```

- [ ] **Step 2: Run** — first test FAILS today (`.js` parses as JavaScript/Standard; JSX is a syntax error), second passes (guards against regressions from enabling JSX).

- [ ] **Step 3: Implement** — in `script_regions.rs`:

```rust
pub(super) fn source_type_for_region(filename: &str) -> SourceType {
    let source_type =
        SourceType::from_path(Path::new(filename)).unwrap_or_else(|_| SourceType::mjs());

    // JSX in plain .js is widespread (CRA-era apps, React Native). Enabling the
    // JSX variant only accepts a superset: a bare `<` can never start a valid
    // plain-JS expression, so no existing program changes meaning.
    if source_type.is_javascript() {
        return SourceType::jsx();
    }

    source_type
}
```

Add the needed `use oxc_span::SourceType; use std::path::Path;` imports. In `imports.rs::imports_from_region` replace:

```rust
    let source_type =
        SourceType::from_path(Path::new(&region.filename)).unwrap_or_else(|_| SourceType::mjs());
```

with:

```rust
    let source_type = super::script_regions::source_type_for_region(&region.filename);
```

In T5's `region_completion_context`, use the same helper instead of the local `from_path` call (keeps document analysis and completion consistent).

Caveat to verify while implementing: `SourceType::jsx()`'s module kind (oxc_span `:315`). If it is not `Unambiguous`, construct the type by taking `from_path`'s result and switching only the language variant via whatever mutator 0.138 exposes (check `source_type.rs` for `with_` methods; if none exists, `SourceType::from_path(Path::new("x.jsx"))` gives JavaScript+Jsx+Unambiguous and is the safe constructor). `.cjs`/`.mjs`/`.cts`/`.mts` keep their explicit module kinds — only widen when `from_path` yielded plain JavaScript; if preserving CommonJS/Module kinds matters for the widening, map `.cjs → SourceType::jsx() is wrong` — instead only widen for the `Unambiguous` JavaScript case and leave explicit-kind files untouched.

- [ ] **Step 4: Run the full suite** — green. Delete `repro_jsx_in_js_documents_fails_analysis` from `review_repros.rs`.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/document/script_regions.rs daemon/src/document/imports.rs daemon/src/document/completion.rs daemon/tests/document_analysis.rs daemon/tests/review_repros.rs
git commit -m "fix(document): analyze JSX inside plain .js documents" -m "SourceType::from_path maps .js to JavaScript without the JSX variant, so a CRA-style App.js containing JSX failed to parse and the whole document lost its import hints silently. Enabling JSX for plain JavaScript regions accepts a strict superset of programs."
```

(Note: packages *shipping* JSX in `.js` still fail at the module-graph layer — that is deferred item DF-6, which this task deliberately does not attempt.)

---

### Task T7: Recognize bare Node builtin subpaths — B7

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:1345-1389` (`is_node_builtin_specifier`)
- Test: `daemon/tests/document_analysis.rs` (append)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn builtin_subpath_specifiers_are_not_runtime_packages() {
    for specifier in [
        "fs/promises",
        "dns/promises",
        "stream/promises",
        "stream/web",
        "stream/consumers",
        "timers/promises",
        "readline/promises",
        "path/posix",
        "path/win32",
        "util/types",
        "assert/strict",
        "inspector/promises",
    ] {
        assert!(
            !import_lens_daemon::document::is_runtime_package_specifier(specifier),
            "{specifier} should be treated as a Node builtin"
        );
    }

    assert!(import_lens_daemon::document::is_runtime_package_specifier(
        "fs-extra"
    ));
}
```

- [ ] **Step 2: Run** → FAIL on `fs/promises`.

- [ ] **Step 3: Extend the `matches!` list** in `is_node_builtin_specifier` with exactly the bare-importable subpath forms (alphabetical, merged into the existing list):

```rust
            | "assert/strict"
            | "dns/promises"
            | "fs/promises"
            | "inspector/promises"
            | "path/posix"
            | "path/win32"
            | "readline/promises"
            | "stream/consumers"
            | "stream/promises"
            | "stream/web"
            | "timers/promises"
            | "util/types"
```

(Do NOT add prefix-only builtins like `test`/`sea`/`sqlite` — those require the `node:` prefix in Node, and `node:`-prefixed specifiers are already filtered by the URL-scheme rule in `specifier.rs:43-56`; bare `test` is a legitimate npm package name.)

- [ ] **Step 4: Run suite**; delete `repro_builtin_subpath_treated_as_package` from `review_repros.rs`.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/tests/document_analysis.rs daemon/tests/review_repros.rs
git commit -m "fix(graph): treat bare builtin subpaths like fs/promises as Node builtins" -m "import { readFile } from 'fs/promises' was classified as a runtime import of an npm package named fs, so the editor decorated a valid Node import with an inline 'Package not found' hint."
```

---

### Task T8: Handle `eval`/`arguments` keys in synthetic JSON modules — B8

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:602-658` (`is_safe_js_identifier` blocklist)
- Test: `daemon/tests/graph.rs` (append, reusing that file's temp-workspace/write helpers — read the file's helper names first and follow them)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn json_modules_with_strict_mode_restricted_keys_still_build() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import data from './data.json';\nexport const value = data;",
    );
    write_source(&root, "data.json", "{\"eval\": 1, \"arguments\": 2, \"safe\": 3}");

    let graph = build_module_graph(&root.join("entry.js"));

    fs::remove_dir_all(root).expect("temp workspace should be removed");
    let graph = graph.expect("JSON with eval/arguments keys should build");
    let json_module = graph
        .modules
        .iter()
        .find(|module| module.path.extension().is_some_and(|ext| ext == "json"))
        .expect("json module should be in the graph");
    assert!(json_module.source.contains("export const safe"));
    assert!(!json_module.source.contains("export const eval"));
    assert!(!json_module.source.contains("export const arguments"));
}
```

- [ ] **Step 2: Run** → FAIL (graph build errors on the synthetic module).

- [ ] **Step 3: Add `"eval" | "arguments"`** to the `!matches!` blocklist in `is_safe_js_identifier` (binding either name is a strict-mode SyntaxError; modules are always strict, so the synthesized `export const eval = …` cannot parse).

- [ ] **Step 4: Run suite**; delete `repro_json_eval_key_breaks_graph` from `review_repros.rs`.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/tests/graph.rs daemon/tests/review_repros.rs
git commit -m "fix(graph): skip eval/arguments named exports in synthetic JSON modules" -m "A JSON module containing an 'eval' or 'arguments' key synthesized 'export const eval = ...', a strict-mode SyntaxError, so the entire module graph (and every import touching the JSON) failed to analyze."
```

---

### Task T9: Deflake the package.json streaming server test — B10

**Files:**
- Modify: `daemon/tests/server.rs:649-654`

- [ ] **Step 1: Widen the first-partial timeout** from 200 ms to 10 s. The assertion that matters — the first frame read is a partial (`indexes == Some([0,1])`, asserted at `:656`) — is order-based; the timeout only bounds the wait:

```rust
    let first_partial = tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_response(&mut client_stream),
    )
```

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --test server` three times → 7 passed each.

- [ ] **Step 3: Commit**

```bash
git add daemon/tests/server.rs
git commit -m "test(server): deflake streaming package.json partial-frame assertion" -m "The 200ms timeout for the first partial frame races analysis startup under parallel test load (observed once on a cold full-suite run). The ordering guarantee is proven by the frame's indexes field, not by wall time."
```

---

### Task T10: Remove verified dead code — D1..D5

**Files:**
- Modify: `daemon/src/document/script_regions.rs:49-68,311-314`, `daemon/src/pipeline/minify.rs:95-124`, `daemon/src/pipeline/analyze.rs:157-162`, `daemon/src/pipeline/cjs_scan.rs:456-458`, `daemon/src/document/ignore.rs:94-96`

- [ ] **Step 1: Delete each item**

1. `script_regions.rs`: delete `language_from_filename` + `_language_from_filename_for_tests` (D1). **Ordering note:** if T6 landed first, `source_type_for_region` now lives here — do not confuse the two; only the `ScriptLanguage`-returning pair is dead.
2. `minify.rs`: reduce `is_import_lens_marker_statement` to the export-form check (D2); delete `is_import_lens_marker_export_statement` and now-unused `Expression`/`CallExpression` imports:

```rust
fn is_import_lens_marker_statement(statement: &Statement<'_>) -> bool {
    let Statement::ExportNamedDeclaration(export) = statement else {
        return false;
    };

    export.declaration.is_none()
        && export.source.is_none()
        && !export.specifiers.is_empty()
        && export.specifiers.iter().all(|specifier| {
            module_export_name(&specifier.exported).starts_with("__importLensUse_")
        })
}
```

3. `analyze.rs`: replace the always-true condition (D3) with `if !is_cjs {`; drop `ImportKind` from imports if now unused in that file (it is still used elsewhere in `analyze.rs` — check `diagnostic_requested_exports` first; it is, so keep the import).
4. `cjs_scan.rs`: delete `is_boundary` (D4); `literal_requires` calls `is_identifier_boundary` directly.
5. `ignore.rs`: delete `glob_matches` (D5); the two `should_ignore_import` arms call `glob_matches_exact`.

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon && cargo clippy -p import-lens-daemon --all-targets` → green, no new warnings.

- [ ] **Step 3: Commit**

```bash
git add daemon/src/document/script_regions.rs daemon/src/pipeline/minify.rs daemon/src/pipeline/analyze.rs daemon/src/pipeline/cjs_scan.rs daemon/src/document/ignore.rs
git commit -m "refactor: remove dead code paths" -m "language_from_filename and its test wrapper have no callers; the __importLensUse(...) call-marker branch checks a form no code generates (markers are always export statements); the ImportKind matches! lists every enum variant and is always true; is_boundary and glob_matches were pass-through wrappers."
```

---

### Task T11: Consolidate duplicated helpers — R1..R4

**Files:**
- Create: `daemon/src/time.rs`
- Modify: `daemon/src/lib.rs`, `daemon/src/lifecycle.rs`, `daemon/src/cache/disk.rs`, `daemon/src/cache/project.rs`, `daemon/src/ipc/server.rs`, `daemon/src/ipc/protocol.rs`, `daemon/src/service.rs`

**Interfaces:**
- Produces: `pub fn crate::time::unix_millis(time: SystemTime) -> u64`; `pub fn crate::time::unix_millis_now() -> u64`; `pub fn protocol::is_supported_protocol_version(version: u32) -> bool`; `impl ImportDiagnostic { pub fn for_stage(stage: &str, message: impl Into<String>) -> Self }`; `#[derive(Default)]` on `WorkspaceReportSummary`.

- [ ] **Step 1: Create `daemon/src/time.rs`**

```rust
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_millis(time: SystemTime) -> u64 {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

pub fn unix_millis_now() -> u64 {
    unix_millis(SystemTime::now())
}
```

Register in `lib.rs` (alphabetical): `pub mod time;`

- [ ] **Step 2: Replace the five copies (R1)** — delete `lifecycle.rs::unix_millis`, `disk.rs::unix_millis_now`, `project.rs::unix_millis_now`, `server.rs::current_time_millis` (incl. its comment block), `service.rs::current_time_millis`; import from `crate::time`.

- [ ] **Step 3: Single version check (R2)** — add to `protocol.rs` below `PROTOCOL_VERSION`:

```rust
pub fn is_supported_protocol_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
}
```

Delete both `server.rs` copies (`is_supported_protocol_version`, `is_supported_hello_version` — identical ranges) and the `service.rs` copy; import from `protocol`.

- [ ] **Step 4: Single diagnostics constructor (R3)** — add to `protocol.rs`:

```rust
impl ImportDiagnostic {
    pub fn for_stage(stage: &str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.to_owned(),
            message: message.into(),
            details: Vec::new(),
        }
    }
}
```

Replace and delete `server.rs::protocol_diagnostics_for_stage`, `server.rs::protocol_diagnostics`, `service.rs::protocol_diagnostics` (call sites become `vec![ImportDiagnostic::for_stage(stage, message)]`). Local `diagnostic(...)` helpers that also fill `details` (`cjs.rs`, `file_size.rs`) are NOT part of this — they carry details and stay.

- [ ] **Step 5: `Default` for the empty summary (R4)** — add `Default` to `WorkspaceReportSummary`'s derives; delete `service.rs::empty_workspace_report_summary` and the hand-built summary in `server.rs::workspace_report_protocol_error`; both call sites use `WorkspaceReportSummary::default()`. (All-`u64`/`Vec` fields ⇒ derive is exactly the previous value. Serde output unchanged — `Default` is not a serde trait.)

- [ ] **Step 6: Run** `cargo test -p import-lens-daemon && cargo clippy -p import-lens-daemon --all-targets` → green.

- [ ] **Step 7: Commit**

```bash
git add daemon/src/time.rs daemon/src/lib.rs daemon/src/lifecycle.rs daemon/src/cache/disk.rs daemon/src/cache/project.rs daemon/src/ipc/server.rs daemon/src/ipc/protocol.rs daemon/src/service.rs
git commit -m "refactor: consolidate duplicated time, version-check and diagnostic helpers" -m "unix-millis conversion existed five times, the protocol version range check three times, the single-diagnostic constructor three times, and the empty workspace report summary was hand-built twice."
```

---

### Task T12: Unify the analyze/prewarm caching tail in `service.rs` — R5

**Files:**
- Modify: `daemon/src/service.rs:1014-1077,1142-1173`

**Interfaces:**
- Produces (private): `fn analyze_and_cache(&self, cache: &ImportCache, context: &AnalysisContext, request: &ImportRequest, key: String, resolved: ResolvedPackage, should_store: impl Fn() -> bool) -> ImportResult`.

- [ ] **Step 1: Extract the shared tail**

```rust
    fn analyze_and_cache(
        &self,
        cache: &ImportCache,
        context: &AnalysisContext,
        request: &ImportRequest,
        key: String,
        resolved: ResolvedPackage,
        should_store: impl Fn() -> bool,
    ) -> ImportResult {
        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) && should_store() {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(cache, request, &result, &resolved, &fingerprints);
            cache.insert_with_fingerprints(key, result.clone(), fingerprints);
        }

        result
    }
```

Rewrite the three callers:

```rust
    pub fn prewarm_import<F>(&self, context: &AnalysisContext, request: &ImportRequest, should_continue: F)
    where
        F: Fn() -> bool,
    {
        let Ok(resolved) = resolve_package_entry(&context.active_document_path, request) else {
            return;
        };
        self.prewarm_resolved_import(context, request, resolved, should_continue);
    }

    pub fn prewarm_resolved_import<F>(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        resolved: ResolvedPackage,
        should_continue: F,
    ) where
        F: Fn() -> bool,
    {
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        if cache.get(&key).is_some() || !should_continue() {
            return;
        }

        let _ = self.analyze_and_cache(cache.as_ref(), context, request, key, resolved, should_continue);
    }

    fn analyze_with_cache(&self, context: &AnalysisContext, request: &ImportRequest) -> ImportResult {
        let resolved = match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => resolved,
            Err(_) => return analyze_import(context, request),
        };
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        if let Some(result) = cache.get(&key) {
            return result;
        }

        self.analyze_and_cache(cache.as_ref(), context, request, key, resolved, || true)
    }
```

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon` → PASS (prefetch/service/performance suites cover all three paths).

- [ ] **Step 3: Commit**

```bash
git add daemon/src/service.rs
git commit -m "refactor(service): share the analyze-and-cache tail across request and prewarm paths" -m "analyze_with_cache, prewarm_import and prewarm_resolved_import each hand-rolled the same resolve/key/probe/analyze/alias/insert sequence; behavior is unchanged."
```

---

### Task T13: Replace the 36-arm `statement_span` with `GetSpan` — R6

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:1061-1116`

- [ ] **Step 1:** Add `GetSpan` to the `oxc_span` import; delete `fn statement_span`; in `statement_binding_ranges` replace `let span = statement_span(statement)?;` with `let span = statement.span();`.

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon` → PASS (`impl GetSpan for Statement` confirmed in oxc_ast 0.138 `generated/derive_get_span.rs:583`).

- [ ] **Step 3: Commit**

```bash
git add daemon/src/pipeline/graph.rs
git commit -m "refactor(graph): use oxc GetSpan instead of a hand-written statement span match"
```

---

### Task T14: Remove redundant canonicalize calls and double key decode — P6 + P7

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:329-345`, `daemon/src/pipeline/cjs.rs:41-43`, `daemon/src/prefetch.rs:248-257`

- [ ] **Step 1: graph.rs** — `load_module_from` receives `importer` already canonicalized (it is the caller's own `path` from `:334`). Replace:

```rust
                let importer = normalize_existing_path(importer)?;
                if self.circular_edges.insert((importer.clone(), path.clone())) {
```

with:

```rust
                if self
                    .circular_edges
                    .insert((importer.to_path_buf(), path.clone()))
                {
```

(and use `importer.display()` in the diagnostic details).

- [ ] **Step 2: cjs.rs** — queue entries are the pre-normalized entry (`:30`) or `resolve_module_path` outputs (canonicalized in `resolver.rs:607`). Drop the loop-top re-normalize:

```rust
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
```

- [ ] **Step 3: prefetch.rs** — build the request from the already-decoded identity instead of decoding the key twice:

```rust
        .filter_map(|key| {
            let identity = decode_cache_identity(&key)?;
            let resolved = resolved_from_cache_identity(&identity)?;
            let request = ImportRequest {
                specifier: identity.specifier,
                package_name: identity.package_name,
                version: identity.package_version,
                named: identity.named_exports,
                import_kind: identity.import_kind,
                runtime: identity.runtime,
            };
            Some(PrewarmJob { request, resolved })
        })
```

(`cached_import_request_from_key` stays — `daemon/tests/prefetch.rs` exercises it. Field-move order matters: take `resolved` **before** moving `identity` fields, since `resolved_from_cache_identity` borrows `&identity`.)

- [ ] **Step 4: Run** full suite → PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/src/pipeline/cjs.rs daemon/src/prefetch.rs
git commit -m "perf: drop redundant canonicalize syscalls and duplicate cache-key decodes" -m "Module loaders re-canonicalized paths that were canonicalized one frame earlier (fs::canonicalize opens the file on Windows), and the recent-prewarm path msgpack-decoded every cache key twice."
```

---

### Task T15: Reuse one ureq Agent per registry client — P1

**Files:**
- Modify: `daemon/src/registry/client.rs`

- [ ] **Step 1: Build the agent once**

```rust
#[derive(Debug, Clone)]
pub struct UreqRegistryHttpClient {
    agent: ureq::Agent,
}

impl Default for UreqRegistryHttpClient {
    fn default() -> Self {
        Self::new(DEFAULT_TIMEOUT_MS)
    }
}

impl UreqRegistryHttpClient {
    pub fn new(timeout_ms: u64) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}
```

`get_package_metadata` uses `self.agent.get(&url)`; the per-call agent construction and the `timeout_ms` field disappear. (`SystemTime` import stays — `retry_after_delay_ms` uses it.)

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --test registry && cargo build -p import-lens-daemon` → PASS (registry tests inject fake clients; the build proves the ureq path).

- [ ] **Step 3: Commit**

```bash
git add daemon/src/registry/client.rs
git commit -m "perf(registry): reuse a single ureq agent for connection pooling" -m "A new Agent (fresh connection pool + TLS handshake) was created per package metadata request; refreshing N packages paid N handshakes to registry.npmjs.org."
```

---

### Task T16: Flush only dirty cache entries on recycle — P2

**Files:**
- Modify: `daemon/src/cache/memory.rs`
- Test: `daemon/tests/memory_cache.rs` (append; adapt constructor/result helper names to that file's existing helpers)

**Interfaces:**
- `ImportCache` gains `dirty: Mutex<HashSet<String>>`; `flush_to_disk` replays only keys whose disk insert failed. Public API unchanged. Existing test `cache_disk.rs:406` remains valid (its entry persisted at insert time).

- [ ] **Step 1: Write the test**

```rust
#[test]
fn flush_to_disk_succeeds_with_nothing_dirty() {
    let storage = temp_storage();
    let cache = ImportCache::new(Some(storage.clone()), true);
    cache.insert("v3:00".to_owned(), result("pkg"));
    cache.flush_to_disk().expect("flush should succeed");

    let memory_only = ImportCache::new(None, false);
    memory_only.insert("v3:01".to_owned(), result("pkg2"));
    memory_only.flush_to_disk().expect("flush should succeed");

    fs::remove_dir_all(storage).expect("cleanup");
}
```

- [ ] **Step 2: Implement**

```rust
use std::{collections::HashSet, path::PathBuf, sync::Mutex};

#[derive(Debug)]
pub struct ImportCache {
    memory: HashMap<String, CachedImport>,
    disk: DiskCache,
    dirty: Mutex<HashSet<String>>,
}
```

- Both constructors + `Default` initialize `dirty: Mutex::new(HashSet::new())`.
- `insert_with_fingerprints` on disk error records the key:

```rust
        if let Err(error) = self.disk.insert(&key, &cached) {
            crate::logging::log_warn("cache", format!("skipping disk insert for {key}: {error}"));
            if let Ok(mut dirty) = self.dirty.lock() {
                dirty.insert(key.clone());
            }
        }
```

- `flush_to_disk` replays only dirty keys, restoring them on failure:

```rust
    pub fn flush_to_disk(&self) -> Result<(), String> {
        let dirty_keys = match self.dirty.lock() {
            Ok(mut dirty) => std::mem::take(&mut *dirty),
            Err(_) => return Err("cache dirty-set lock poisoned".to_owned()),
        };

        let entries = {
            let memory = self.memory.pin();
            dirty_keys
                .iter()
                .filter_map(|key| memory.get(key).map(|cached| (key.clone(), cached.clone())))
                .collect::<Vec<_>>()
        };

        for (key, cached) in entries {
            if let Err(error) = self.disk.insert(&key, &cached) {
                if let Ok(mut dirty) = self.dirty.lock() {
                    dirty.extend(dirty_keys);
                }
                return Err(error);
            }
        }

        self.disk.flush_pending_touches();
        Ok(())
    }
```

- `clear` also clears the dirty set.

- [ ] **Step 3: Run** full suite → PASS.

- [ ] **Step 4: Commit**

```bash
git add daemon/src/cache/memory.rs daemon/tests/memory_cache.rs
git commit -m "perf(cache): flush only failed disk inserts on recycle" -m "flush_to_disk re-wrote every in-memory entry as its own committed redb transaction although inserts already persist synchronously — a recycle with a large cache issued up to 200k redundant transactions. Track keys whose disk insert failed and replay just those."
```

---

### Task T17: Single-scan cache cleanup — P3

**Files:**
- Modify: `daemon/src/cache/project.rs:150-207`

- [ ] **Step 1: Rework `cleanup` around one `list_shards()` call**

```rust
    pub fn cleanup(&self) -> ProjectCacheCleanup {
        let now = crate::time::unix_millis_now();
        let max_age_millis = self.max_age_days.saturating_mul(24 * 60 * 60 * 1000);
        let max_size_bytes = self.max_size_mb.saturating_mul(1024 * 1024);
        let mut removed = Vec::new();
        let mut failed = Vec::new();
        let mut remaining = Vec::new();

        for shard in self.list_shards() {
            let expired = shard
                .last_used_millis
                .is_some_and(|last_used| now.saturating_sub(last_used) > max_age_millis);

            if expired {
                let result = self.remove_shard_by_id(&shard.shard_id);
                push_operation_result(result, &mut removed, &mut failed);
            } else {
                remaining.push(shard);
            }
        }

        let mut total_size_bytes = remaining.iter().map(|shard| shard.size_bytes).sum::<u64>();

        if max_size_bytes > 0 && total_size_bytes > max_size_bytes {
            remaining.sort_by(|left, right| {
                left.last_used_millis
                    .unwrap_or(0)
                    .cmp(&right.last_used_millis.unwrap_or(0))
            });

            for shard in remaining {
                if total_size_bytes <= max_size_bytes {
                    break;
                }

                let size_bytes = shard.size_bytes;
                let result = self.remove_shard_by_id(&shard.shard_id);
                if result.removed {
                    total_size_bytes = total_size_bytes.saturating_sub(size_bytes);
                }
                push_operation_result(result, &mut removed, &mut failed);
            }
        }

        if let Ok(mut last_cleanup) = self.last_cleanup_millis.lock() {
            *last_cleanup = Some(now);
        }

        ProjectCacheCleanup {
            total_size_bytes,
            removed,
            failed,
        }
    }
```

Behavior preserved: age-expired shards leave `remaining` regardless of removal success (matching the old `removed_ids` exclusion). Semantic delta, stated in the commit: the returned total is the arithmetic remainder from the single scan instead of a third full walk. If a `tests/project_cache.rs` cleanup test asserts a re-walked total, adjust it and call the change out in the commit body.

**Timing note:** T11 moves `unix_millis_now` to `crate::time`. If this task runs before T11, use the local `unix_millis_now()` and let T11 swap it.

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --test project_cache` then full suite → PASS.

- [ ] **Step 3: Commit**

```bash
git add daemon/src/cache/project.rs
git commit -m "perf(cache): scan shard directory once during cleanup" -m "cleanup() called list_shards twice and total_size_bytes once more, so every hello-triggered cleanup walked the entire cache directory tree three times; track remaining shards and adjust totals arithmetically instead."
```

---

### Task T18: Bound the module-graph cache — P4

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:26-31,212-241`
- Test: `daemon/tests/graph.rs` (append; that file serializes graph-cache tests behind a lock — reuse its existing lock helper)

**Interfaces:**
- Produces: `pub const MAX_CACHED_GRAPHS: usize = 32;` and `pub fn module_graph_cache_len() -> usize` (test seam).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn graph_cache_evicts_least_recently_used_beyond_cap() {
    let _guard = graph_cache_lock();
    clear_module_graph_cache();
    let root = temp_workspace();

    for index in 0..(MAX_CACHED_GRAPHS + 3) {
        let entry = format!("entry{index}.js");
        write_source(&root, &entry, &format!("export const value{index} = {index};"));
        build_module_graph_cached(&root.join(&entry)).expect("graph should build");
    }

    fs::remove_dir_all(root).expect("temp workspace should be removed");
    assert!(module_graph_cache_len() <= MAX_CACHED_GRAPHS);
    clear_module_graph_cache();
}
```

(Adapt the lock helper name to the file's existing pattern.)

- [ ] **Step 2: Run** → FAIL (len is cap+3).

- [ ] **Step 3: Implement LRU stamp + eviction**

```rust
pub const MAX_CACHED_GRAPHS: usize = 32;

#[derive(Debug, Clone)]
struct CachedModuleGraph {
    graph: Arc<ModuleGraph>,
    fingerprints: Vec<FileFingerprint>,
    last_used_millis: Arc<std::sync::atomic::AtomicU64>,
}
```

In `build_module_graph_cached_with_runtime`:
- On hit: `cached.last_used_millis.store(crate::time::unix_millis_now(), Ordering::Relaxed)` before returning (same T11 timing note as T17).
- On insert: stamp `last_used_millis: Arc::new(AtomicU64::new(unix_millis_now()))`, then:

```rust
    if pinned.len() > MAX_CACHED_GRAPHS {
        let oldest = pinned
            .iter()
            .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            pinned.remove(&key);
        }
    }
```

Test seam:

```rust
pub fn module_graph_cache_len() -> usize {
    GRAPH_CACHE
        .get()
        .map(|cache| cache.pin().len())
        .unwrap_or(0)
}
```

- [ ] **Step 4: Run** full suite → PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/tests/graph.rs
git commit -m "perf(graph): bound the module graph cache with LRU eviction" -m "GRAPH_CACHE grew without limit and every entry retains the full prepared source of all modules in its graph (up to 100MB per graph), so long sessions across many packages could hold gigabytes; cap at 32 graphs evicting least-recently-used."
```

---

### Task T19: Line-index positions — P5

**Files:**
- Modify: `daemon/src/document/positions.rs` (rewrite around `LineIndex`)
- Modify: `daemon/src/document/imports.rs` (build once per document, thread through)
- Modify: `daemon/src/document/package_json.rs` (build once per call)
- Test: `daemon/src/document/positions.rs` `#[cfg(test)]` module

**Interfaces:**
- Produces: `pub struct LineIndex` with `pub fn new(source: &str) -> Self`, `pub fn position_at(&self, source: &str, offset: usize) -> SourcePosition`, `pub fn range_from_offsets(&self, source: &str, start: usize, end: usize) -> SourceRange`. Free functions `position_at`/`range_from_offsets` are removed (internal callers only — `document/mod.rs` never exported them).

- [ ] **Step 1: Write the tests**

```rust
#[cfg(test)]
mod tests {
    use super::LineIndex;

    #[test]
    fn line_index_handles_lf() {
        let source = "ab\ncd";
        let index = LineIndex::new(source);
        let positions = (0..=source.len())
            .map(|offset| {
                let position = index.position_at(source, offset);
                (position.line, position.character)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)]
        );
    }

    #[test]
    fn line_index_handles_crlf() {
        let source = "ab\r\ncd";
        let index = LineIndex::new(source);
        assert_eq!(index.position_at(source, 2).line, 0);
        assert_eq!(index.position_at(source, 4).line, 1);
        assert_eq!(index.position_at(source, 4).character, 0);
    }

    #[test]
    fn line_index_handles_lone_cr() {
        let source = "a\rb";
        let index = LineIndex::new(source);
        assert_eq!(index.position_at(source, 2).line, 1);
        assert_eq!(index.position_at(source, 2).character, 0);
    }

    #[test]
    fn line_index_counts_utf16_columns() {
        let source = "const s = '\u{1D11E}x';";
        let index = LineIndex::new(source);
        let x_offset = source.find('x').expect("x exists");
        assert_eq!(
            index.position_at(source, x_offset).character,
            "const s = '".len() as u32 + 2
        );
    }

    #[test]
    fn line_index_clamps_out_of_range_offsets() {
        let index = LineIndex::new("ab");
        assert_eq!(index.position_at("ab", 99).character, 2);
    }
}
```

- [ ] **Step 2: Implement**

```rust
use crate::ipc::protocol::{SourcePosition, SourceRange};

pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut line_starts = vec![0];

        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => line_starts.push(index + 1),
                b'\r' => {
                    if bytes.get(index + 1) == Some(&b'\n') {
                        line_starts.push(index + 2);
                        index += 2;
                        continue;
                    }
                    line_starts.push(index + 1);
                }
                _ => {}
            }
            index += 1;
        }

        Self { line_starts }
    }

    pub fn position_at(&self, source: &str, offset: usize) -> SourcePosition {
        let safe_offset = offset.min(source.len());
        let line = self
            .line_starts
            .partition_point(|start| *start <= safe_offset)
            - 1;
        let line_start = self.line_starts[line];
        let character = source[line_start..safe_offset]
            .chars()
            .map(|char| char.len_utf16() as u32)
            .sum();

        SourcePosition {
            line: line as u32,
            character,
        }
    }

    pub fn range_from_offsets(&self, source: &str, start: usize, end: usize) -> SourceRange {
        SourceRange {
            start: self.position_at(source, start),
            end: self.position_at(source, end),
        }
    }
}
```

Contract note: `safe_offset` must sit on a char boundary; all callers pass oxc span boundaries or scanner offsets, and the old implementation carried the same implicit contract. Offsets are never mid-CRLF for the same reason.

- [ ] **Step 3: Update callers** — `imports.rs::analyze_imports` builds `let line_index = LineIndex::new(source);` and threads `&line_index` down to `create_detected_import` (replacing all `position_at(document_source, …)`/`range_from_offsets(…)` calls); `package_json.rs` builds one `LineIndex` per public function and passes it into `dependency_entries_for_section`.

- [ ] **Step 4: Run** full suite → PASS (document_analysis + service tests assert exact line/character values end-to-end).

- [ ] **Step 5: Commit**

```bash
git add daemon/src/document/positions.rs daemon/src/document/imports.rs daemon/src/document/package_json.rs
git commit -m "perf(document): compute source positions via a per-document line index" -m "position_at rescanned the document from byte zero for every lookup and each detected import needs six lookups, making import detection O(imports x document bytes) on every debounced keystroke; build the line-start table once and binary-search it."
```

---

### Task T20: Clear the clippy baseline — P8

**Files:**
- Modify: `daemon/src/ipc/server.rs:274,485-486`, `daemon/src/registry/service.rs:62,74,175`, `daemon/src/report/model.rs:354`

- [ ] **Step 1:** `cargo clippy --fix --lib -p import-lens-daemon --allow-dirty` for the six collapsible-ifs (edition-2024 let-chains); review the diff.

- [ ] **Step 2:** Manual fix in `build_treemap`:

```rust
            percentage: ((row.brotli_bytes * 100) + (total_brotli_bytes / 2))
                .checked_div(total_brotli_bytes)
                .unwrap_or(0),
```

(`treemap_with_zero_total_reports_zero_percentages` in `report/model.rs` tests guards the zero case.)

- [ ] **Step 3: Run** `cargo clippy -p import-lens-daemon --all-targets` → zero warnings; full suite → PASS.

- [ ] **Step 4: Commit**

```bash
git add daemon/src/ipc/server.rs daemon/src/registry/service.rs daemon/src/report/model.rs
git commit -m "chore: clear clippy baseline (collapsible ifs, checked division)"
```

---

### Task T21: Remove the repro harness + final verification

**Files:**
- Delete: `daemon/tests/review_repros.rs`

- [ ] **Step 1:** Confirm every repro graduated (T1–T8 each deleted theirs; `repro_star_cycle_stack_overflow` is covered by T1's bundle test). Delete the file.

- [ ] **Step 2: Final gate**

Run: `cargo test -p import-lens-daemon && cargo clippy -p import-lens-daemon --all-targets && cargo build --release -p import-lens-daemon`
Expected: all green, zero clippy warnings, release build OK.

- [ ] **Step 3: Commit**

```bash
git rm daemon/tests/review_repros.rs
git commit -m "test: drop temporary review repro harness" -m "Every repro graduated into a regression test beside the code it guards."
```

---

## Part C — Deferred backlog (full context for future work)

Each item below is real and verified, but deliberately out of scope: the fix needs its own design, benchmarks, or a product decision. Written so any of them can be picked up cold.

### DF-1: Reuse `oxc_resolver::Resolver` instances (largest perf lever)

- **Context:** Every import analysis calls `resolve_package_entry` (`resolver.rs:85-112`) — *even on cache hits*, because the cache key needs the resolved entry (`service.rs:1147-1154`). Each call builds a fresh `Resolver` via `create_resolver(runtime)` (`resolver.rs:515-517`), and `validate_declared_entry_resolution` (`resolver.rs:208-239`) builds a *second* one per root-import request. `oxc_resolver` keeps its FS/description-file cache inside the `Resolver` instance, so a fresh instance re-stats `node_modules` layouts from scratch every time. `ModuleGraphBuilder` (`graph.rs:311-321`) and `analyze_cjs_graph_with_runtime` (`cjs.rs:39`) also each build their own.
- **Proposed approach:** One shared `Resolver` per `ImportRuntime` (3 total), stored in `ImportLensService` (or a `OnceLock` beside `GRAPH_CACHE`), handed to `resolve_package_entry`/graph/CJS builders by reference. Invalidate by **rebuilding all three** on: `NodeModulesChanged` (`server.rs:637-641`), `CacheInvalidate`/`CacheInvalidateAll`, and daemon hello. `oxc_resolver 11` exposes `Resolver::clear_cache()` — prefer that over rebuild if available in the pinned version.
- **Correctness risk:** a stale resolver cache can return entries for *changed* `node_modules` between watcher debounce windows (extension buffers invalidations for 250 ms, `watcherInvalidation.ts:2`). The import cache's fingerprint check (`memory.rs:57-69`) still rejects stale *results*, but a stale *resolution* could produce a fresh-looking result for a moved entry file. Decide: accept the 250 ms window, or fingerprint the resolution inputs too.
- **Test strategy:** integration test that (1) resolves, (2) swaps the package's `main`, (3) sends `node_modules_changed`, (4) asserts re-resolution sees the new entry. Benchmark with `daemon/tests/performance.rs` fixtures before/after.
- **Effort:** ~1–2 days incl. benchmarks. **Payoff:** biggest single latency win for warm-path requests (per-request re-stat of the node_modules chain disappears).

### DF-2: Precompute bundle rename spans at graph build (second-largest lever)

- **Context:** `bundle.rs::semantic_rename_replacements` (`:554-631`) re-parses and re-runs semantic analysis on **every included module for every bundle request**, even though the module graph itself is cached (`GRAPH_CACHE`). The data it derives (root-scope symbol spans + reference spans + shorthand spans, `:595-628`) depends only on module source — not on the request or reachability — so it is cacheable per module. `graph.rs::binding_dependencies` (`:1007-1059`) already builds a semantic model per module at graph time; the same pass could record rename spans.
- **Proposed approach:** Extend `ModuleRecord` with `root_symbol_spans: Vec<(String, Vec<(usize, usize)>)>` (symbol name → decl+reference spans) and `shorthand_spans: HashSet<(usize, usize)>`, filled in `parse_module` from the semantic pass that `binding_dependencies` already runs (merge the two walks). `semantic_rename_replacements` then becomes a pure lookup — no parse, no semantic.
- **Risks:** memory growth per cached graph (span lists), and subtle behavior drift if the current code's `span_overlaps_replacements` filtering interacts with spans differently — port the `seen_spans`/protected-replacement logic unchanged.
- **Test strategy:** all existing `tests/bundle.rs` tests are byte-exact on bundle output — they are the harness. Add one perf assertion via `tests/performance.rs` if it has timing scaffolding.
- **Effort:** ~2 days. **Payoff:** removes a full parse+semantic per module per cache-miss/file-size request; dominates `file_size_document` latency on multi-package files.

### DF-3: Stop re-statting every dependency fingerprint on every cache hit

- **Context:** `ImportCache::get` verifies `fingerprints_are_current` on each memory hit (`memory.rs:60`), which `fs::metadata`s every module file + package.json of the import's graph (`key.rs:99-105`). A lodash-es-sized graph is hundreds of stats per hit, per import, per debounced keystroke.
- **Proposed approach options:** (a) per-package *generation counter* bumped by `NodeModulesChanged`/invalidation; a cached entry stores the generation it was verified at and skips re-stat while generations match — exact, event-driven, needs the watcher to be reliable; (b) time-boxed trust: re-verify at most every N seconds per key (small `Mutex<HashMap<key, last_verified>>`); simpler, bounded staleness N. The extension already watches `node_modules` (`watcher.ts` → `node_modules_changed`), which argues for (a) with (b) as fallback when no watcher event ever arrived (e.g. excluded folders).
- **Test strategy:** unit tests over the generation/TTL gate; regression: mutate a dep file *without* sending invalidation and assert staleness window ≤ chosen bound; with invalidation, assert immediate miss.
- **Effort:** 1 day. **Payoff:** large on Windows where `fs::metadata` is comparatively expensive.

### DF-4: Batch redb writes during cold batches

- **Context:** `DiskCache::insert` opens a write txn and commits per entry (`disk.rs:103-141`). A cold `analyze_document` with 50 imports commits 50 durable txns while rayon workers contend on redb's single writer.
- **Proposed approach:** an insert-queue in `DiskCache` (Mutex<Vec<(key, envelope)>>) flushed by whichever thread exceeds a size/age threshold in one txn — mirrors the existing `pending_touches` pattern (`disk.rs:143-182`). Recency rows go in the same txn. `get` must consult the queue before the table (or flush-before-read) to keep read-your-writes.
- **Test strategy:** extend `tests/cache_disk.rs` — insert N, crash-free reload sees all; interleave get/insert for read-your-writes.
- **Effort:** ~1 day. **Payoff:** cold-batch latency + less writer contention.

### DF-5: Debounce registry snapshot persistence

- **Context:** `RegistryMetadataCache::write_entry` serializes and rewrites the whole `registry-metadata.json` per package fetched (`registry/cache.rs:47-99`). Refreshing N packages does N full-file rewrites (O(N²) bytes).
- **Proposed approach:** dirty-flag + flush on (a) end of each `RefreshRegistryHints` request (server already knows when the final aggregate response is sent, `server.rs:552-592`) and (b) a small timer/threshold like `RECENCY_TOUCH_FLUSH_BATCH`. Keep last-writer-wins + tmp-rename atomicity.
- **Test strategy:** `tests/registry.rs` already covers persistence round-trips; add "N writes → 1 file write" via an injected counter or file mtime check.
- **Effort:** half a day.

### DF-6: JSX-in-`.js` support in the module graph / bundler

- **Context:** T6 fixes *documents*; packages that ship untranspiled JSX in `.js` (React Native ecosystem: `react-native`, many `@react-native-*` libs declare `main: index.js` with JSX) still fail: `prepare_module_source` only transforms `ts|tsx|mts|cts|jsx` (`graph.rs:590-594`), so a JSX `.js` module hits `parse_module` with `SourceType::mjs()` (`graph.rs:474-479`) → parse error → whole graph fails → LOW-confidence static-entry fallback (`analyze.rs:163-172`).
- **Proposed approach:** in `prepare_module_source`, when a `.js` module's plain parse fails, retry with the JSX variant; on success, run the same transform path used for `.jsx` (`transform_module_source` with an adjusted `SourceType`) so downstream bundling/minification sees plain JS. Do NOT unconditionally parse `.js` as JSX in the graph — the prepared-source pipeline needs the *transform* (minifier input must be JSX-free), so detection has to gate the transform, and transform-by-default for all `.js` would cost a full extra pass per module.
- **Test strategy:** graph test with a two-module package where `index.js` contains JSX; assert graph builds, bundle minifies, and the JSX module was transformed (no `<` tags in prepared source).
- **Effort:** ~1 day. **Payoff:** unlocks accurate sizing for the React Native ecosystem.

### DF-7: Convert `load_module_from` recursion to an explicit worklist

- **Context:** `graph.rs:325-446` recurses per dependency edge. Depth is bounded by the longest acyclic import *chain* (cycles are cut by `path_to_id`), cap 2000 modules. A pathological 2000-deep chain × ~0.5–1 KB frames approaches the 2 MB default stack of rayon/tokio worker threads. Not reproduced with a real package — defensive only.
- **Proposed approach:** DFS with an explicit `Vec<(PathBuf, Option<PathBuf>)>` stack; keep `loading_paths` semantics for cycle diagnostics (push marker frames or track depth at pop). `include_module_with_imports` (`bundle.rs:173-262`) has the same shape but converges by `processed_bindings` and is likewise cycle-safe — same treatment optional.
- **Test strategy:** generated fixture with a 3000-deep chain behind a raised `GraphLimits` — must error on the module cap, not overflow.
- **Effort:** half a day.

### DF-8: Workspace report re-loads `.importlensignore` per file

- **Context:** `build_workspace_report_inner` (`service.rs:222-271`) calls `handle_analyze_document` per scanned file; each call walks ancestors looking for `.importlensignore` (`ignore.rs:38-62`). 2 000 files × depth ~10 = ~20 000 stats per report.
- **Proposed approach:** memoize `directory → Option<ignore rules>` in a per-report `HashMap` threaded through, or hoist a workspace-level rules load once (semantic change: per-directory ignore files would need the memo variant to stay correct).
- **Effort:** small; do together with any report-perf pass. Reports are user-triggered (not per-keystroke), so priority is low.

### DF-9: `cache_remove(current_project)` vs in-flight analyses (Windows)

- **Context:** watch-item, downgraded from bug. `remove_shard_by_id` (`project.rs:320-393`) drops the registry's `Arc<ImportCache>` before `remove_dir_all`, so the redb file handle closes first — *unless* a concurrently running analysis/prewarm still holds a clone (the IPC loop handles `CacheRemove` inline while `spawn_workspace_report`/prefetch jobs may be running, `server.rs:481-498`). Then Windows keeps `importlens.redb` alive (delete-pending) and directory removal fails with a user-visible error; retry succeeds. The UI does report the failure (`cacheManager.ts` → `reportRemoveResponse`).
- **Proposed approach if it ever bites:** track in-flight users per shard (e.g. `Weak` count check with a short bounded wait before `remove_dir_all`), or mark the shard directory for deletion-on-next-hello (a `.pending-delete` marker cleaned in `ProjectCacheRegistry::new`).
- **Test strategy:** integration test holding a second `Arc<ImportCache>` across `remove_current_project` on Windows CI; assert the error is reported and a retry after drop succeeds.

### DF-10: Compact cache keys (hex-msgpack identity is the key)

- **Context:** cache keys are `v3:` + hex(msgpack(CacheIdentityV3)) (`key.rs:107-110`) — routinely 400–1000+ chars — used as redb keys and memory map keys; `decode_cache_identity` re-parses them for invalidation matching (`cache_key_matches_package`, per key per invalidate) and for prewarm. The identity must survive somewhere because prewarm reconstructs requests from stored keys (`prefetch.rs:248-257`).
- **Proposed approach:** key = short hash (e.g. blake3/xxh3 of the msgpack); move the full identity into `CacheEnvelope.package_identity` (it already exists there for envelopes! `disk.rs:29-35`) and add a `package_name` secondary index table for invalidation. Memory map keys shrink ~10×; invalidation stops msgpack-decoding every key. Requires schema bump (`CURRENT_SCHEMA_VERSION` 4 → 5) and a recent-keys migration (or accept a one-time cold cache — the schema-mismatch path already recreates the DB, `disk.rs:415-461`).
- **Effort:** ~1–1.5 days. **Payoff:** memory + invalidation speed; not latency-critical.

### DF-11: Product decision — remove or wire up the batch/file_size transport surface

- **Context:** the daemon's `Batch` and `FileSize` messages and the extension's `sendBatch`/`requestFileSize` chains (`manager.ts:59`, `transport.ts:41,135`, `nativeTransport.ts:331-339,384-392`, `client.ts:137-163,270-275` + `#batchPending` machinery) have **zero production callers**; only `extension/test/**` and daemon tests use them. They were the transport contract for a WASM fallback transport planned in `2026-05-29-incomplete-feature-completion.md:805-861` that was never built (`extension/src/daemon/` has no wasmTransport).
- **Options:** (a) keep as-is (future WASM transport still plausible; zero runtime cost); (b) remove the extension-side dead chains and daemon `FileSize` handler, keep daemon `Batch` (it is the streaming test vehicle in `tests/server.rs`); (c) full removal + rewrite streaming tests around `analyze_package_json`. Recommendation: (b) if WASM transport is abandoned, else (a). **User call.**

### DF-12: Micro/opportunistic (batch with neighboring work, don't schedule alone)

- `analyze_package_json` parses the JSON twice (`package_json_dependency_sections` + `_entries`, `service.rs:584,590`) and resolves installed versions sequentially before streaming (`service.rs:590-637`).
- `minify_source_inner` runs a default `Transformer` pass over already-plain-JS bundles (`minify.rs:58-74`) — likely removable, but verify against JSX/edge inputs before touching; measure first.
- `sanitize_identifier` can collide distinct non-ASCII names (`bundle.rs:758-772`) — append a short hash only if a real package ever hits it.
- `compute_file_size` merges graphs built under different `ImportRuntime`s for mixed-runtime component docs (`file_size.rs:222-236` keeps first-seen module edges) — accepted approximation; document in code if touched.
- `find_package_root` allocates a `checked:` string per probed ancestor on the happy path (`resolver.rs:308-313`) — build details only on failure.
- Registry hints persist under the *per-workspace* cache dir (`hello.storage_path`), so npm metadata is re-fetched per workspace. Moving `registry-metadata.json` under the global lifecycle path would share it across projects — needs a one-time migration read of the old location.

---

## Execution order & rationale

Correctness first (T1–T9: crash → wrong sizes → broken features → flaky test), then dead code (T10) before the DRY passes so removals don't churn (T11–T13), micro-perf (T14), then the four independent perf tasks (T15–T18), the line index (T19), lint zeroing (T20), cleanup (T21). T11 (`crate::time`) is referenced by T17/T18 — both carry a timing note and work in either order.

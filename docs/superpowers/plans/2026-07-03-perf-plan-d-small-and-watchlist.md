# Perf Plan D — Small Wins & Watch-List (DF-8, DF-12 remainder; DF-7/DF-9 conditional)

> **STATUS: plan ready.** Last of the grouped follow-ups from `2026-07-03-daemon-review-fixes.md` (Part C). Sequence: B → A → C → **D (this)**. This is the "small/conditional" bucket: a few clean, low-risk wins as real tasks, plus a documented watch-list of items that should **not** be built yet (and why).
>
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Tasks use `- [ ]` checkboxes; the watch-list is documentation, not tasks.

**Goal:** Land the handful of low-risk micro-wins that survived verification, and record the conditional/defensive items with enough context (root cause, proposed fix, trigger condition) that they can be picked up cold if they ever become worth it — without spending effort or adding risk on theoretical problems now.

**Architecture:** All active tasks are internal to `pipeline/resolver.rs`, `service.rs`, and (optionally) the report path. No IPC/protocol change, no on-disk format change.

## Global constraints

- Each task ends with `cargo test -p import-lens-daemon` green and `cargo clippy -p import-lens-daemon --all-targets` introducing no new warnings.
- One commit per task; conventional-commit messages. Pure micro-optimizations state "no behavior change."
- Behavior must be identical — these are allocation/parallelism cleanups, not semantic changes. Existing `tests/resolver.rs` and `tests/service.rs` are the guards.

---

## Verification notes (checked against current code)

- **DF-12b confirmed.** `find_package_root` ([resolver.rs:313](../../daemon/src/pipeline/resolver.rs)) pushes `format!("checked: {}", package_json_path.display())` on **every** probed ancestor, but `checked_paths` is only read in the `Err` join ([resolver.rs:327](../../daemon/src/pipeline/resolver.rs)); on success (line 315-316) every allocation is discarded. This runs per import resolution (hot path).
- **DF-12a confirmed, right-sized.** `analyze_package_json` ([service.rs:606](../../daemon/src/service.rs)) resolves installed versions in a **sequential** `for entry in package_json_dependency_entries(...)` loop — each `resolve_installed_package_version` does a `find_package_root` ancestor walk + `package.json` read + parse — before the "loading" partial is emitted and before the parallel analysis. For a package.json with many dependencies this serial phase delays the first partial. The registry `hint_for` call in the loop is `&self` and thread-safe (Mutex-guarded cache + single-flight), so the loop is parallelizable. **Dropped from scope:** the same function parses the JSON twice (`package_json_dependency_sections` + `package_json_dependency_entries`); verified negligible (a few-KB document → µs) and deduping it would change shared public signatures, so it is not worth the churn.
- **DF-8 confirmed but marginal.** `build_workspace_report_inner` runs `handle_analyze_document` per scanned file (`par_iter`); each calls `detected_imports_for_document` → `load_import_lens_ignore` → `find_import_lens_ignore` ([ignore.rs](../../daemon/src/document/ignore.rs)), which walks ancestors calling `is_file()` for `.importlensignore`. For a 2000-file report that is thousands of extra `stat`s — but the report already scans, parses, graphs, minifies and compresses every file (seconds of work), so the ignore walk is well under 1% of report time, and reports are user-triggered, not per-keystroke. Kept as an **optional** task with honest framing; a per-directory memo preserves nested-`.importlensignore` semantics.
- **DF-7 / DF-9 / DF-12c / DF-12d — watch-list, not built.** Rationale and fix sketches below.

---

### Task D1: Build `find_package_root` failure details only on failure (DF-12b)

**Files:**
- Modify: `daemon/src/pipeline/resolver.rs` (`find_package_root`)
- Test: existing `daemon/tests/resolver.rs` covers both success and the "not found" error message — they are the guard (the error string must still list the checked paths).

- [ ] **Step 1: Confirm the guard.** Ensure `tests/resolver.rs` has a test asserting the not-found error contains the probed `checked:` paths; if not, add a small one so the lazy-formatting refactor is covered:

```rust
#[test]
fn find_package_root_error_lists_probed_paths() {
    let root = temp_workspace();
    let document = root.join("src").join("app.ts");
    std::fs::create_dir_all(document.parent().unwrap()).expect("dirs");
    std::fs::write(&document, "").expect("doc");

    let error = import_lens_daemon::pipeline::resolver::find_package_root(&document, "nope-lib")
        .expect_err("missing package should error");

    std::fs::remove_dir_all(root).expect("cleanup");
    assert!(error.contains("nope-lib"));
    assert!(error.contains("checked:"), "error should list probed paths: {error}");
}
```

- [ ] **Step 2: Refactor to collect paths, format lazily.** Push the `PathBuf` (cheap) into `checked_paths` and build the `checked: …` strings only in the `Err` branch:

```rust
    let mut checked_paths: Vec<PathBuf> = Vec::new();

    loop {
        let package_root = current.join("node_modules").join(package_name);
        let package_json_path = package_root.join("package.json");

        if package_json_path.exists() {
            return Ok(package_root);
        }
        checked_paths.push(package_json_path);

        if !current.pop() {
            break;
        }
    }

    let details = checked_paths
        .iter()
        .map(|path| format!("checked: {}", path.display()))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!("package manifest not found for {package_name}; {details}"))
```

(Push *after* the existence check so a found root does no allocation at all.)

- [ ] **Step 3: Run** `cargo test -p import-lens-daemon --test resolver` → green (same error text). Full suite + clippy clean.

- [ ] **Step 4: Commit**

```bash
git add daemon/src/pipeline/resolver.rs daemon/tests/resolver.rs
git commit -m "perf(resolver): build find_package_root failure details only on failure" -m "find_package_root allocated a formatted 'checked: <path>' string for every probed ancestor on every import resolution, discarding them all on the common success path. Collect PathBufs and format the diagnostic only in the not-found branch. No behavior change; the error message is unchanged."
```

---

### Task D2: Resolve package.json dependency versions in parallel (DF-12a)

**Files:**
- Modify: `daemon/src/service.rs` (`analyze_package_json` — the pre-stream version-resolution loop)
- Test: `daemon/tests/service.rs` (existing package.json analysis tests are the guard; add one asserting order/version correctness if coverage is thin)

- [ ] **Step 1: Confirm/strengthen the guard.** Ensure a `tests/service.rs` package.json test asserts the resolved `installed_version` and section for each dependency in order (the parallel version must produce identical `states`/`import_requests` ordering). Strengthen if needed.

- [ ] **Step 2: Parallelize the version-resolution phase.** Replace the sequential `for entry in package_json_dependency_entries(...)` loop with a `par_iter().map(...).collect()` that produces an ordered `Vec<(PackageJsonDependencyAnalysisItem, Option<ImportRequest>)>`, then split into `states` / `import_requests` in order. Each closure does exactly what the loop body does today — `resolve_installed_package_version`, the `hint_for` call (thread-safe), and building the `Loading`/`Missing` state — so output is identical, only concurrent. Use `rayon::prelude::*` (already imported in `service.rs`). Example shape:

```rust
        let entries = package_json_dependency_entries(&request.source);
        let resolved: Vec<(PackageJsonDependencyAnalysisItem, Option<ImportRequest>)> = entries
            .into_par_iter()
            .map(|entry| {
                match resolve_installed_package_version(&context.active_document_path, &entry.name) {
                    Ok(version) => {
                        let import_request = ImportRequest {
                            specifier: entry.name.clone(),
                            package_name: entry.name.clone(),
                            version: version.clone(),
                            named: Vec::new(),
                            import_kind: ImportKind::Namespace,
                            runtime: ImportRuntime::Component,
                        };
                        let registry_hint = self
                            .registry_hints
                            .hint_for(&entry.name, Some(&version), registry_hint_mode, now_ms)
                            .hint;
                        let state = PackageJsonDependencyAnalysisItem {
                            name: entry.name.clone(),
                            section: entry.section.clone(),
                            entry,
                            status: ImportAnalysisStatus::Loading,
                            installed_version: Some(version),
                            registry_hint,
                            message: None,
                            result: None,
                        };
                        (state, Some(import_request))
                    }
                    Err(message) => {
                        let state = PackageJsonDependencyAnalysisItem {
                            name: entry.name.clone(),
                            section: entry.section.clone(),
                            entry,
                            status: ImportAnalysisStatus::Missing,
                            installed_version: None,
                            registry_hint: None,
                            message: Some(message),
                            result: None,
                        };
                        (state, None)
                    }
                }
            })
            .collect();
        let (states, import_requests): (Vec<_>, Vec<_>) = resolved.into_iter().unzip();
```

(`into_par_iter().collect()` preserves order, so `states`/`import_requests` line up with the streaming `indexes` exactly as before. The rest of the function — the loading-partial emit and the downstream `par_iter` analysis — is unchanged.)

- [ ] **Step 3: Run** `cargo test -p import-lens-daemon --test service` and the streaming server test → green (identical states, same partial `indexes`). Full suite + clippy clean.

- [ ] **Step 4: Commit**

```bash
git add daemon/src/service.rs daemon/tests/service.rs
git commit -m "perf(service): resolve package.json dependency versions in parallel" -m "analyze_package_json resolved every dependency's installed version sequentially (each an ancestor walk plus a package.json read) before emitting the loading partial, serializing the pre-stream phase for large manifests. Resolve them with rayon; ordering, states and streaming indexes are unchanged."
```

---

### Task D3 (OPTIONAL — marginal): memoize `.importlensignore` during workspace reports (DF-8)

> Reports are user-triggered and the ignore walk is <1% of report time — do this only if profiling a large-monorepo report shows it matters. If skipped, DF-8 stays in the backlog.

**Files:** `daemon/src/service.rs` (`build_workspace_report_inner` / a report-scoped analyze variant), preserving per-directory (nearest-ancestor) `.importlensignore` semantics.

- [ ] Build a per-report `HashMap<PathBuf /* dir */, Arc<Vec<ImportLensIgnoreRule>>>` computed lazily (nearest-ancestor `.importlensignore` per directory), threaded into the report's per-file analysis instead of `handle_analyze_document`'s unconditional `load_import_lens_ignore`. Guard with a report test that a nested `.importlensignore` still filters only files under its directory. Commit as `perf(report): memoize .importlensignore lookups across scanned files`.

---

## Watch-list — documented, deliberately not built

These are real but should not be implemented now; each records the trigger that would justify it.

### DF-7 — recursive `load_module_from` → explicit work stack (defensive)
- **What:** `load_module_from` ([graph.rs](../../daemon/src/pipeline/graph.rs)) recurses per dependency edge; depth is bounded by the longest acyclic import chain (cycles cut by `path_to_id`), capped at `MAX_GRAPH_MODULES = 2000`. A ~2000-deep linear chain × ~0.5–1 KB frames could approach a worker-thread stack limit.
- **Why not now:** no real package reproduces it; rewriting a correct, hot recursion risks a regression for a theoretical gain.
- **Trigger / fix:** if a real package ever overflows, convert to an explicit `Vec<(PathBuf, Option<PathBuf>)>` stack, preserving `loading_paths`/`circular_edges` cycle diagnostics; add a generated ~3000-deep-chain fixture asserting it errors on the module cap rather than overflowing. ~half a day.

### DF-9 — `cache_remove(current_project)` vs. in-flight analyses on Windows
- **What:** `remove_shard_by_id` ([project.rs](../../daemon/src/cache/project.rs)) drops the registry's `Arc<ImportCache>` before `remove_dir_all`, so redb closes first — unless a concurrent analysis/prewarm still holds a clone, in which case Windows keeps `importlens.redb` open (delete-pending) and removal fails; a retry succeeds, and the UI already reports the failure.
- **Why not now:** transient, self-correcting, already surfaced to the user; the fix adds real complexity (in-flight tracking or deletion-on-next-hello) for a rare race.
- **Trigger / fix:** if users report frequent removal failures, track in-flight users per shard (bounded wait on a `Weak` count before `remove_dir_all`) or mark the shard directory for deletion-on-next-hello (a `.pending-delete` marker cleaned in `ProjectCacheRegistry::new`). Windows-CI integration test holding a second `Arc<ImportCache>` across the removal.

### DF-12c — `compute_file_size` mixed-runtime graph merge (accepted approximation)
- **What:** `compute_file_size` ([file_size.rs](../../daemon/src/pipeline/file_size.rs)) merges graphs built under different `ImportRuntime`s for mixed-runtime component docs, keeping first-seen module edges.
- **Why not now:** it is an accepted approximation, only reachable for mixed-runtime documents, and "fixing" it would mean per-runtime sub-bundling with unclear value.
- **Action if touched:** add a code comment stating the first-graph-wins approximation so the next reader does not mistake it for a bug. No behavior change.

### DF-12d — share registry metadata across workspaces (needs cross-process design)
- **What:** `RegistryMetadataCache` persists under `hello.storage_path` (the per-workspace cache dir), so npm metadata is re-fetched per workspace. Moving `registry-metadata.json` to the global lifecycle path (`--storage`) would share it across every workspace's daemon.
- **Why not now — this is not a trivial path change:** each workspace runs its own daemon process, so a shared file means **multiple processes** writing one `registry-metadata.json`. The current atomic tmp+rename is single-writer-wins, so concurrent daemons would clobber each other's entries (worse under Plan C's DF-5 debounce, which widens the write window). It needs cross-process coordination (file lock, or per-process shard files merged on read) plus a one-time migration read of the old per-workspace location.
- **Trigger / fix:** if registry re-fetch across projects is shown to be costly, design it as its own plan: per-process shard files under the global path (`registry-metadata.<pid-or-hash>.json`) merged on load, avoiding cross-process write contention; migrate the old file on first run.

### Also parked (from earlier plans / decisions)
- **DF-10** — compact cache keys (short-hash + secondary index + schema bump). Pulled from Plan C; do only if memory/invalidation is shown to be a real problem, split into 10a (in-memory invalidation index, no schema change) and 10b (the risky key-format + migration). See Plan C's closing section.
- **DF-11** — batch/`file_size` transport surface **kept as-is** per decision (WASM fallback transport is a planned future path).

## Exit

D1 → D2 (D3 optional). With these, the actionable backlog is closed except the deliberately-parked DF-7, DF-9, DF-10, DF-12c/d and the kept DF-11 — each documented above with its trigger condition.

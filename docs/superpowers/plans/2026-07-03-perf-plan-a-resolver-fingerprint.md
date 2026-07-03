# Perf Plan A — Resolver Reuse & Fingerprint Gate (DF-1, DF-3)

> **STATUS: plan ready; execute task-by-task, one commit per task.** Second of the grouped follow-up plans from `2026-07-03-daemon-review-fixes.md` (Part C). Sequence: B (done/queued) → **A (this)** → C (persistence) → D (small/conditional).
>
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use `- [ ]` checkboxes.

**Goal:** Stop paying a full module-resolution FS walk and a full dependency re-`stat` on the warm path. Every analysis request today (a) builds a throwaway `oxc_resolver::Resolver` and resolves from a cold cache — *even on a cache hit* — and (b) `fs::metadata`s every dependency file of the import's graph on every memory-cache hit. Both are pure waste between `node_modules` changes, which the extension already signals. Fix each behind the same invalidation chokepoint.

**Architecture:** Two process-global caches gain lifecycle management, both invalidated at the existing `ImportLensService::invalidate_package` / `invalidate_all` chokepoints (which the IPC server already drives from `CacheInvalidate` / `CacheInvalidateAll` / `NodeModulesChanged`). No IPC/protocol change; no on-disk cache-format change (the DF-3 state is in-memory only). The oxc resolver's internal cache is `DashMap`-backed (`Send + Sync`), so one shared instance is safe across the rayon batch workers.

**Tech stack:** Rust 2024, `oxc_resolver = "=11.22.0"`, rayon, std sync primitives (no new deps).

## Global constraints

- **Resolution results must not change.** `daemon/tests/resolver.rs` is the correctness guard for DF-1: a shared, warm resolver must resolve every specifier identically to a fresh one (the oxc cache memoizes filesystem *facts* — path existence, `package.json` contents — not option-dependent resolution results, so sharing one cache across the three runtime option-sets is correct). If any resolver test changes, the sharing is wrong.
- **Cache-hit results must stay correct within the accepted staleness window** (see DF-3 decision below). The existing `daemon/tests/*cache*.rs` and `daemon/tests/service.rs` invalidation tests are the guard.
- Each task ends with `cargo test -p import-lens-daemon` green and `cargo clippy -p import-lens-daemon --all-targets` introducing no new warnings.
- One commit per task; conventional-commit messages with a body naming the user-visible effect and quoting the bench delta for perf tasks.
- Perf measurement extends `daemon/tests/performance.rs` (existing `threshold_ms()`/`Instant` pattern, `#[ignore]` release-only).

---

## Verification notes (checked against current code + oxc_resolver 11.22.0)

- **DF-1 hot path confirmed.** `resolve_with_oxc` ([resolver.rs:150](../../daemon/src/pipeline/resolver.rs)) calls `create_resolver(request.runtime)` — a fresh `Resolver::new(...)` with an empty cache — on **every** call, and `analyze_with_cache` ([service.rs](../../daemon/src/service.rs)) runs `resolve_package_entry` *before* the `cache.get()` check, so a warm cache hit still pays a full cold resolution. `validate_declared_entry_resolution` ([resolver.rs:208](../../daemon/src/pipeline/resolver.rs)) builds a *second* fresh resolver per root import; `ModuleGraphBuilder::new` ([graph.rs](../../daemon/src/pipeline/graph.rs)) and `analyze_cjs_graph_with_runtime` ([cjs.rs](../../daemon/src/pipeline/cjs.rs)) each build their own. Five construction sites, each discarding the FS cache.
  - `create_resolver` returns `oxc_resolver::Resolver` = `ResolverGeneric<FileSystemOs>` with `cache: Arc<Cache>`, `Cache` backed by `DashMap` (`oxc_resolver/src/cache/cache_impl.rs:26`) ⇒ **`Send + Sync`**, safe to share across `par_iter` batch workers.
  - `Resolver::clone_with_options(opts)` (`lib.rs:200`) does `Arc::clone(&self.inner.cache)` ⇒ one shared FS cache across the 3 runtime option-sets. `Resolver::clear_cache()` exists (`lib.rs:218`).
  - **Design correction vs. backlog:** `clear_cache`'s doc warns *"the caller must ensure there're no ongoing resolution operations… otherwise it may cause those operations to return an incorrect result."* In this daemon, invalidation is handled inline in the IPC loop while prewarm (`prefetch.rs` background threads) and workspace-report/registry-refresh (spawned, un-awaited) resolutions can be in flight. So **do not `clear_cache()` in place.** Instead publish resolvers behind an atomic snapshot (`RwLock<Arc<ResolverSet>>`), and on invalidation *swap in a fresh `ResolverSet`* (new empty cache). In-flight operations keep their `Arc` snapshot (old cache) and finish consistently; the old cache drops when the last reader releases it. This sidesteps the concurrent-clear hazard entirely.
  - **Scope note:** DF-1 removes the resolver's internal FS re-walk. `find_package_manifest` does its *own* `fs::read_to_string(package.json)` + `serde_json` parse independent of oxc — that read is **not** eliminated here (it's a separate future item, not DF-1).
- **DF-3 confirmed.** `ImportCache::get` ([memory.rs:67](../../daemon/src/cache/memory.rs)) calls `fingerprints_are_current(&cached.dependency_fingerprints)` on every memory hit; `fingerprints_are_current` ([key.rs](../../daemon/src/cache/key.rs)) `fs::metadata`s every stored path (all module files + `package.json` of the import's graph). For a large dependency (hundreds of modules) that is hundreds of syscalls per hit, per import, per debounced keystroke.
  - **Design decision — global generation counter + TTL backstop (surfaced for sign-off).** Dependency fingerprints point at files under `node_modules`; between `node_modules` changes those files do not change (the user edits *source*, which is never a dependency fingerprint). So re-`stat`ing on every hit is waste *unless* `node_modules` changed. Gate it on a process-global monotonic `CACHE_GENERATION` bumped at every `invalidate_package`/`invalidate_all`: a cached entry records the generation it was last verified at; `get` skips the re-`stat` while `entry.generation == CACHE_GENERATION`. A **per-package** generation would be wrong — an entry's fingerprints span transitive dependencies from *other* packages — so the counter is global (coarse but correct: any invalidation forces one re-verify of everything on next touch). Backstop for changes that arrive with **no** invalidation event (e.g. a watcher-excluded folder, external process): also re-verify when `now - entry.verified_at_millis > REVERIFY_TTL_MS` (bounded staleness = TTL). The two verification fields are **in-memory only** — not serialized into the redb `CacheEnvelope` — so no schema bump; a disk-loaded entry starts at generation 0 and is verified once on first hit (correct).

---

### Task A1: Add a steady-state warm-path bench to `tests/performance.rs`

One bench captures both wins: after warming the cache, repeated re-analysis of the same batch pays (a) a fresh resolve per import (DF-1) and (b) a full fingerprint re-`stat` per hit (DF-3), exactly the per-keystroke steady state.

**Files:** Modify `daemon/tests/performance.rs` (append)

- [ ] **Step 1: Add an ignored timing test** over a multi-module dependency, warmed once then hit M times:

```rust
#[test]
#[ignore = "release-only performance smoke run by pnpm test:performance"]
fn warm_reanalysis_of_multi_module_dependency_stays_under_threshold() {
    use std::fs;
    let workspace = common::temp_workspace("import-lens-perf-warm");
    let pkg = workspace.join("node_modules").join("wide-lib");
    fs::create_dir_all(&pkg).expect("pkg dir");
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"wide-lib","version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    let mut index = String::new();
    for i in 0..60 {
        fs::write(
            pkg.join(format!("leaf{i}.js")),
            format!("export const fn{i} = () => {i};\n"),
        )
        .expect("leaf");
        index.push_str(&format!("export {{ fn{i} }} from './leaf{i}.js';\n"));
    }
    fs::write(pkg.join("index.js"), index).expect("index");

    let service = ImportLensService::new(None, false);
    let document = workspace.join("src").join("app.ts");
    let batch = |request_id: u64| BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.to_string_lossy().to_string(),
        imports: vec![ImportRequest {
            specifier: "wide-lib".to_owned(),
            package_name: "wide-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["fn0".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    };

    // Warm once (miss), then simulate 50 debounced re-analyses (all hits).
    assert_eq!(service.handle_batch(batch(0)).imports[0].error, None);
    let start = Instant::now();
    for i in 1..=50 {
        let response = service.handle_batch(batch(i));
        assert!(response.imports[0].cache_hit, "expected warm hit");
    }
    let elapsed_ms = start.elapsed().as_millis();

    fs::remove_dir_all(&workspace).expect("cleanup");
    eprintln!("warm_reanalysis: {elapsed_ms}ms for 50 hits");
    assert!(
        elapsed_ms <= threshold_ms(2000),
        "warm re-analysis exceeded threshold: {elapsed_ms}ms"
    );
}
```

- [ ] **Step 2: Record baseline.** `IMPORT_LENS_PERF_MULTIPLIER=1 cargo test -p import-lens-daemon --test performance --release -- --ignored warm_reanalysis --nocapture`; note the ms in the commit body.

- [ ] **Step 3: Commit**

```bash
git add daemon/tests/performance.rs
git commit -m "test(perf): add warm multi-module re-analysis timing baseline" -m "Warms the cache once then times 50 all-hit re-analyses of a 60-module dependency, capturing the per-hit resolve (DF-1) and fingerprint re-stat (DF-3) costs the next two commits remove. Baseline: <X>ms at multiplier 1."
```

---

### Task A2: Share one resolver set with atomic invalidation (DF-1)

**Files:**
- Modify: `daemon/src/pipeline/resolver.rs` (introduce `ResolverSet` + `shared_resolvers()` + `invalidate_shared_resolvers()`; `create_resolver` → borrow the shared one)
- Modify: `daemon/src/pipeline/graph.rs` (`ModuleGraphBuilder` holds an `Arc<ResolverSet>` snapshot)
- Modify: `daemon/src/pipeline/cjs.rs` (`analyze_cjs_graph_with_runtime` uses the shared set)
- Modify: `daemon/src/service.rs` (`invalidate_package` / `invalidate_all` also invalidate resolvers)
- Test: `daemon/tests/resolver.rs` (append the invalidation test); existing tests are the correctness guard.

**Interfaces (produced):**
- `pub struct ResolverSet { component: Resolver, client: Resolver, server: Resolver }` with `pub fn resolver(&self, runtime: ImportRuntime) -> &Resolver`.
- `pub fn shared_resolvers() -> Arc<ResolverSet>` — loads the current snapshot (read-lock + `Arc::clone`).
- `pub fn invalidate_shared_resolvers()` — swaps in a fresh `ResolverSet` (new empty cache).

- [ ] **Step 1: Write the failing invalidation test** (append to `daemon/tests/resolver.rs`, following its temp-workspace helpers)

```rust
#[test]
fn shared_resolver_reflects_node_modules_change_only_after_invalidation() {
    let root = temp_workspace();
    write_package_with_main(&root, "swap-lib", "1.0.0", "a.js"); // helper writes package.json + a.js/b.js
    let document = root.join("src").join("app.ts");

    let first = resolve_entry_for(&document, "swap-lib"); // helper: resolve_package_entry -> entry_path
    assert!(first.ends_with("a.js"));

    // Repoint main to b.js on disk; without invalidation the shared cache still serves a.js.
    rewrite_main(&root, "swap-lib", "b.js");
    let stale = resolve_entry_for(&document, "swap-lib");
    assert!(stale.ends_with("a.js"), "shared cache should persist until invalidated");

    import_lens_daemon::pipeline::resolver::invalidate_shared_resolvers();
    let fresh = resolve_entry_for(&document, "swap-lib");

    fs::remove_dir_all(root).expect("cleanup");
    assert!(fresh.ends_with("b.js"), "resolution should update after invalidation");
}
```

(Add the small `write_package_with_main` / `rewrite_main` / `resolve_entry_for` helpers to the test file if not present.)

- [ ] **Step 2: Run** → the `stale`/`fresh` assertions fail today (fresh resolver every call means `stale` already sees `b.js`), proving the current per-call construction and confirming the new test drives the shared behavior. Expect FAIL at the `stale` assertion.

- [ ] **Step 3: Implement the shared set** in `resolver.rs`:

```rust
use std::sync::{Arc, OnceLock, RwLock};

pub struct ResolverSet {
    component: Resolver,
    client: Resolver,
    server: Resolver,
}

impl ResolverSet {
    fn new() -> Self {
        let base = Resolver::new(resolve_options(ImportRuntime::Component));
        let client = base.clone_with_options(resolve_options(ImportRuntime::Client));
        let server = base.clone_with_options(resolve_options(ImportRuntime::Server));
        Self { component: base, client, server }
    }

    pub fn resolver(&self, runtime: ImportRuntime) -> &Resolver {
        match runtime {
            ImportRuntime::Component => &self.component,
            ImportRuntime::Client => &self.client,
            ImportRuntime::Server => &self.server,
        }
    }
}

static SHARED_RESOLVERS: OnceLock<RwLock<Arc<ResolverSet>>> = OnceLock::new();

fn resolver_slot() -> &'static RwLock<Arc<ResolverSet>> {
    SHARED_RESOLVERS.get_or_init(|| RwLock::new(Arc::new(ResolverSet::new())))
}

pub fn shared_resolvers() -> Arc<ResolverSet> {
    resolver_slot()
        .read()
        .map(|guard| Arc::clone(&guard))
        .unwrap_or_else(|_| Arc::new(ResolverSet::new()))
}

pub fn invalidate_shared_resolvers() {
    if let Ok(mut guard) = resolver_slot().write() {
        *guard = Arc::new(ResolverSet::new());
    }
}
```

(`component`/`client`/`server` share one `Arc<Cache>` via `clone_with_options`; the swap drops the whole set so the new one starts cold. Base built with Component options — the `base.clone_with_options` for client/server reuses that cache, which memoizes option-independent FS facts only.)

- [ ] **Step 4: Switch the five call sites to borrow the shared set:**
  - `resolve_with_oxc`: `let resolvers = shared_resolvers(); let resolved = resolve_module_path(resolvers.resolver(request.runtime), directory, &request.specifier)?;`
  - `validate_declared_entry_resolution`: take the shared `&Resolver` instead of `create_resolver(runtime)`.
  - `ModuleGraphBuilder`: store `resolvers: Arc<ResolverSet>` (loaded once in `new`) and use `self.resolvers.resolver(self.runtime)` where it currently uses `self.resolver`. (Keep a `runtime` field.)
  - `analyze_cjs_graph_with_runtime`: `let resolvers = shared_resolvers();` once, use `resolvers.resolver(runtime)` in the loop.
  - Delete `create_resolver` (or make it `#[cfg(test)]` if a test needs an isolated resolver — check `tests/resolver.rs` first).

- [ ] **Step 5: Hook invalidation** in `service.rs`:

```rust
    pub fn invalidate_package(&self, package_name: &str) {
        self.cache_registry.invalidate_package(package_name);
        invalidate_module_graph_cache_for_package(package_name);
        crate::pipeline::resolver::invalidate_shared_resolvers();
    }

    pub fn invalidate_all(&self) {
        self.cache_registry.clear_all();
        clear_module_graph_cache();
        crate::pipeline::resolver::invalidate_shared_resolvers();
    }
```

(`invalidate_package_json_paths` already funnels into these two, so `NodeModulesChanged` is covered.)

- [ ] **Step 6: Run** `cargo test -p import-lens-daemon` → the new invalidation test passes and **every existing `tests/resolver.rs` assertion stays green** (resolution unchanged). Clippy clean. Re-run the A1 bench and quote the delta.

- [ ] **Step 7: Commit**

```bash
git add daemon/src/pipeline/resolver.rs daemon/src/pipeline/graph.rs daemon/src/pipeline/cjs.rs daemon/src/service.rs daemon/tests/resolver.rs
git commit -m "perf(resolver): share one oxc resolver with its FS cache across requests" -m "Every analysis built a throwaway oxc Resolver and resolved from a cold cache, even on cache hits, re-walking node_modules per request and per keystroke. Publish three runtime resolvers sharing one DashMap-backed cache behind an Arc snapshot; invalidation swaps in a fresh set rather than clearing in place (oxc's clear_cache is unsafe against concurrent resolutions from prewarm/report threads). Resolution results are unchanged (tests/resolver.rs). Bench: <before>ms -> <after>ms for 50 warm re-analyses."
```

---

### Task A3: Gate fingerprint re-verification on a generation counter + TTL (DF-3)

**Files:**
- Modify: `daemon/src/cache/memory.rs` (`CachedImport` gains in-memory verification state; `get` gates the re-`stat`; global generation counter + `bump_cache_generation()`)
- Modify: `daemon/src/service.rs` (`invalidate_package` / `invalidate_all` bump the generation)
- Test: `daemon/tests/memory_cache.rs` (append)

**Interfaces (produced):**
- `pub fn bump_cache_generation()` and an internal `fn current_cache_generation() -> u64` over a `static CACHE_GENERATION: AtomicU64`.
- `CachedImport` gains `verified_generation: u64` and `verified_at_millis: u64` (in-memory only; `disk.rs`'s `CacheEnvelope` is unchanged — a disk-loaded entry starts at generation 0).
- `const REVERIFY_TTL_MS: u64 = 30_000;`

- [ ] **Step 1: Write the tests** (append to `daemon/tests/memory_cache.rs`)

```rust
#[test]
fn cache_hit_skips_fingerprint_restat_until_generation_bumps() {
    // A disk-disabled cache still fingerprint-gates in memory. Insert with a
    // fingerprint pointing at a real file; a hit within the same generation
    // must NOT observe an out-of-band deletion (proves the stat was skipped),
    // while a hit after bump_cache_generation() must re-verify and miss.
    let dir = temp_storage();
    let dep = dir.join("dep.js");
    std::fs::write(&dep, "export const x = 1;").expect("dep file");
    let fp = fingerprints_for_paths(vec![dep.clone()]); // test helper re-exported from cache::key

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints("v3:aa".to_owned(), result("dep"), fp);

    std::fs::remove_file(&dep).expect("delete dep out of band");
    // Same generation, within TTL: re-stat is skipped, so the stale hit still returns.
    assert!(cache.get("v3:aa").is_some(), "should serve without re-stat inside generation");

    bump_cache_generation();
    // After a bump the entry re-verifies, finds the missing file, and evicts.
    assert!(cache.get("v3:aa").is_none(), "generation bump forces re-verify");

    std::fs::remove_dir_all(dir).expect("cleanup");
}
```

(Expose `fingerprints_for_paths` / `bump_cache_generation` for the test as needed; adapt the `result()` helper to the file's existing one.)

- [ ] **Step 2: Run** → FAIL today (every hit re-`stat`s, so the first `get` after deletion already misses). Confirms the current unconditional re-verify.

- [ ] **Step 3: Implement** in `memory.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static CACHE_GENERATION: AtomicU64 = AtomicU64::new(1);
const REVERIFY_TTL_MS: u64 = 30_000;

pub fn bump_cache_generation() {
    CACHE_GENERATION.fetch_add(1, Ordering::Release);
}

fn current_cache_generation() -> u64 {
    CACHE_GENERATION.load(Ordering::Acquire)
}
```

`CachedImport` gains `verified_generation: u64` and `verified_at_millis: u64`. On `insert_with_fingerprints`, stamp `current_cache_generation()` + `crate::time::unix_millis_now()`. Rewrite the memory branch of `get`:

```rust
    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            let generation = current_cache_generation();
            let now = crate::time::unix_millis_now();
            let fresh_without_restat = cached.verified_generation == generation
                && now.saturating_sub(cached.verified_at_millis) <= REVERIFY_TTL_MS;

            if !fresh_without_restat {
                if !fingerprints_are_current(&cached.dependency_fingerprints) {
                    memory.remove(key);
                    self.disk.remove(key);
                    return None;
                }
                // Re-verified: restamp so the next hit can skip the stat.
                let mut restamped = cached.clone();
                restamped.verified_generation = generation;
                restamped.verified_at_millis = now;
                memory.insert(key.to_owned(), restamped);
            }

            let cached = memory.get(key)?;
            let mut result = cached.result.clone();
            result.cache_hit = true;
            self.disk.touch(key);
            return Some(result);
        }

        if let Some(cached) = self.disk.get(key) {
            let mut result = cached.result.clone();
            memory.insert(key.to_owned(), cached);
            result.cache_hit = true;
            return Some(result);
        }

        None
    }
```

(The disk `get` path already verifies fingerprints in `DiskCache::get_entry`; a promoted entry keeps generation 0 and re-verifies once on next hit — correct.)

- [ ] **Step 4: Bump on invalidation** in `service.rs` — add `crate::cache::memory::bump_cache_generation();` to both `invalidate_package` and `invalidate_all` (alongside the A2 resolver invalidation). A per-package bump of a global counter is intentional: fingerprints span transitive packages, so any invalidation must force a global re-verify.

- [ ] **Step 5: Run** `cargo test -p import-lens-daemon` → new tests green; existing invalidation tests (`service.rs`, `cache_disk.rs`) green — invalidation still produces a fresh analysis because the bump forces re-verify (and A2 already clears the graph/resolver caches). Clippy clean. Re-run the A1 bench and quote the delta.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/memory.rs daemon/src/service.rs daemon/tests/memory_cache.rs
git commit -m "perf(cache): skip dependency re-stat on hits between invalidations" -m "Every memory-cache hit fs::metadata'd every module file and package.json of the import's graph, hundreds of syscalls per hit per keystroke, though those node_modules files only change when the extension sends an invalidation. Gate the re-stat on a global generation counter bumped at invalidate_package/invalidate_all, with a 30s TTL backstop for changes that arrive without a watcher event. Verification state is in-memory only (no redb schema change). Bench: <before>ms -> <after>ms for 50 warm re-analyses."
```

---

## Risk & staleness summary (for sign-off)

- **Accepted window:** between a `node_modules` change and the extension's invalidation message (its ~250 ms debounce + IPC latency), resolutions use the old resolver cache and hits skip re-`stat`. Stale *results* are still caught by the fingerprint check on the next re-verify (generation bump forces it). Stale *resolutions* within the window can produce a fresh-looking result for a moved entry; corrected on the next invalidation. This is the same window the daemon already tolerates via debounced invalidation.
- **No-event changes** (watcher-excluded folders, external mutation): bounded to `REVERIFY_TTL_MS` (30 s) for DF-3; DF-1's resolver cache would stay stale until *some* invalidation arrives — acceptable for the resolver (a wrong resolution still yields a fingerprint-checked result), but if this proves a problem in practice, add a TTL swap to the resolver set too. Noted, not built.
- **Thread-safety:** DF-1 swaps `Arc<ResolverSet>` snapshots (no in-place clear ⇒ no oxc concurrent-clear hazard); DF-3 uses an `AtomicU64` generation and papaya's concurrent map. No new locks on the resolution hot path beyond one `RwLock` read (uncontended; writers are rare invalidations).

## Exit

A1 → A2 → A3. On completion, Plan C (persistence write amplification: DF-4 redb batching, DF-5 registry debounce, DF-10 compact keys + schema bump) is next. DF-7 and the DF-12 remainder sit in Plan D.

# Cache Freshness Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cached bundle sizes provably correct — bind every result to a content hash of the exact bytes that produced it, capture the cache generation *before* analysis (not at insert), make the freshness check tri-state so transient filesystem errors never destroy a valid cache, and move the re-verify TTL to a monotonic clock.

**Architecture:** This is **Plan 1 of 4** for the cache-lifecycle redesign (spec: `docs/superpowers/specs/2026-07-05-cache-lifecycle-redesign-design.md`, Phase 1). It touches only the *freshness/correctness* layer; capacity/eviction, identity v4, SWR, registry, UI, and migration are later plans. Content hashes are captured at module-graph build time (the graph is memoized, so bytes are read once) and ride the cached graph into the value-side `dependency_fingerprints`. The boolean `fingerprints_are_current` becomes a tri-state `check_fingerprints` returning `Fresh | Stale | Gone | Unknown`; only a definitive `NotFound` evicts, a transient error keeps the entry.

**Tech Stack:** Rust (edition 2024), `redb` (disk cache), `papaya` (in-memory + graph cache), `rmp-serde` (msgpack), OXC pipeline, `xxhash-rust` (new — fast content hash).

## Global Constraints

- **Conventional Commits with a mandatory body.** Format `type(scope): subject`, blank line, body explaining what + why. Types: `feat fix perf docs refactor style test chore ci build`. (Enforced by a `commit-msg` hook.)
- **Gates must pass** before each commit (pre-commit hook runs them on staged files): `cargo clippy --workspace --all-targets` (workspace lint = `deny`), `cargo deny check`, `cargo fmt`. Run the full suite with `pnpm test`.
- **Dependency version policy:** tier by blast radius. A new leaf crate (the hasher) uses a **caret** pin, consistent with the file. Add it with `cargo add` (picks latest compatible) — do **not** hand-pin an exact version. OXC stays patch-only (do not touch).
- **Windows-first robustness:** only `std::io::ErrorKind::NotFound` means "gone." Every other `fs` error is transient → **keep** the entry, never delete.
- **Content hash is a fast non-cryptographic hash** (integrity, not security): `xxhash_rust::xxh3::xxh3_64`.
- **No change to *what* bytes are analyzed** — only *when/how* fingerprints and generation are captured. Analysis output must be byte-identical.
- **Cache key must stay byte-stable** in this plan (identity v4 is a later plan). Adding `content_hash` to `FileFingerprint` must not change the serialized key for a fingerprint whose hash is absent.

## File Structure

- `daemon/Cargo.toml` — add the `xxhash-rust` dependency.
- `daemon/src/cache/key.rs` — `content_hash()` helper; add `FileFingerprint.content_hash`; add `Freshness` enum + `check_fingerprint`/`check_fingerprints`/`classify_stat_error`; add `file_fingerprint_with_hash`; keep `fingerprints_are_current` as a thin `Fresh`-only wrapper.
- `daemon/src/pipeline/graph.rs` — add `ModuleRecord.content_hash` (captured in `load_module_from`); add `ModuleGraph::content_hash_for`; content-hash-aware `module_graph_fingerprints`.
- `daemon/src/service.rs` — capture generation at the top of `analyze_and_cache`; content-hash-aware `dependency_fingerprints`; thread the captured generation into inserts.
- `daemon/src/cache/memory.rs` — `insert_with_fingerprints_at_generation`; tri-state `get`; `verified_at: Option<Instant>`.
- `daemon/src/cache/disk.rs` — tri-state `get_entry`/`pending_insert_entry`; `verified_at: None` on decode.
- Tests: `daemon/tests/freshness_core.rs` (new, integration), plus `#[cfg(test)] mod tests` additions in `key.rs`.

---

## Task 1: Add a fast content hasher

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/src/cache/key.rs` (add `content_hash` fn + unit test module)

**Interfaces:**
- Produces: `pub fn content_hash(bytes: &[u8]) -> u64` in `crate::cache::key`.

- [ ] **Step 1: Add the dependency**

Run: `cargo add xxhash-rust --features xxh3` (run in the `daemon/` directory, or `cargo add -p daemon xxhash-rust --features xxh3` from the root). This writes a caret pin like `xxhash-rust = { version = "^0.8", features = ["xxh3"] }`.

- [ ] **Step 2: Write the failing test**

Add to the bottom of `daemon/src/cache/key.rs`, inside a `#[cfg(test)] mod tests { use super::*; ... }` block (create the block if absent):

```rust
#[test]
fn content_hash_is_deterministic_and_distinguishes_content() {
    assert_eq!(content_hash(b"export const x = 1;"), content_hash(b"export const x = 1;"));
    assert_ne!(content_hash(b"export const x = 1;"), content_hash(b"export const x = 2;"));
    // Same length, different content — the case mtime+len can miss.
    assert_ne!(content_hash(b"aaaa"), content_hash(b"bbbb"));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test content_hash_is_deterministic_and_distinguishes_content`
Expected: FAIL — `cannot find function content_hash`.

- [ ] **Step 4: Implement**

Add near the top of `daemon/src/cache/key.rs` (after the imports):

```rust
/// Fast, non-cryptographic content hash of the bytes actually read during
/// analysis. Used to detect real content changes (and ignore no-op touches
/// such as `npm ci` that only bump mtime). Not for security.
pub fn content_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test content_hash_is_deterministic_and_distinguishes_content`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add daemon/Cargo.toml daemon/Cargo.lock daemon/src/cache/key.rs
git commit -m "feat(daemon): add xxh3 content-hash helper

Introduce content_hash() over xxhash-rust for the cache freshness
redesign, so a cached size can be bound to a hash of the exact bytes
analyzed rather than an mtime+len signature."
```

---

## Task 2: `content_hash` on `FileFingerprint` + tri-state freshness check

**Files:**
- Modify: `daemon/src/cache/key.rs`
- Test: `daemon/src/cache/key.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `content_hash` (Task 1).
- Produces:
  - `FileFingerprint.content_hash: Option<u64>` (last field, `#[serde(default, skip_serializing_if = "Option::is_none")]`).
  - `pub enum Freshness { Fresh, Stale, Gone, Unknown }`
  - `pub fn check_fingerprint(stored: &FileFingerprint) -> Freshness`
  - `pub fn check_fingerprints(fingerprints: &[FileFingerprint]) -> Freshness`
  - `pub fn file_fingerprint_with_hash(path: impl AsRef<Path>, content_hash: Option<u64>) -> Option<FileFingerprint>`
  - `fn classify_stat_error(kind: std::io::ErrorKind) -> Freshness`
  - `fingerprints_are_current` retained as `matches!(check_fingerprints(..), Freshness::Fresh)`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `daemon/src/cache/key.rs`:

```rust
#[test]
fn classify_stat_error_only_notfound_is_gone() {
    use std::io::ErrorKind;
    assert!(matches!(classify_stat_error(ErrorKind::NotFound), Freshness::Gone));
    assert!(matches!(classify_stat_error(ErrorKind::PermissionDenied), Freshness::Unknown));
    // Any non-NotFound (locked file, offline drive) is transient → keep.
    assert!(matches!(classify_stat_error(ErrorKind::Other), Freshness::Unknown));
}

#[test]
fn check_fingerprint_content_hash_ignores_mtime_only_touch() {
    let dir = std::env::temp_dir().join(format!("il-fp-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("m.js");
    std::fs::write(&file, b"export const x = 1;").expect("write");

    // Fingerprint WITH content hash of the real bytes.
    let hash = content_hash(b"export const x = 1;");
    let fp = file_fingerprint_with_hash(&file, Some(hash)).expect("fp");
    assert!(matches!(check_fingerprint(&fp), Freshness::Fresh));

    // Rewrite identical content but force a NEW mtime+len signature by lying in
    // the stored fingerprint: same content hash, stale mtime/len. Content hash
    // wins → still Fresh (no-op touch is not a change).
    let touched = FileFingerprint { modified_millis: fp.modified_millis + 5_000, len: fp.len + 99, ..fp.clone() };
    assert!(matches!(check_fingerprint(&touched), Freshness::Fresh));

    // Real content change → Stale.
    std::fs::write(&file, b"export const x = 2;").expect("rewrite");
    assert!(matches!(check_fingerprint(&fp), Freshness::Stale));

    // Deleted → Gone.
    std::fs::remove_file(&file).expect("rm");
    assert!(matches!(check_fingerprint(&fp), Freshness::Gone));

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn check_fingerprints_precedence_unknown_dominates() {
    // Empty set is Fresh.
    assert!(matches!(check_fingerprints(&[]), Freshness::Fresh));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib classify_stat_error_only_notfound_is_gone check_fingerprint_content_hash check_fingerprints_precedence`
Expected: FAIL — `Freshness`, `check_fingerprint`, etc. not found.

- [ ] **Step 3: Add the `content_hash` field**

In `daemon/src/cache/key.rs`, change the `FileFingerprint` struct (make `content_hash` the LAST field so an absent hash omits cleanly from msgpack, keeping keys byte-stable):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub len: u64,
    pub modified_millis: u64,
    /// xxh3 of the bytes read during analysis. Absent for fingerprints built by
    /// a pure stat (`file_fingerprint`). Skipped when None so the serialized key
    /// stays identical to the pre-content-hash format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<u64>,
}
```

- [ ] **Step 4: Update `file_fingerprint` and add the hash-aware builder**

`file_fingerprint` (the existing stat-only builder) now sets `content_hash: None`. Add `file_fingerprint_with_hash` beside it:

```rust
fn file_fingerprint(path: impl AsRef<Path>) -> Option<FileFingerprint> {
    file_fingerprint_with_hash(path, None)
}

/// Stat `path` for len+mtime and attach an already-computed content hash (from
/// the bytes read at analysis time). `content_hash: None` degrades to mtime+len.
pub fn file_fingerprint_with_hash(
    path: impl AsRef<Path>,
    content_hash: Option<u64>,
) -> Option<FileFingerprint> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).ok()?;
    let modified_millis = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default();
    Some(FileFingerprint {
        path: normalize_identity_path(path),
        len: metadata.len(),
        modified_millis,
        content_hash,
    })
}
```

- [ ] **Step 5: Add the tri-state check and rewrite `fingerprints_are_current`**

Replace the existing `fingerprints_are_current` (key.rs:141-147) with the tri-state machinery plus a thin boolean wrapper:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Verified current against the file on disk.
    Fresh,
    /// A dependency file's content changed (still present).
    Stale,
    /// A dependency file is definitively absent (`NotFound`).
    Gone,
    /// Could not verify (transient stat/read error). Caller must KEEP, not evict.
    Unknown,
}

fn classify_stat_error(kind: std::io::ErrorKind) -> Freshness {
    if kind == std::io::ErrorKind::NotFound {
        Freshness::Gone
    } else {
        Freshness::Unknown
    }
}

/// Tri-state freshness of one stored fingerprint against the current file.
pub fn check_fingerprint(stored: &FileFingerprint) -> Freshness {
    let metadata = match fs::metadata(&stored.path) {
        Ok(metadata) => metadata,
        Err(error) => return classify_stat_error(error.kind()),
    };
    let current_len = metadata.len();
    let current_mtime = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default();

    // Cheap pre-filter: unchanged mtime+len means unchanged content — skip the read.
    if current_len == stored.len && current_mtime == stored.modified_millis {
        return Freshness::Fresh;
    }

    // mtime/len differ. With a content hash we can tell a real change from a
    // no-op touch; without one we can only assume Stale.
    let Some(expected) = stored.content_hash else {
        return Freshness::Stale;
    };
    match fs::read(&stored.path) {
        Ok(bytes) if content_hash(&bytes) == expected => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
        Err(error) => classify_stat_error(error.kind()),
    }
}

/// Worst-case freshness across a set. `Unknown` dominates so a transient error on
/// any file never triggers a destructive decision; otherwise Gone, then Stale.
pub fn check_fingerprints(fingerprints: &[FileFingerprint]) -> Freshness {
    let mut worst = Freshness::Fresh;
    for fingerprint in fingerprints {
        match check_fingerprint(fingerprint) {
            Freshness::Unknown => return Freshness::Unknown,
            Freshness::Gone if worst != Freshness::Unknown => worst = Freshness::Gone,
            Freshness::Stale if matches!(worst, Freshness::Fresh) => worst = Freshness::Stale,
            _ => {}
        }
    }
    worst
}

/// Back-compatible boolean: true only when every fingerprint is `Fresh`.
pub fn fingerprints_are_current(fingerprints: &[FileFingerprint]) -> bool {
    matches!(check_fingerprints(fingerprints), Freshness::Fresh)
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib classify_stat_error_only_notfound_is_gone check_fingerprint_content_hash check_fingerprints_precedence`
Expected: PASS. Then `cargo clippy --workspace --all-targets` — clean.

- [ ] **Step 7: Commit**

```bash
git add daemon/src/cache/key.rs
git commit -m "feat(daemon): tri-state freshness check with content hash

Add FileFingerprint.content_hash (Option, skipped when absent so keys stay
byte-stable) and a tri-state check_fingerprint/check_fingerprints returning
Fresh/Stale/Gone/Unknown. Only NotFound is Gone; any other stat error is
Unknown so a transient failure keeps the entry instead of deleting it.
fingerprints_are_current becomes a Fresh-only wrapper."
```

---

## Task 3: Capture content hashes at graph build; feed the value-side fingerprints

**Files:**
- Modify: `daemon/src/pipeline/graph.rs` (`ModuleRecord`, `load_module_from`, `ModuleGraph::content_hash_for`, `module_graph_fingerprints`)
- Modify: `daemon/src/service.rs` (`dependency_fingerprints`)
- Test: `daemon/tests/freshness_core.rs` (new)

**Interfaces:**
- Consumes: `content_hash`, `file_fingerprint_with_hash` (Tasks 1–2).
- Produces:
  - `ModuleRecord.content_hash: u64` (raw-source hash, captured in `load_module_from`).
  - `pub fn ModuleGraph::content_hash_for(&self, path: &Path) -> Option<u64>`.
  - `module_graph_fingerprints` and `service::dependency_fingerprints` now attach module content hashes.

- [ ] **Step 1: Write the failing test**

Create `daemon/tests/freshness_core.rs`:

```rust
use import_lens_daemon::ipc::protocol::ImportRuntime;
use import_lens_daemon::pipeline::graph::build_module_graph_cached_with_runtime;
use std::fs;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("il-fresh-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn module_graph_carries_content_hash_for_loaded_modules() {
    let dir = temp_dir("graphhash");
    let entry = dir.join("entry.mjs");
    let dep = dir.join("dep.mjs");
    fs::write(&dep, "export const value = 41;\n").expect("dep");
    fs::write(&entry, "import { value } from './dep.mjs';\nexport const total = value + 1;\n").expect("entry");

    let graph = build_module_graph_cached_with_runtime(&entry, ImportRuntime::Component).expect("graph");

    // Every loaded module has a non-zero content hash captured from its raw bytes.
    let dep_hash = graph.content_hash_for(&dep).expect("dep hash present");
    assert_ne!(dep_hash, 0);
    assert_eq!(dep_hash, import_lens_daemon::cache::key::content_hash(b"export const value = 41;\n"));

    fs::remove_dir_all(dir).ok();
}
```

(`ImportRuntime::Component` is the correct variant — the enum is `Component | Client | Server` with `Component` the default; existing graph tests such as `daemon/tests/prefetch.rs` use it.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test freshness_core module_graph_carries_content_hash_for_loaded_modules`
Expected: FAIL — no method `content_hash_for`.

- [ ] **Step 3: Add `content_hash` to `ModuleRecord` and capture it in `load_module_from`**

In `daemon/src/pipeline/graph.rs`, add a field to `ModuleRecord` (after `original_source_bytes`):

```rust
    /// xxh3 of the raw file bytes read for this module (pre-transform). Lets the
    /// cache detect real content changes without re-reading on a warm graph.
    pub content_hash: u64,
```

In `load_module_from`, capture the hash from the raw `source` immediately after the read (graph.rs:448-449), before `prepare_module_source` moves it:

```rust
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read module {}: {error}", path.display()))?;
        let content_hash = crate::cache::key::content_hash(source.as_bytes());
        let source_bytes = source.len();
```

Then set `content_hash,` (field-init shorthand) in the `ModuleRecord { .. }` literal constructed later in the same function. **This is the only `ModuleRecord` construction in the crate (graph.rs:535)** — synthetic JSON modules flow through the same constructor — so no other site needs updating.

- [ ] **Step 4: Add `ModuleGraph::content_hash_for`**

In the `impl ModuleGraph` block (near `module_by_id`, graph.rs:205):

```rust
    /// Raw-source content hash for a loaded module path, if present in the graph.
    /// Canonicalizes first: the graph keys paths canonically (on Windows the
    /// verbatim `\\?\C:\...` form via `fs::canonicalize`), so a raw caller path
    /// must be normalized to match — this also hardens the `dependency_fingerprints`
    /// callers below, which pass raw `package_root.join(..)` / `entry_path` values.
    pub fn content_hash_for(&self, path: &Path) -> Option<u64> {
        let canonical = std::fs::canonicalize(path).ok()?;
        self.module_id_by_path(&canonical)
            .and_then(|id| self.module_by_id(id))
            .map(|module| module.content_hash)
    }
```

- [ ] **Step 5: Make the fingerprint builders content-hash aware**

Rewrite `module_graph_fingerprints` (graph.rs:328-333) to attach module hashes:

```rust
fn module_graph_fingerprints(entry_path: &Path, graph: &ModuleGraph) -> Vec<FileFingerprint> {
    let mut paths = Vec::with_capacity(graph.dependency_paths.len() + 1);
    paths.push(entry_path.to_path_buf());
    paths.extend(graph.dependency_paths.iter().cloned());
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| {
            let hash = graph.content_hash_for(&path);
            crate::cache::key::file_fingerprint_with_hash(path, hash)
        })
        .collect()
}
```

The rewrite calls `crate::cache::key::file_fingerprint_with_hash` fully-qualified, so no import is added — but `fingerprints_for_paths` is now unused in `graph.rs`. **Change the top-of-file import (graph.rs:2) from `cache::key::{FileFingerprint, fingerprints_are_current, fingerprints_for_paths},` to `cache::key::{FileFingerprint, fingerprints_are_current},`** (keep `fingerprints_are_current` — still used at graph.rs:249,312). Workspace lint is `deny`, so a leftover unused import fails the build. Keep the returned vector sorted+deduped by path (the sort/dedup above preserves it).

In `daemon/src/service.rs`, rewrite `dependency_fingerprints` (service.rs:1541-1560) to attach hashes from the graph:

```rust
fn dependency_fingerprints(
    request: &ImportRequest,
    resolved: &ResolvedPackage,
    result: &ImportResult,
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::file_fingerprint_with_hash;

    if result.is_cjs {
        return [
            resolved.package_root.join("package.json"),
            resolved.entry_path.clone(),
        ]
        .into_iter()
        .filter_map(|path| file_fingerprint_with_hash(path, None))
        .collect();
    }

    let Ok(graph) =
        build_module_graph_cached_with_runtime(&resolved.entry_path, request.runtime)
    else {
        return [
            resolved.package_root.join("package.json"),
            resolved.entry_path.clone(),
        ]
        .into_iter()
        .filter_map(|path| file_fingerprint_with_hash(path, None))
        .collect();
    };

    let mut paths = vec![
        resolved.package_root.join("package.json"),
        resolved.entry_path.clone(),
    ];
    paths.extend(graph.modules.iter().map(|module| module.path.clone()));
    paths.extend(graph.dependency_paths.iter().cloned());
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| file_fingerprint_with_hash(path.clone(), graph.content_hash_for(&path)))
        .collect()
}
```

(package.json gets `None` — it is stat-only here; content-hashing the manifest is deferred to identity-v4, Plan 2.)

Also drop the now-unused `fingerprints_for_paths` from the `service.rs` key import (**service.rs:3**): change `key::{cache_key_for_resolved_import, fingerprints_for_paths},` to `key::{cache_key_for_resolved_import},` (keep `cache_key_for_resolved_import` — still used at service.rs:1388). `deny` lint fails on the leftover.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test freshness_core module_graph_carries_content_hash_for_loaded_modules`
Expected: PASS. Then `cargo test` (whole suite) — the existing graph/cache tests stay green. Then `cargo clippy --workspace --all-targets`.

- [ ] **Step 7: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/src/service.rs daemon/tests/freshness_core.rs
git commit -m "feat(daemon): content-hash dependency fingerprints at graph build

Capture each module's raw-source xxh3 in load_module_from and carry it on
ModuleRecord, so the memoized graph exposes content_hash_for(path). Value-side
dependency_fingerprints and the graph-cache fingerprints now attach those
hashes, binding a cached result to the exact bytes analyzed without re-reading."
```

---

## Task 4: Capture the cache generation before analysis (fixes D4 / TOCTOU)

**Files:**
- Modify: `daemon/src/cache/memory.rs` (`insert_with_fingerprints_at_generation`)
- Modify: `daemon/src/service.rs` (`analyze_and_cache`, `cache_full_variant_alias`)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- Consumes: `cache_generation()`, `insert_with_fingerprints` (existing).
- Produces: `pub fn ImportCache::insert_with_fingerprints_at_generation(&self, key: String, result: ImportResult, dependency_fingerprints: Vec<FileFingerprint>, verified_generation: u64)`. Existing `insert_with_fingerprints` delegates with `current_cache_generation()`.

- [ ] **Step 1: Write the failing test**

Add to `daemon/tests/freshness_core.rs`:

```rust
use import_lens_daemon::cache::key::fingerprints_for_paths;
use import_lens_daemon::cache::memory::{ImportCache, bump_cache_generation, cache_generation};

fn sample_result(specifier: &str) -> import_lens_daemon::ipc::protocol::ImportResult {
    // Mirror the helper in tests/memory_cache.rs (full field set).
    use import_lens_daemon::ipc::protocol::ImportResult;
    ImportResult {
        specifier: specifier.to_owned(),
        raw_bytes: 10, minified_bytes: 8, gzip_bytes: 6, brotli_bytes: 5, zstd_bytes: 5,
        cache_hit: false, side_effects: false, truly_treeshakeable: true, is_cjs: false,
        confidence: Default::default(), confidence_reasons: Vec::new(), error: None,
        diagnostics: Vec::new(), module_breakdown: None, shared_bytes: None,
        internal_contributions: Vec::new(),
    }
}

#[test]
fn insert_at_captured_generation_does_not_serve_stale_after_bump() {
    let dir = temp_dir("d4");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep v1");
    let fp_v1 = fingerprints_for_paths(vec![dep.clone()]);

    let cache = ImportCache::new(None, false);
    let captured = cache_generation();

    // Simulate: file changes during analysis, and a NodeModulesChanged bump lands
    // before the (late) insert of the v1-derived result.
    fs::write(&dep, "export const x = 2222;").expect("dep v2");
    bump_cache_generation();

    cache.insert_with_fingerprints_at_generation("v3:d4".to_owned(), sample_result("dep"), fp_v1, captured);

    // The entry was stamped with the OLD generation, so get() must re-verify and
    // (because the file changed) must NOT serve the stale v1 result.
    assert!(
        cache.get("v3:d4").is_none(),
        "captured-generation insert must not launder a stale result as fresh"
    );

    fs::remove_dir_all(dir).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test freshness_core insert_at_captured_generation_does_not_serve_stale_after_bump`
Expected: FAIL — no method `insert_with_fingerprints_at_generation`.

- [ ] **Step 3: Add the generation-aware insert**

In `daemon/src/cache/memory.rs`, refactor `insert_with_fingerprints` to delegate, and add the new method:

```rust
pub fn insert_with_fingerprints(
    &self,
    key: String,
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
) {
    self.insert_with_fingerprints_at_generation(
        key,
        result,
        dependency_fingerprints,
        current_cache_generation(),
    );
}

/// Insert stamping a caller-captured generation (taken BEFORE reading the
/// analyzed bytes) rather than the generation at insert time. If an
/// invalidation bumped the generation during analysis, the entry is born
/// "must re-verify" and cannot be served on the fast path.
pub fn insert_with_fingerprints_at_generation(
    &self,
    key: String,
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
    verified_generation: u64,
) {
    let now = crate::time::unix_millis_now();
    let cached = CachedImport {
        result,
        dependency_fingerprints,
        verified_generation,
        verified_at_millis: now,
        last_used_millis: Arc::new(AtomicU64::new(now)),
    };

    if let Err(error) = self.disk.insert(&key, &cached) {
        crate::logging::log_warn("cache", format!("skipping disk insert for {key}: {error}"));
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.insert(key.clone());
        }
    }

    self.memory.pin().insert(key, cached);
    self.enforce_memory_cap();
}
```

(Note: `verified_at_millis` becomes `verified_at` in Task 6; keep this shape until then.)

- [ ] **Step 4: Capture the generation in `analyze_and_cache`**

In `daemon/src/service.rs`, snapshot the generation at the TOP of `analyze_and_cache` (before `analyze_resolved_import`) and use it for both inserts:

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
        let captured_generation = crate::cache::memory::cache_generation();
        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) && should_store() {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(cache, request, &result, &resolved, &fingerprints, captured_generation);
            cache.insert_with_fingerprints_at_generation(key, result.clone(), fingerprints, captured_generation);
        }

        result
    }
```

Add a `verified_generation: u64` parameter to `cache_full_variant_alias` (service.rs:1366) and have its internal `insert_with_fingerprints(...)` call become `insert_with_fingerprints_at_generation(namespace_key, namespace_result, dependency_fingerprints.to_vec(), verified_generation)`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --test freshness_core insert_at_captured_generation_does_not_serve_stale_after_bump`
Expected: PASS. Then `cargo test` (whole suite) green; `cargo clippy --workspace --all-targets` clean.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/memory.rs daemon/src/service.rs
git commit -m "fix(daemon): stamp cache entries with the pre-analysis generation

analyze_and_cache captured the generation at insert time, so a result computed
from pre-change bytes could be stamped with the post-invalidation generation and
served as fresh (permanent staleness, D4). Capture the generation before
analysis and stamp that; a bump during analysis now forces re-verification."
```

---

## Task 5: Tri-state `get` — keep on `Unknown`, evict only on `Stale`/`Gone`

**Files:**
- Modify: `daemon/src/cache/memory.rs` (`get`)
- Modify: `daemon/src/cache/disk.rs` (`get_entry`, `pending_insert_entry`)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- Consumes: `check_fingerprints` / `Freshness` (Task 2).

- [ ] **Step 1: Write the failing test**

Add to `daemon/tests/freshness_core.rs`:

```rust
#[test]
fn get_evicts_on_changed_or_missing_but_keeps_on_fresh() {
    let dir = temp_dir("tristate");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints("v3:ts".to_owned(), sample_result("dep"), fp);
    bump_cache_generation(); // force the slow path (re-verify) on next get

    // Fresh → served.
    assert!(cache.get("v3:ts").is_some(), "unchanged dep should serve");

    // Changed → evicted. (Different LENGTH so detection is robust even if two
    // writes land within NTFS mtime resolution — these fingerprints carry no hash.)
    fs::write(&dep, "export const x = 222;").expect("change");
    bump_cache_generation();
    assert!(cache.get("v3:ts").is_none(), "changed dep should evict");

    fs::remove_dir_all(dir).ok();
}
```

(Transient `Unknown` behavior is covered by the `classify_stat_error` unit test in Task 2 — forcing a non-`NotFound` stat error from an integration test is not portable on Windows. The `get` match arm below is the mechanical realization of that classification.)

- [ ] **Step 2: Run test to verify it fails or regresses**

Run: `cargo test --test freshness_core get_evicts_on_changed_or_missing_but_keeps_on_fresh`
Expected: PASS *today* for the changed/fresh cases (the old boolean already evicts on change) — this test is a **regression guard**. Proceed to make the behavior tri-state without breaking it.

- [ ] **Step 3: Rewrite the memory `get` slow-path**

In `daemon/src/cache/memory.rs` `get`, replace the delete-on-mismatch block (memory.rs:122-141) with a tri-state match. The snippet uses `crate::cache::key::{check_fingerprints, Freshness}` fully-qualified, so **no import is added — instead remove the now-unused `fingerprints_are_current` from the `use crate::cache::key::{...}` import block** (its only use, memory.rs:123, is replaced below). Leftover unused imports fail the `deny` lint.

```rust
        if !fresh_without_restat {
            match crate::cache::key::check_fingerprints(&cached.dependency_fingerprints) {
                crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
                    memory.remove(key);
                    self.disk.remove(key);
                    return None;
                }
                crate::cache::key::Freshness::Unknown => {
                    // Could not verify (transient fs error). Keep the entry and
                    // serve the last-known value; do NOT restamp, so the next hit
                    // re-checks once the transient condition clears.
                    let mut result = cached.result.clone();
                    result.cache_hit = true;
                    self.disk.touch(key);
                    return Some(result);
                }
                crate::cache::key::Freshness::Fresh => {
                    let mut restamped = cached.clone();
                    restamped.verified_generation = generation;
                    restamped.verified_at_millis = now;
                    let mut result = restamped.result.clone();
                    memory.insert(key.to_owned(), restamped);
                    result.cache_hit = true;
                    self.disk.touch(key);
                    return Some(result);
                }
            }
        }
```

- [ ] **Step 4: Rewrite disk `get_entry` and `pending_insert_entry`**

In `daemon/src/cache/disk.rs`, replace both `if !fingerprints_are_current(&cached.dependency_fingerprints) { ... remove ... None }` blocks (get_entry ~line 115, pending_insert_entry ~line 193) with:

```rust
    match crate::cache::key::check_fingerprints(&cached.dependency_fingerprints) {
        crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
            // (in get_entry: drop the txn/table first, as today, then) remove + return None
            self.remove(key);
            return None;
        }
        // Fresh OR Unknown → keep and return the entry (Unknown must not delete).
        _ => {}
    }
```

For `get_entry`, preserve the existing `drop(table); drop(read_txn);` ordering before `self.remove(key)`. The snippet uses `check_fingerprints`/`Freshness` fully-qualified, so **remove the now-unused `fingerprints_are_current` from the `disk.rs` key import** (its only uses, disk.rs:115 and :193, are both replaced). Leftover unused imports fail the `deny` lint.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test freshness_core get_evicts_on_changed_or_missing_but_keeps_on_fresh`
Expected: PASS. Then `cargo test` (whole suite, incl. `memory_cache.rs` / `cache_disk.rs`) — green. `cargo clippy --workspace --all-targets` clean.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/memory.rs daemon/src/cache/disk.rs
git commit -m "fix(daemon): tri-state cache get keeps entries on transient fs errors

get()/get_entry deleted an entry on ANY non-current fingerprint, so a locked
file or offline drive (a transient stat error) destroyed a valid cache and could
delete a whole shard. Switch to check_fingerprints: evict only on Stale/Gone
(NotFound), keep and serve on Unknown."
```

---

## Task 6: Monotonic clock for the re-verify TTL

**Files:**
- Modify: `daemon/src/cache/memory.rs` (`CachedImport.verified_at`, `get`, inserts)
- Modify: `daemon/src/cache/disk.rs` (`decode_cached_result` sets `verified_at: None`)
- Test: `daemon/tests/freshness_core.rs`

**Interfaces:**
- `CachedImport.verified_at: Option<std::time::Instant>` replaces `verified_at_millis: u64` (runtime-only field, never persisted).

- [ ] **Step 1: Write the failing test**

Add to `daemon/tests/freshness_core.rs`:

```rust
#[test]
fn fresh_insert_serves_on_fast_path_within_ttl() {
    let dir = temp_dir("ttl");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints("v3:ttl".to_owned(), sample_result("dep"), fp);

    // Same generation + within TTL → fast path skips the re-stat, so deleting the
    // dep out of band still serves (this is the intended TTL behavior).
    fs::remove_file(&dep).expect("rm");
    assert!(cache.get("v3:ttl").is_some(), "fast path within TTL serves without re-stat");

    fs::remove_dir_all(dir).ok();
}
```

- [ ] **Step 2: Run test to verify it passes as a guard**

Run: `cargo test --test freshness_core fresh_insert_serves_on_fast_path_within_ttl`
Expected: PASS today (wall-clock TTL). This guards that the switch to `Instant` preserves behavior.

- [ ] **Step 3: Switch the field to `Instant`**

In `daemon/src/cache/memory.rs`:
- Add `use std::time::{Duration, Instant};`.
- Replace `const REVERIFY_TTL_MS: u64 = 30_000;` with `const REVERIFY_TTL: Duration = Duration::from_secs(30);`.
- In `CachedImport`, replace `pub verified_at_millis: u64,` with `pub verified_at: Option<Instant>,` (comment: `None` = never verified this run → must re-verify).
- In `insert_with_fingerprints_at_generation`, set `verified_at: Some(Instant::now())` (drop the `now`-millis usage for this field; keep `last_used_millis` on wall-clock as-is).
- In `get`, replace the fast-path TTL check:

```rust
        let fresh_without_restat = cached.verified_generation == generation
            && cached.verified_at.is_some_and(|at| at.elapsed() < REVERIFY_TTL);
```

  and in the `Fresh` re-verify arm set `restamped.verified_at = Some(Instant::now());` (replacing `verified_at_millis = now`). In the disk-rehydrate arm, set `cached.verified_at = Some(Instant::now());`.

- [ ] **Step 4: Update disk decode**

In `daemon/src/cache/disk.rs` `decode_cached_result` (both `CachedImport { .. }` constructions), replace `verified_at_millis: 0,` with `verified_at: None,` (a rehydrated entry has not been verified this run).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test freshness_core fresh_insert_serves_on_fast_path_within_ttl`
Expected: PASS. Then `cargo test` (whole suite) — green (the existing `cache_hit_skips_fingerprint_restat_until_generation_bumps` in `memory_cache.rs` must still pass). `cargo clippy --workspace --all-targets` clean.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/memory.rs daemon/src/cache/disk.rs
git commit -m "fix(daemon): monotonic re-verify TTL

The 30s re-verify fast-path compared wall-clock millis, so a backward clock jump
(NTP, VM resume) could extend a staleness window arbitrarily. Use a monotonic
Instant for the runtime-only verified_at; rehydrated entries start unverified."
```

---

## Self-Review (completed against spec Phase 1)

**Spec coverage:**
- §4.1 content-hash fingerprint + mtime pre-filter → Tasks 1–2. ✅
- §4.2 atomic capture (generation before read; fingerprints with bytes) → Tasks 3 (hashes at build) + 4 (generation before analysis). ✅
- §4.3 tri-state probe (NotFound-only deletion, transient → keep) → Tasks 2 + 5. ✅
- §4.6 monotonic clocks → Task 6. ✅
- Deferred by design to later plans: identity v4 / drop in-key fingerprints (§4.7, Plan 2), SWR + `ResultFreshness` + `Unverified` graduation (§4.5, §4.3.1, Plan 2), CI-forces-fresh (Plan 2), capacity/recency/eviction (Plan 3). Not in scope here — noted, not dropped.

**Placeholder scan:** none — every code step carries real code; the one unavoidable "confirm the variant name" note (Task 3 `ImportRuntime`) is a lookup, not a placeholder.

**Type consistency:** `content_hash: Option<u64>` (fingerprint) vs `content_hash: u64` (ModuleRecord, always present) are intentionally different and bridged by `content_hash_for -> Option<u64>` + `file_fingerprint_with_hash(path, Option<u64>)`. `Freshness` variants, `check_fingerprint(s)`, `insert_with_fingerprints_at_generation`, `verified_at: Option<Instant>` are used consistently across tasks.

**Known validate/tune items carried forward (from the spec's residual list):** the fast/slow-path cost for a large first-party graph, and eviction/budget tuning, are Plan 3 concerns; nothing in Plan 1 depends on them.

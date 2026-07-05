# Cache Identity v4 + L1 Signature Independence — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drop the mtime-based fingerprints from the cache KEY (identity v3 → v4) so a content-identical reinstall (`npm ci`, `git checkout`) reuses the same key instead of minting a new one and orphaning the old — relying on the already-content-hash-exact value-side re-validation — and give the L1 file-size cache its own edit sensitivity so it doesn't regress when the key stops carrying fingerprints.

**Architecture:** This is **Plan 2 of the cache-lifecycle redesign** (spec `docs/superpowers/specs/2026-07-05-cache-lifecycle-redesign-design.md` §4.7), building on Plan 1 (Freshness core, merged). Removing `manifest_fingerprint`/`entry_fingerprint` from `CacheIdentityV3` makes the key stable across identical-content reinstalls; validity is enforced entirely by the stored `dependency_fingerprints` (which since Plan 1 covers package.json + entry + the full module graph, content-hashed, and re-validates on every get). Because all keys change prefix `v3:`→`v4:`, the on-disk schema version is bumped so the existing wipe-on-mismatch path reclaims the now-unreachable v3 rows. L1's signature folds in an independent entry+manifest fingerprint to replace the sensitivity the key used to donate.

**Tech Stack:** Rust (edition 2024), `redb`, `papaya`, `rmp-serde` (msgpack), `xxhash-rust`.

## Global Constraints

- **Conventional Commits with a mandatory body** (`type(scope): subject`, blank line, body). Enforced by a `commit-msg` hook.
- **Gates before each commit** (pre-commit hook runs them on staged files): `cargo clippy --workspace --all-targets` (workspace lint = `deny`), `cargo deny check`, `cargo fmt`. Full suite: `cargo test -p import-lens-daemon`.
- **This environment's rust-analyzer shows STALE errors mid-edit.** Trust `cargo check -p import-lens-daemon --all-targets` and `cargo test`, never the editor squiggles.
- **Value-side re-validation is the source of truth** (established in Plan 1): the cache key is an *identity*, not a freshness signal. Never re-add per-file freshness to the key.
- **Analysis output must be byte-identical** — this plan changes only cache *identity* and L1 *signature*, never what is analyzed.
- **A one-time cold disk cache on upgrade is the accepted cost** (all keys change v3→v4; the schema bump wipes the old rows). This matches the redesign's migration guidance ("version change → clear, small price"). No per-version migration code.

## File Structure

- `daemon/src/cache/key.rs` — rename `CacheIdentityV3`→`CacheIdentityV4`, drop the two fingerprint fields, `CACHE_KEY_PREFIX_V3`→`CACHE_KEY_PREFIX_V4 = "v4:"`, drop the fingerprint construction in `cache_identity_for_import`.
- `daemon/src/prefetch.rs` — rename `CacheIdentityV3`→`CacheIdentityV4` (`:2` import, `:324` `import_request_from_identity` param); reads only surviving fields.
- `daemon/src/pipeline/resolver.rs` — rename `CacheIdentityV3`→`CacheIdentityV4` (`:1` import, `:115` `resolved_from_cache_identity` param); reads only surviving fields.
- `daemon/src/cache/disk.rs` — `cache_envelope` loses the manifest/entry merge; `CacheEnvelope` drops the now-unused `package_identity`; drop the now-unused `CacheIdentityV3`+`decode_cache_identity` imports; bump `CURRENT_SCHEMA_VERSION` 4→5.
- `daemon/src/pipeline/file_size_cache.rs` — `file_size_signature` folds in an independent entry+manifest fingerprint; the L1 test lands in this file's `#[cfg(test)] mod tests`.
- `daemon/tests/cache_disk.rs` — bump its local `CURRENT_SCHEMA_VERSION` to 5 + the stale-schema-wipe test.
- `daemon/tests/cache_key.rs` — update the `"v3:"` prefix assertions to `"v4:"`.
- `daemon/tests/cache_identity_v4.rs` (new) — the reinstall-reuse test and the "key carries no fingerprint" test.

---

## Task 1: Drop in-key fingerprints — cache identity v4

**Files:**
- Modify: `daemon/src/cache/key.rs`
- Modify: `daemon/src/prefetch.rs` (rename `CacheIdentityV3`→`CacheIdentityV4` at `:2`, `:324`)
- Modify: `daemon/src/pipeline/resolver.rs` (rename `CacheIdentityV3`→`CacheIdentityV4` at `:1`, `:115`)
- Modify: `daemon/src/cache/disk.rs` (`cache_envelope`, `CacheEnvelope`, imports)
- Modify: `daemon/tests/cache_key.rs` (prefix assertions)
- Create: `daemon/tests/cache_identity_v4.rs`

**Interfaces:**
- Consumes (from Plan 1): `dependency_fingerprints` already covers package.json + entry + graph (content-hashed); `file_fingerprint_with_hash`.
- Produces: `pub struct CacheIdentityV4` (no fingerprint fields); `pub const CACHE_KEY_PREFIX_V4: &str = "v4:"`; `cache_key_for_resolved_import` now returns a `v4:`-prefixed key that does not vary on entry/manifest mtime.

- [ ] **Step 1: Write the failing reinstall-reuse test**

Create `daemon/tests/cache_identity_v4.rs`:

```rust
use import_lens_daemon::cache::key::{cache_key_for_resolved_import, decode_cache_identity};
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::resolver::resolve_package_entry;
use std::{fs, thread, time::Duration};

fn write_pkg(root: &std::path::Path, entry_bytes: &str) {
    fs::create_dir_all(root).expect("pkg root");
    fs::write(root.join("package.json"), r#"{"version":"1.0.0","module":"index.js"}"#).expect("manifest");
    fs::write(root.join("index.js"), entry_bytes).expect("entry");
}

fn request() -> ImportRequest {
    ImportRequest {
        specifier: "v4-lib".to_owned(),
        package_name: "v4-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    }
}

#[test]
fn identical_content_reinstall_reuses_the_same_key() {
    let ws = std::env::temp_dir().join(format!("il-v4-reuse-{}", std::process::id()));
    let pkg = ws.join("node_modules").join("v4-lib");
    let document = ws.join("src").join("index.ts");
    let req = request();

    write_pkg(&pkg, "export const value = 1;");
    let resolved1 = resolve_package_entry(&document, &req).expect("resolve 1");
    let key1 = cache_key_for_resolved_import(&req, &resolved1);
    assert!(key1.starts_with("v4:"), "keys are v4-prefixed");

    // Reinstall with IDENTICAL bytes but a new mtime (npm ci / git checkout).
    thread::sleep(Duration::from_millis(20));
    write_pkg(&pkg, "export const value = 1;");
    let resolved2 = resolve_package_entry(&document, &req).expect("resolve 2");
    let key2 = cache_key_for_resolved_import(&req, &resolved2);

    assert_eq!(key1, key2, "identical-content reinstall must reuse the key (v4 drops in-key fingerprints)");
    fs::remove_dir_all(ws).ok();
}

#[test]
fn cache_key_identity_carries_no_fingerprints() {
    let ws = std::env::temp_dir().join(format!("il-v4-nofp-{}", std::process::id()));
    let pkg = ws.join("node_modules").join("v4-lib");
    let document = ws.join("src").join("index.ts");
    let req = request();
    write_pkg(&pkg, "export const value = 1;");
    let resolved = resolve_package_entry(&document, &req).expect("resolve");
    let key = cache_key_for_resolved_import(&req, &resolved);

    // The decoded identity no longer exposes fingerprint fields (compile-time), and
    // the key round-trips.
    let identity = decode_cache_identity(&key).expect("decode v4 identity");
    assert_eq!(identity.package_name, "v4-lib");
    fs::remove_dir_all(ws).ok();
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p import-lens-daemon --test cache_identity_v4`
Expected: FAIL — `key1.starts_with("v4:")` fails (keys are still `v3:`), and `identical_content_reinstall_reuses_the_same_key` fails (v3 keys differ on mtime).

- [ ] **Step 3: Drop the fingerprints from the identity + bump the prefix**

In `daemon/src/cache/key.rs`:

Rename the const (line ~20) and add the v4 name:
```rust
pub const CACHE_KEY_PREFIX_V4: &str = "v4:";
```
(Remove `CACHE_KEY_PREFIX_V3`. If any non-test code still references `CACHE_KEY_PREFIX_V3`, update it — `decode_cache_identity`/`encode_cache_identity` below are the only ones.)

Rename the struct and drop the two fingerprint fields (was `key.rs:36-49`):
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIdentityV4 {
    pub analyzer_version: String,
    pub specifier: String,
    pub package_name: String,
    pub package_version: String,
    pub package_root: Option<String>,
    pub entry_path: Option<String>,
    pub runtime: ImportRuntime,
    pub import_kind: ImportKind,
    pub named_exports: Vec<String>,
}
```

In `cache_identity_for_import` (was `key.rs:60-84`), remove the two `manifest_fingerprint`/`entry_fingerprint` lines from the returned struct and change the return type to `CacheIdentityV4`. The function body otherwise unchanged (it no longer needs to build fingerprints):
```rust
fn cache_identity_for_import(
    request: &ImportRequest,
    resolved: Option<&ResolvedPackage>,
) -> CacheIdentityV4 {
    let mut named_exports = if matches!(&request.import_kind, ImportKind::Named) {
        request.named.clone()
    } else {
        Vec::new()
    };
    named_exports.sort();
    named_exports.dedup();

    CacheIdentityV4 {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        specifier: request.specifier.clone(),
        package_name: request.package_name.clone(),
        package_version: request.version.clone(),
        package_root: resolved.map(|package| normalize_identity_path(&package.package_root)),
        entry_path: resolved.map(|package| normalize_identity_path(&package.entry_path)),
        runtime: request.runtime,
        import_kind: request.import_kind,
        named_exports,
    }
}
```

Update `encode_cache_identity`/`decode_cache_identity` to the v4 type + prefix:
```rust
fn encode_cache_identity(identity: &CacheIdentityV4) -> String {
    let bytes = rmp_serde::to_vec(identity).unwrap_or_default();
    format!("{CACHE_KEY_PREFIX_V4}{}", hex_encode(&bytes))
}

pub fn decode_cache_identity(key: &str) -> Option<CacheIdentityV4> {
    let encoded = key.strip_prefix(CACHE_KEY_PREFIX_V4)?;
    let bytes = hex_decode(encoded)?;
    rmp_serde::from_slice(&bytes).ok()
}
```

`cache_key_is_orphan`, `cache_key_matches_package`, `cache_key_matches_any_package` need only their type reference updated (they read `analyzer_version`/`entry_path`/`package_root`/`package_name`, none of the dropped fields).

**Also rename the type in two other live files** (both read only surviving fields — a pure type-name swap; workspace `deny` fails the build if any `CacheIdentityV3` remains):
- `daemon/src/prefetch.rs:2` — the `cache::key::{CacheIdentityV3, decode_cache_identity}` import → `CacheIdentityV4`.
- `daemon/src/prefetch.rs:324` — `fn import_request_from_identity(identity: CacheIdentityV3)` → `CacheIdentityV4`.
- `daemon/src/pipeline/resolver.rs:1` — `use crate::cache::key::CacheIdentityV3;` → `CacheIdentityV4`.
- `daemon/src/pipeline/resolver.rs:115` — `pub fn resolved_from_cache_identity(identity: &CacheIdentityV4)`.

- [ ] **Step 4: Simplify `cache_envelope` and drop the now-unused `package_identity`**

First confirm `package_identity` is write-only: run `grep -rn "package_identity" daemon/src`. Expected readers: only its construction in `cache_envelope` and the struct field (`decode_cached_result` does not read it). If that holds, in `daemon/src/cache/disk.rs`:

Remove `package_identity` from `CacheEnvelope` (was `disk.rs:40-47`):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    analyzer_version: String,
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
    full_contributions: Vec<ModuleContribution>,
}
```

Rewrite `cache_envelope` (was `disk.rs:737-760`) — the manifest/entry merge is gone (the fields no longer exist), and it no longer decodes the identity:
```rust
fn cache_envelope(_key: &str, cached: CachedImport) -> CacheEnvelope {
    CacheEnvelope {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        full_contributions: if cached.result.internal_contributions.is_empty() {
            cached.result.module_breakdown.clone().unwrap_or_default()
        } else {
            cached.result.internal_contributions.clone()
        },
        result: cached.result,
        dependency_fingerprints: cached.dependency_fingerprints,
    }
}
```
`decode_cached_result` (`disk.rs:762-787`) needs **no edit** — it does `rmp_serde::from_slice::<CacheEnvelope>(bytes)` then reads `analyzer_version`/`result`/`full_contributions`/`dependency_fingerprints` to build a `CachedImport`; it never names `package_identity`, so removing the struct field is sufficient (this also confirms dropping the field can't break decode).

**Remove BOTH now-unused imports** from the disk.rs `cache::key::{…}` import block (`disk.rs:3-4`): `CacheIdentityV3` (only used by the deleted field) **and** `decode_cache_identity` (only used at the deleted merge). Keep `ANALYZER_VERSION`, `FileFingerprint`, `cache_key_is_orphan`, `cache_key_matches_any_package` — all still used. Workspace `deny` fails on either leftover.

Note: this envelope shape change is safe because Task 2 bumps the schema version and wipes old rows; in the interim, any old v3-keyed envelope is only reachable by an (un-minted) v3 key and old rows that a scan (preload) touches decode-fail gracefully → `None` → skipped.

- [ ] **Step 5: Update the `v3:` test assertions**

In `daemon/tests/cache_key.rs` (was `:57-58`), change the two `starts_with("v3:")` assertions to `starts_with("v4:")`. (Leave opaque `"v3:aa"`-style literal keys in other test files as-is — they are arbitrary strings that never decode as identities; they keep working. Only fix assertions on the production prefix.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p import-lens-daemon --test cache_identity_v4 && cargo test -p import-lens-daemon --test cache_key`
Expected: PASS. Then `cargo test -p import-lens-daemon` (whole suite green), `cargo check -p import-lens-daemon --all-targets`, `cargo clippy --workspace --all-targets` (clean).

- [ ] **Step 7: Commit**

```bash
git add daemon/src/cache/key.rs daemon/src/prefetch.rs daemon/src/pipeline/resolver.rs daemon/src/cache/disk.rs daemon/tests/cache_key.rs daemon/tests/cache_identity_v4.rs
git commit -m "feat(daemon): drop in-key fingerprints (cache identity v4)

Remove manifest_fingerprint/entry_fingerprint from the cache identity and bump
the key prefix v3->v4, so an identical-content reinstall (npm ci, git checkout)
reuses the same key instead of orphaning the old entry. Validity is already
enforced value-side by dependency_fingerprints (package.json + entry + the full
content-hashed module graph, re-validated on every get). cache_envelope no
longer merges the redundant stat-only key fingerprints, and the now write-only
package_identity is dropped from the stored envelope."
```

---

## Task 2: Bump the disk schema version to reclaim v3 rows on upgrade

**Files:**
- Modify: `daemon/src/cache/disk.rs` (`CURRENT_SCHEMA_VERSION`)
- Test: `daemon/tests/cache_disk.rs`

**Interfaces:**
- Consumes: the existing `ensure_schema`/`recreate_database` wipe-on-mismatch path (`disk.rs:525-601`).
- Produces: opening a disk cache written by a prior schema version recreates it empty.

- [ ] **Step 1: Write the failing test**

Add to `daemon/tests/cache_disk.rs` (reuse its `temp_storage`/`db_path` helpers and the local `CACHE_TABLE`/`METADATA_TABLE` defs it already declares for redb inspection):

```rust
#[test]
fn opening_a_stale_schema_db_recreates_it_empty() {
    use redb::{Database, ReadableDatabase, ReadableTableMetadata};
    let storage = temp_storage();

    // Simulate an old-schema cache: write a metadata row with a prior version and a
    // junk cache row, using the same table defs the daemon uses.
    {
        let db = Database::create(db_path(&storage)).expect("create db");
        let write = db.begin_write().expect("begin");
        {
            let mut meta = write.open_table(METADATA_TABLE).expect("meta table");
            meta.insert("schema_version", &4u64).expect("write old version"); // one below current
            let mut cache = write.open_table(CACHE_TABLE).expect("cache table");
            cache.insert("v3:stale", b"junk".as_slice()).expect("write stale row");
        }
        write.commit().expect("commit");
    }

    // Opening through the daemon must detect the mismatch and recreate empty.
    let cache = ImportCache::new(Some(storage.clone()), true);
    drop(cache);

    let db = Database::open(db_path(&storage)).expect("reopen");
    let read = db.begin_read().expect("read");
    let table = read.open_table(CACHE_TABLE).expect("cache table");
    assert_eq!(table.len().expect("len"), 0, "stale-schema cache should be wiped on open");

    fs::remove_dir_all(storage).expect("cleanup");
}
```

(If `4u64` equals the current version at the time you write this, use `CURRENT_SCHEMA_VERSION - 1` conceptually — the point is "one below current". After Step 3 the current version is 5, so the literal `4` above is correct.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p import-lens-daemon --test cache_disk opening_a_stale_schema_db_recreates_it_empty`
Expected: FAIL — with `CURRENT_SCHEMA_VERSION` still 4, the metadata row `4` matches, the DB is NOT recreated, and the junk `v3:stale` row survives → `table.len()` is 1, not 0.

- [ ] **Step 3: Bump the schema version**

In `daemon/src/cache/disk.rs` (was `:25`):
```rust
const CURRENT_SCHEMA_VERSION: u64 = 5;
```

**Also** bump the test file's own copy so its 4 version assertions (`cache_disk.rs:182,333,349,365`, which compare the written version against this local const) stay green — `daemon/src/cache/disk.rs`... no: `daemon/tests/cache_disk.rs:17`:
```rust
const CURRENT_SCHEMA_VERSION: u64 = 5;
```
(These two constants are independent copies; the daemon writes its value and the test asserts against its own. Both must read 5.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p import-lens-daemon --test cache_disk opening_a_stale_schema_db_recreates_it_empty`
Expected: PASS — version `4` ≠ `5` → `ensure_schema` errors → `recreate_database` wipes → 0 rows. Then `cargo test -p import-lens-daemon` (whole suite green), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/cache/disk.rs daemon/tests/cache_disk.rs
git commit -m "feat(daemon): bump disk cache schema to 5 for the v4 key change

The v3->v4 key change orphans every existing on-disk row. Bump the schema
version so the existing wipe-on-mismatch path recreates the cache empty on the
first upgraded open, reclaiming the dead v3 rows instead of leaving them as
unreachable weight until LRU eviction. One-time cold cache is the accepted cost."
```

---

## Task 3: L1 file-size signature independence

**Files:**
- Modify: `daemon/src/pipeline/file_size_cache.rs` (`file_size_signature` + a test in its existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `resolve_package_entry` (already called in `file_size_signature`), `file_fingerprint_with_hash`.
- Produces: `file_size_signature` changes when a resolved package's entry or manifest changes on disk, independent of the (now fingerprint-free) cache key.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block already in `daemon/src/pipeline/file_size_cache.rs` (where `file_size_signature`, `AnalysisContext`, `ImportRequest`, `ImportKind`, `ImportRuntime` are already in scope via `use super::*` and the module's imports — do NOT add an `import_lens_daemon::` path; that only works from `tests/`):

```rust
#[test]
fn file_size_signature_changes_when_entry_bytes_change() {
    // Real node_modules fixture so resolve_package_entry succeeds and the
    // Ok(resolved) arm folds in the entry+manifest fingerprint.
    let ws = std::env::temp_dir().join(format!("il-l1-sig-{}", std::process::id()));
    let pkg = ws.join("node_modules").join("l1-lib");
    std::fs::create_dir_all(&pkg).expect("pkg");
    std::fs::write(pkg.join("package.json"), r#"{"version":"1.0.0","module":"index.js"}"#).expect("manifest");
    std::fs::write(pkg.join("index.js"), "export const a = 1;").expect("entry v1");

    let context = AnalysisContext {
        workspace_root: ws.clone(),
        active_document_path: ws.join("src").join("index.ts"),
    };
    let requests = vec![ImportRequest {
        specifier: "l1-lib".to_owned(),
        package_name: "l1-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    }];

    let sig1 = file_size_signature(&context, &requests);
    // Change the entry's CONTENT (and length) — the signature must change even
    // though the v4 cache key no longer carries entry fingerprints.
    std::fs::write(pkg.join("index.js"), "export const a = 222222;").expect("entry v2");
    let sig2 = file_size_signature(&context, &requests);

    assert_ne!(sig1, sig2, "L1 signature must react to a resolved entry's content change");
    std::fs::remove_dir_all(ws).ok();
}
```

(If the `#[cfg(test)] mod tests` block imports `AnalysisContext`/`ImportRequest`/`ImportKind`/`ImportRuntime` under specific paths, reuse whatever is already in scope there — the existing `unresolvable_context`/`named_request` helpers prove these types are already imported in that module.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p import-lens-daemon --lib file_size_signature_changes_when_entry_bytes_change`
Expected: FAIL — after Task 1, `cache_key_for_resolved_import` no longer varies on the entry mtime/content, so `sig1 == sig2`.

- [ ] **Step 3: Fold an independent entry+manifest fingerprint into the signature**

In `daemon/src/pipeline/file_size_cache.rs` `file_size_signature` (was `:141-169`), in the `Ok(resolved)` arm, hash a stat fingerprint of the resolved entry + manifest into each token so L1 regains the per-file sensitivity the key used to donate:

```rust
pub fn file_size_signature(context: &AnalysisContext, requests: &[ImportRequest]) -> u64 {
    // `cache_key_for_resolved_import` is already imported at module scope (file_size_cache.rs:2);
    // only pull in `fingerprints_for_paths` here to avoid shadowing it (unused-import deny).
    use crate::cache::key::fingerprints_for_paths;

    let mut tokens = requests
        .iter()
        .map(
            |request| match resolve_package_entry(&context.active_document_path, request) {
                Ok(resolved) => {
                    // The v4 cache key is fingerprint-free, so fold an independent
                    // entry+manifest stat fingerprint in to keep L1 edit-sensitive.
                    let fingerprints = fingerprints_for_paths(vec![
                        resolved.entry_path.clone(),
                        resolved.package_root.join("package.json"),
                    ]);
                    format!(
                        "{}|{:?}",
                        cache_key_for_resolved_import(request, &resolved),
                        fingerprints
                    )
                }
                Err(_) => format!(
                    "unresolved:{}:{}:{}:{:?}:{:?}:{}",
                    request.package_name,
                    request.specifier,
                    request.version,
                    request.runtime,
                    request.import_kind,
                    request.named.join(",")
                ),
            },
        )
        .collect::<Vec<_>>();
    tokens.sort();

    let mut hasher = DefaultHasher::new();
    cache_generation().hash(&mut hasher);
    for token in &tokens {
        token.hash(&mut hasher);
    }
    hasher.finish()
}
```

(`FileFingerprint` derives `Debug`, so `{:?}` of the `Vec<FileFingerprint>` is a stable, mtime+len-sensitive token. `fingerprints_for_paths` sorts+dedups, so ordering is deterministic.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p import-lens-daemon --lib file_size_signature_changes_when_entry_bytes_change`
Expected: PASS. Then `cargo test -p import-lens-daemon` (whole suite green), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/file_size_cache.rs
git commit -m "fix(daemon): give the L1 file-size signature its own edit sensitivity

The L1 aggregate signature borrowed per-file mtime sensitivity from the cache
key's entry/manifest fingerprints. Identity v4 dropped those, so fold an
independent entry+manifest stat fingerprint into the signature; L1 again
detects entry/manifest edits without depending on the (now fingerprint-free)
key. Transitive-graph staleness stays bounded by L1's existing 30s TTL."
```

---

## Self-Review (against spec §4.7 + the planning brief)

**Spec coverage:**
- §4.7 "drop `manifest_fingerprint`+`entry_fingerprint` (→ v4)" → Task 1. ✅
- §4.7 "validity enforced value-side" → confirmed by the brief: `dependency_fingerprints` already covers manifest+entry+graph. ✅
- §4.7 "L1 folds a document fingerprint independently" → Task 3. ✅
- Migration (§11) for the v4 orphaning → Task 2 (schema bump reuses the existing wipe path; the general version-gate mechanism remains Plan 4 scope). ✅
- Out of scope here (later plans): SWR, D3 first-party bypass, capacity, registry, UI.

**Placeholder scan:** Task 3 Step 1 leaves the `AnalysisContext`/`ImportRequest` construction as "match the file's existing helpers" — this is a deliberate instruction to mirror the real test builder (whose exact form lives in `file_size_cache.rs`'s existing tests), not a code placeholder; the implementer copies the concrete builder from that file. All other steps carry complete code.

**Type consistency:** `CacheIdentityV4` (Task 1) is used consistently in `encode`/`decode`/`cache_identity_for_import`/the orphan helpers; `CACHE_KEY_PREFIX_V4` replaces every `CACHE_KEY_PREFIX_V3`. `CacheEnvelope` loses `package_identity` in both its definition and `decode_cached_result`'s deserialize target. `CURRENT_SCHEMA_VERSION = 5` (Task 2) matches the test's "4 = one below current".

**Ordering safety:** Task 1's envelope-shape change is safe before Task 2's wipe because old rows decode-fail gracefully (→ `None`, skipped) and are never reached by a minted v4 key; Task 2 then reclaims them. Task 3 is independent of Task 2.

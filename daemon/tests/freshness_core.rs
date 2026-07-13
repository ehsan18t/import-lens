use import_lens_daemon::cache::disk::DiskCache;
use import_lens_daemon::cache::key::{FileFingerprint, fingerprints_for_paths};
use import_lens_daemon::cache::memory::{
    CachedImport, ImportCache, bump_cache_generation, cache_generation,
};
use import_lens_daemon::ipc::protocol::ImportRuntime;
use std::fs;
use std::sync::{Arc, Mutex, atomic::AtomicU64};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("il-fresh-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

// `CACHE_GENERATION` (memory.rs) is one process-wide static, and cargo runs the
// `#[test]` fns within this binary on multiple threads by default. A test whose
// correctness depends on the generation NOT changing across its own short
// insert-then-get window (only `fresh_insert_serves_on_fast_path_within_ttl`
// below) would otherwise race against sibling tests in this same file that call
// `bump_cache_generation()`. Those sibling tests only ever depend on the
// generation having moved (any bump, by them or a concurrent test, still
// satisfies their assertions), so serializing just the three of them against
// each other — not against the disk-hydration or content-hash tests, which
// never touch the generation counter — is sufficient to make this
// deterministic without slowing down the rest of the file.
static GENERATION_RACE_GUARD: Mutex<()> = Mutex::new(());

fn lock_generation_race_guard() -> std::sync::MutexGuard<'static, ()> {
    GENERATION_RACE_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sample_result(specifier: &str) -> import_lens_daemon::ipc::protocol::ImportResult {
    // Mirror the helper in tests/memory_cache.rs.
    use import_lens_daemon::ipc::protocol::{ImportResult, MeasuredSizes};
    let mut result = ImportResult::measured(
        specifier,
        MeasuredSizes {
            raw_bytes: 10,
            minified_bytes: 8,
            gzip_bytes: 6,
            brotli_bytes: 5,
            zstd_bytes: 5,
        },
    );
    result.truly_treeshakeable = true;
    result
}

fn cached_import(
    result: import_lens_daemon::ipc::protocol::ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
) -> CachedImport {
    // The runtime verification fields are not serialized into the disk envelope
    // (decode_cached_result reconstructs them), so their values here are inert.
    CachedImport {
        result,
        dependency_fingerprints,
        verified_generation: 0,
        verified_at: None,
        first_party: false,
        last_seq: Arc::new(AtomicU64::new(0)),
        persisted_seq: Arc::new(AtomicU64::new(0)),
    }
}

#[test]
fn insert_at_captured_generation_does_not_serve_stale_after_bump() {
    let _serialize = lock_generation_race_guard();
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

    cache.insert_with_fingerprints_at_generation(
        "v3:d4".to_owned(),
        sample_result("dep"),
        fp_v1,
        captured,
    );

    // The entry was stamped with the OLD generation, so get() must re-verify and
    // (because the file changed) must NOT serve the stale v1 result.
    assert!(
        cache.get("v3:d4").is_none(),
        "captured-generation insert must not launder a stale result as fresh"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn get_evicts_on_changed_or_missing_but_keeps_on_fresh() {
    let _serialize = lock_generation_race_guard();
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

// The two tests below exercise `disk.rs`'s tri-state `get_entry` match directly:
// `get_evicts_on_changed_or_missing_but_keeps_on_fresh` above uses a
// disk-DISABLED cache (`ImportCache::new(None, false)`), so it never touches
// `DiskCache::get_entry` / `pending_insert_entry` at all. These use a
// disk-ENABLED cache and force the lookup through a *cold* disk hit — dropping
// and recreating the `ImportCache` from the same storage path with a 0 recent
// -preload limit, the same idiom `cache_disk.rs`'s
// `disk_cache_lazy_hit_populates_memory_cache` uses — so `ImportCache::get`'s
// disk-fallback branch, and therefore `disk.rs`'s own Stale/Gone eviction
// match, actually runs. The `Unknown` arm has no portable Windows repro (no
// reliable way to force a non-`NotFound` stat error from a test), so it is
// intentionally not covered here.

#[test]
fn disk_hydrated_entry_evicts_when_dependency_content_changes() {
    let storage_dir = temp_dir("disk-tristate-stale-storage");
    let dep_dir = temp_dir("disk-tristate-stale-dep");
    let dep = dep_dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);
    let key = "v3:disk-stale".to_owned();

    {
        let cache = ImportCache::new(Some(storage_dir.clone()), true);
        cache.insert_with_fingerprints(key.clone(), sample_result("dep"), fp);
        // Drop flushes the queued disk insert (DiskCache::Drop drains
        // pending_inserts), so the entry is actually persisted before reopen.
    }

    // Change the dependency's content (different LENGTH so this is not
    // mtime-flaky) before the cache reopens, so the disk layer's fingerprint
    // check on the cold hit below observes a real content change (Stale).
    fs::write(&dep, "export const x = 222;").expect("change dep");

    // recent_preload_limit: 0 forces memory_len() == 0 on startup, so `get` must
    // fall through to the disk-fallback branch in memory.rs, which calls
    // DiskCache::get_with_freshness → get_entry.
    let cache = ImportCache::new_with_recent_preload_limit(Some(storage_dir.clone()), true, 0);
    assert_eq!(cache.memory_len(), 0);
    assert!(
        cache.get(&key).is_none(),
        "disk-hydrated entry with a changed (Stale) dependency must evict, not serve"
    );

    drop(cache);
    fs::remove_dir_all(&storage_dir).ok();
    fs::remove_dir_all(&dep_dir).ok();
}

#[test]
fn disk_hydrated_entry_evicts_when_dependency_is_deleted() {
    let storage_dir = temp_dir("disk-tristate-gone-storage");
    let dep_dir = temp_dir("disk-tristate-gone-dep");
    let dep = dep_dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);
    let key = "v3:disk-gone".to_owned();

    {
        let cache = ImportCache::new(Some(storage_dir.clone()), true);
        cache.insert_with_fingerprints(key.clone(), sample_result("dep"), fp);
    }

    fs::remove_file(&dep).expect("delete dep");

    let cache = ImportCache::new_with_recent_preload_limit(Some(storage_dir.clone()), true, 0);
    assert_eq!(cache.memory_len(), 0);
    assert!(
        cache.get(&key).is_none(),
        "disk-hydrated entry whose dependency is Gone must evict, not serve"
    );

    drop(cache);
    fs::remove_dir_all(&storage_dir).ok();
    fs::remove_dir_all(&dep_dir).ok();
}

#[test]
fn fresh_insert_serves_on_fast_path_within_ttl() {
    let _serialize = lock_generation_race_guard();
    let dir = temp_dir("ttl");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints("v3:ttl".to_owned(), sample_result("dep"), fp);

    // Same generation + within TTL → fast path skips the re-stat, so deleting the
    // dep out of band still serves (this is the intended TTL behavior).
    fs::remove_file(&dep).expect("rm");
    assert!(
        cache.get("v3:ttl").is_some(),
        "fast path within TTL serves without re-stat"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn first_party_entry_is_reverified_on_get_within_ttl() {
    // D3: a first-party dep (entry outside node_modules) changes without a
    // NodeModulesChanged generation bump, so it must bypass the TTL fast path and be
    // re-validated on every get — unlike the node_modules case above.
    use import_lens_daemon::cache::key::{cache_key_for_resolved_import, cache_key_is_first_party};
    use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest};
    use import_lens_daemon::pipeline::resolver::{ResolvedPackage, SideEffectsMode};

    let _serialize = lock_generation_race_guard();
    let dir = temp_dir("d3-firstparty");
    let pkg_root = dir.join("packages").join("ui");
    fs::create_dir_all(&pkg_root).expect("pkg dir");
    let entry = pkg_root.join("index.ts");
    let dep = pkg_root.join("dep.ts");
    fs::write(&entry, "export { v } from './dep';").expect("entry");
    fs::write(&dep, "export const v = 1;").expect("dep v1");

    let request = ImportRequest {
        specifier: "ui".to_owned(),
        package_name: "ui".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };
    let resolved = ResolvedPackage {
        package_root: pkg_root.clone(),
        package_json: serde_json::json!({ "version": "1.0.0" }),
        entry_path: entry.clone(),
        is_cjs: false,
        side_effects: SideEffectsMode::Missing,
    };
    let key = cache_key_for_resolved_import(&request, &resolved);
    assert!(
        cache_key_is_first_party(&key),
        "an entry outside node_modules is first-party"
    );

    let cache = ImportCache::new(None, false);
    // Stamps the current generation + verified_at=now, so a NON-first-party key would
    // serve on the fast path within TTL. The dep fingerprint is stat-only (no hash),
    // and the change below uses a different length for deterministic detection.
    cache.insert_with_fingerprints(
        key.clone(),
        sample_result("ui"),
        fingerprints_for_paths(vec![dep.clone()]),
    );
    fs::write(&dep, "export const v = 22222;").expect("dep v2");

    assert!(
        cache.get(&key).is_none(),
        "first-party dep change must be re-verified on get (fast-path bypass), not served stale"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn get_with_result_freshness_serves_stale_without_evicting_and_dedupes() {
    use import_lens_daemon::ipc::protocol::{FreshnessKind, ResultFreshness};

    let _serialize = lock_generation_race_guard();
    let dir = temp_dir("swr-stale");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep v1");

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints(
        "v4:swr".to_owned(),
        sample_result("dep"),
        fingerprints_for_paths(vec![dep.clone()]),
    );

    // Change the dep (different length) and force the slow path.
    fs::write(&dep, "export const x = 22222;").expect("dep v2");
    bump_cache_generation();

    // Serve-stale: the last value comes back flagged Stale, and the entry is NOT
    // evicted — a second call still serves it stale.
    let (value, freshness) = cache
        .get_with_result_freshness("v4:swr")
        .expect("serves stale");
    assert_eq!(freshness.kind, FreshnessKind::Stale);
    assert!(freshness.revalidating);
    assert_eq!(value.freshness, ResultFreshness::stale(true));

    let (_v2, freshness2) = cache
        .get_with_result_freshness("v4:swr")
        .expect("still serves stale (not evicted)");
    assert_eq!(freshness2.kind, FreshnessKind::Stale);

    // In-flight dedupe: the first caller claims the revalidation (holding the guard),
    // a concurrent caller is deduped, and dropping the guard releases the claim.
    let guard = cache.begin_revalidation("v4:swr");
    assert!(guard.is_some(), "first caller claims the revalidation");
    assert!(
        cache.begin_revalidation("v4:swr").is_none(),
        "a second caller is deduped while a revalidation is in flight"
    );
    drop(guard);
    assert!(
        cache.begin_revalidation("v4:swr").is_some(),
        "dropping the guard releases the claim for a future revalidation"
    );

    // A Gone dep (deleted) still evicts and is never served stale.
    fs::remove_file(&dep).expect("rm");
    bump_cache_generation();
    assert!(
        cache.get_with_result_freshness("v4:swr").is_none(),
        "a Gone (deleted) dep evicts and returns None, never served stale"
    );

    fs::remove_dir_all(dir).ok();
}

// The two tests below exercise `disk.rs`'s OTHER tri-state match — the one in
// `pending_insert_entry`, the read-your-writes path for an insert still queued
// and not yet flushed to redb. They deliberately drive `DiskCache` directly
// rather than through `ImportCache`: a queued disk insert is simultaneously
// mirrored into `ImportCache`'s in-memory map, which `ImportCache::get` consults
// first, so the memory layer always answers before the disk pending path can
// run — there is no `ImportCache` call sequence that reaches
// `pending_insert_entry`. Against `DiskCache` a single insert stays queued
// (`INSERT_FLUSH_BATCH` is 64) and is never flushed, so `get` must resolve it
// through `pending_insert_entry` and hit that method's Stale/Gone eviction arm.

#[test]
fn pending_unflushed_disk_insert_evicts_when_dependency_content_changes() {
    let storage_dir = temp_dir("disk-pending-stale-storage");
    let dep_dir = temp_dir("disk-pending-stale-dep");
    let dep = dep_dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);
    let key = "v3:disk-pending-stale";

    let disk = DiskCache::new(Some(storage_dir.clone()), true);
    disk.insert(key, &cached_import(sample_result("dep"), fp))
        .expect("queue insert");

    // Still queued (unflushed): read-your-writes resolves via pending_insert_entry.
    assert!(
        disk.get(key).is_some(),
        "a fresh queued insert should resolve through the pending path"
    );

    // Change the dependency (different LENGTH so this is not mtime-flaky) → the
    // next pending-path lookup must classify Stale and evict.
    fs::write(&dep, "export const x = 222;").expect("change dep");
    assert!(
        disk.get(key).is_none(),
        "queued (unflushed) entry with a Stale dependency must evict, not serve"
    );

    drop(disk);
    fs::remove_dir_all(&storage_dir).ok();
    fs::remove_dir_all(&dep_dir).ok();
}

#[test]
fn pending_unflushed_disk_insert_evicts_when_dependency_is_deleted() {
    let storage_dir = temp_dir("disk-pending-gone-storage");
    let dep_dir = temp_dir("disk-pending-gone-dep");
    let dep = dep_dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep");
    let fp = fingerprints_for_paths(vec![dep.clone()]);
    let key = "v3:disk-pending-gone";

    let disk = DiskCache::new(Some(storage_dir.clone()), true);
    disk.insert(key, &cached_import(sample_result("dep"), fp))
        .expect("queue insert");

    assert!(
        disk.get(key).is_some(),
        "a fresh queued insert should resolve through the pending path"
    );

    fs::remove_file(&dep).expect("delete dep");
    assert!(
        disk.get(key).is_none(),
        "queued (unflushed) entry whose dependency is Gone must evict, not serve"
    );

    drop(disk);
    fs::remove_dir_all(&storage_dir).ok();
    fs::remove_dir_all(&dep_dir).ok();
}

#[test]
fn swr_read_hydrates_fresh_from_disk_but_never_serves_disk_only_stale() {
    let dir = temp_dir("swr-hydrate");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const x = 1;").expect("dep should be written");
    let storage = dir.join("cache");
    let key = "react@18.3.1::default";

    {
        let cache = ImportCache::new(Some(storage.clone()), true);
        cache.insert_with_fingerprints(
            key.to_owned(),
            sample_result("dep"),
            fingerprints_for_paths(vec![dep.clone()]),
        );
        cache.flush_to_disk().expect("flush should succeed");
    }

    // Fresh on disk, absent from memory (preload 0): the SWR read must hydrate
    // from disk and serve it Fresh — the cold-start path of the layered cache.
    let cold = ImportCache::new_with_recent_preload_limit(Some(storage.clone()), true, 0);
    let (result, freshness) = cold
        .get_with_result_freshness(key)
        .expect("a fresh disk entry must hydrate through the SWR read");
    assert!(freshness.is_fresh());
    assert!(result.cache_hit);
    drop(cold);

    // Change the dep: the disk copy is now stale. Layered contract, pinned so a
    // future disk-layer change cannot silently alter it: a disk-ONLY stale
    // entry is NOT served stale (the disk layer evicts Stale on read) — SWR
    // falls through to a synchronous recompute. Serve-stale applies to the
    // memory working set only.
    fs::write(
        &dep,
        "export const x = 'changed to substantially longer bytes';",
    )
    .expect("dep should be rewritten");
    let cold = ImportCache::new_with_recent_preload_limit(Some(storage.clone()), true, 0);
    assert!(
        cold.get_with_result_freshness(key).is_none(),
        "a disk-only stale entry must fall through to recompute, never serve stale"
    );
    drop(cold);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn probe_freshness_reports_unknown_distinctly_so_the_reprobe_gate_never_recomputes_it() {
    // §4.3.1: the background SWR revalidation must recompute a genuine content-`Stale`
    // dependency but NEVER a transient `Unknown` one — recomputing an Unknown would
    // re-hit the same stat/read error and could overwrite the good cached value with an
    // error result. `probe_freshness` is the raw 4-state re-probe the gate rests on: it
    // must report `Unknown` DISTINCTLY from `Stale` and re-stat on every call (no TTL
    // fast path). Deterministic `Unknown` with no mock: a fingerprint whose path is a
    // DIRECTORY (content hash set, len/mtime deliberately mismatched so the cheap
    // pre-filter always falls through) drives `check_fingerprint` into its `fs::read`
    // branch, which fails on a directory with a NON-`NotFound` error → `Unknown`.
    use import_lens_daemon::cache::key::{Freshness, content_hash};

    let dir = temp_dir("probe-freshness");
    let probe = dir.join("dep-probe");
    fs::create_dir_all(&probe).expect("probe starts as a directory → Unknown");

    let fresh_bytes: &[u8] = b"export const x = 1;";
    let fingerprint = FileFingerprint {
        path: probe.to_string_lossy().into_owned(),
        // Deliberately-wrong len/mtime: the mtime+len pre-filter never matches, so every
        // probe falls through to the content-hash `fs::read` that yields Unknown on a
        // directory and Fresh/Stale on a file.
        len: 999_999,
        modified_millis: 1,
        content_hash: Some(content_hash(fresh_bytes)),
    };

    let cache = ImportCache::new(None, false);
    let key = "v4:probe-freshness-test".to_owned();
    cache.insert_with_fingerprints(key.clone(), sample_result("dep"), vec![fingerprint]);

    // Directory → transient stat/read error → Unknown: the value the gate SKIPS on
    // (never routed into recompute). Re-probes ignore the TTL fast path, so no bump.
    assert_eq!(
        cache.probe_freshness(&key),
        Some(Freshness::Unknown),
        "a transient error must re-probe as Unknown, distinct from Stale"
    );

    // Matching file → Fresh (the transient cleared).
    fs::remove_dir(&probe).expect("remove probe directory");
    fs::write(&probe, fresh_bytes).expect("write matching file");
    assert_eq!(cache.probe_freshness(&key), Some(Freshness::Fresh));

    // Changed content (hash mismatch) → genuine Stale: the value the gate RECOMPUTES on.
    fs::write(&probe, b"export const x = 22222;").expect("rewrite changed content");
    assert_eq!(
        cache.probe_freshness(&key),
        Some(Freshness::Stale),
        "a genuine content change must re-probe as Stale, so recompute still fires"
    );

    // A key absent from the memory working set → None (the gate does not skip; recompute
    // proceeds exactly as before the gate existed).
    assert_eq!(cache.probe_freshness("v4:not-present"), None);

    fs::remove_dir_all(dir).ok();
}

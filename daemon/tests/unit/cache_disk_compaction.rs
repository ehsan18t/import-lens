//! Compaction tests. In-crate because the idle gate is wall-clock based: `mark_idle_for_test` is
//! the only deterministic way to make a just-written shard read as idle.

use super::DiskCache;
use crate::{
    cache::memory::CachedImport,
    ipc::protocol::{ConfidenceLevel, ImportDiagnostic, ImportResult, MeasuredSizes},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

fn temp_storage() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "il-compaction-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn db_path(storage_path: &Path) -> PathBuf {
    storage_path.join(super::CACHE_DB_FILE_NAME)
}

fn cached(specifier: &str) -> CachedImport {
    let mut result = ImportResult::measured(
        specifier,
        MeasuredSizes {
            raw_bytes: 10,
            minified_bytes: 8,
            gzip_bytes: 7,
            brotli_bytes: 6,
            zstd_bytes: 5,
        },
    );
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons = vec!["test fixture confidence".to_owned()];
    result.diagnostics = vec![ImportDiagnostic {
        stage: crate::engine::diagnostic_stage::EXTERNAL.to_owned(),
        message: "cached".to_owned(),
        details: Vec::new(),
    }];
    CachedImport {
        result,
        dependency_fingerprints: Vec::new(),
        verified_generation: 0,
        verified_at: None,
        first_party: false,
        last_seq: Arc::new(AtomicU64::new(1)),
        persisted_seq: Arc::new(AtomicU64::new(1)),
    }
}

#[test]
fn compaction_shrinks_the_file_after_heavy_eviction() {
    use super::COMPACT_THRESHOLD;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Insert a batch, flush to disk, then evict almost all of it so most of the
    // file becomes reclaimable free space.
    let disk = DiskCache::new(Some(storage.clone()), true);
    let mut all_keys = Vec::new();
    for index in 0..1000 {
        let key = format!("pkg{index}@1.0.0::default");
        let mut entry = cached(&key);
        entry.last_seq = Arc::new(AtomicU64::new(index as u64 + 1));
        disk.insert(&key, &entry).expect("insert should queue");
        all_keys.push(key);
    }
    disk.flush_pending_inserts();

    let size_before = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Evict 950 of 1000 entries.
    let freed = disk.remove_keys(&all_keys[..950]);
    assert!(freed > 0, "eviction should free bytes");

    // redb reuses freed pages rather than shrinking, so the file is still large.
    let size_after_evict = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Compaction reclaims the free pages and shrinks the file — but only once the
    // shard is idle (the fill/evict above just touched it), so mark it idle first.
    disk.mark_idle_for_test();
    let compacted = disk.compact_if_fragmented(COMPACT_THRESHOLD);
    assert!(
        compacted,
        "a mostly-empty file must exceed the fragmentation threshold and compact"
    );

    let size_after_compact = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();
    assert!(
        size_after_compact < size_after_evict,
        "compaction must shrink the file: {size_after_compact} >= {size_after_evict}"
    );
    // Sanity: it is smaller than the fully-populated file too.
    assert!(size_after_compact < size_before);

    // Compaction must shrink the file WITHOUT losing surviving data: every
    // non-evicted entry must still decode and serve.
    for key in &all_keys[950..] {
        assert!(
            disk.get_with_freshness(key)
                .map(|(cached, _)| cached)
                .is_some(),
            "entry {key} must survive compaction intact"
        );
    }

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn compaction_is_gated_on_shard_idleness() {
    use super::COMPACT_THRESHOLD;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Fragment the shard: fill, flush, then evict almost all of it so most of
    // the file is reclaimable free space (crosses COMPACT_THRESHOLD).
    let disk = DiskCache::new(Some(storage.clone()), true);
    let mut all_keys = Vec::new();
    for index in 0..1000 {
        let key = format!("pkg{index}@1.0.0::default");
        let mut entry = cached(&key);
        entry.last_seq = Arc::new(AtomicU64::new(index as u64 + 1));
        disk.insert(&key, &entry).expect("insert should queue");
        all_keys.push(key);
    }
    disk.flush_pending_inserts();
    let freed = disk.remove_keys(&all_keys[..950]);
    assert!(freed > 0, "eviction should free bytes");

    // A surviving get marks the shard as just-accessed — the user is actively
    // analyzing it. A fragmented BUT actively-used shard must NOT be compacted:
    // Database::compact holds the exclusive lock across the whole rewrite, which
    // would block the user's concurrent gets.
    assert!(
        disk.get_with_freshness(&all_keys[950])
            .map(|(cached, _)| cached)
            .is_some()
    );
    assert!(
        !disk.compact_if_fragmented(COMPACT_THRESHOLD),
        "a fragmented but recently-accessed shard must not be compacted"
    );
    let size_while_busy = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Once the shard goes idle (no get/insert within COMPACT_IDLE), the same
    // fragmented shard IS compacted and the file shrinks.
    disk.mark_idle_for_test();
    assert!(
        disk.compact_if_fragmented(COMPACT_THRESHOLD),
        "an idle fragmented shard must be compacted"
    );
    let size_after_compact = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();
    assert!(
        size_after_compact < size_while_busy,
        "compaction must shrink the idle shard: {size_after_compact} >= {size_while_busy}"
    );

    // Compaction must not drop surviving data.
    for key in &all_keys[950..] {
        assert!(
            disk.get_with_freshness(key)
                .map(|(cached, _)| cached)
                .is_some(),
            "entry {key} must survive compaction intact"
        );
    }

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn stale_disk_get_does_not_deadlock_with_concurrent_compaction() {
    use crate::cache::key::file_fingerprint_with_hash;
    use std::sync::mpsc;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let dep = storage.join("dep.js");
    let key = "react@18.3.1::default";

    // A getter thread repeatedly lands on the Stale eviction path inside
    // `get_entry` (db read guard → decode → remove) while a compactor thread
    // hammers the exclusive write lock. Before the guard-scoping fix, the
    // re-entrant `remove` under a held read guard deadlocked against the queued
    // compaction writer; the channel timeout below is the failure signal.
    let disk = Arc::new(DiskCache::new(Some(storage.clone()), true));

    let (done_tx, done_rx) = mpsc::channel();
    let getter = {
        let disk = Arc::clone(&disk);
        let dep = dep.clone();
        std::thread::spawn(move || {
            for round in 0..50 {
                // Fresh content each round, fingerprinted, then changed → Stale.
                fs::write(&dep, format!("export const v = {round};")).expect("dep write");
                let mut entry = cached("react");
                entry.dependency_fingerprints =
                    vec![file_fingerprint_with_hash(&dep, None).expect("stat the dependency")];
                disk.insert(key, &entry).expect("insert should queue");
                disk.flush_pending_inserts();
                fs::write(
                    &dep,
                    format!("export const v = 'changed {round} with longer bytes';"),
                )
                .expect("dep rewrite");
                // Stale → the re-entrant remove path.
                assert!(disk.get_with_freshness(key).is_none());
            }
            let _ = done_tx.send(());
        })
    };
    let compactor = {
        let disk = Arc::clone(&disk);
        std::thread::spawn(move || {
            for _ in 0..200 {
                // Force the idle gate open each iteration so this still drives the
                // exclusive compaction writer against the concurrent stale-get
                // remove path (threshold 0.0 → attempts the exclusive write lock).
                disk.mark_idle_for_test();
                let _ = disk.compact_if_fragmented(0.0);
            }
        })
    };

    assert!(
        done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
        "stale disk get deadlocked against a concurrent compaction"
    );
    getter.join().expect("getter thread");
    compactor.join().expect("compactor thread");

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

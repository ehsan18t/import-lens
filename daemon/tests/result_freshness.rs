use import_lens_daemon::cache::key::{FileFingerprint, content_hash};
use import_lens_daemon::cache::memory::{ImportCache, bump_cache_generation};
use import_lens_daemon::ipc::protocol::{FreshnessKind, ImportResult, ResultFreshness};

fn base_result() -> ImportResult {
    ImportResult {
        specifier: "lib".to_owned(),
        raw_bytes: 10,
        minified_bytes: 8,
        gzip_bytes: 6,
        brotli_bytes: 5,
        zstd_bytes: 5,
        cache_hit: false,
        side_effects: false,
        truly_treeshakeable: true,
        is_cjs: false,
        confidence: Default::default(),
        confidence_reasons: Vec::new(),
        error: None,
        diagnostics: Vec::new(),
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
        freshness: ResultFreshness::fresh(),
    }
}

#[test]
fn freshness_defaults_to_fresh_when_absent() {
    // An old-format payload (no `freshness` key) must decode to Fresh via
    // `#[serde(default)]` — the disk-compat guarantee.
    let json = r#"{
        "specifier":"lib","raw_bytes":10,"minified_bytes":8,"gzip_bytes":6,
        "brotli_bytes":5,"zstd_bytes":5,"cache_hit":false,"side_effects":false,
        "truly_treeshakeable":true,"is_cjs":false,"error":null,"diagnostics":[]
    }"#;
    let decoded: ImportResult = serde_json::from_str(json).expect("decode old-format");
    assert_eq!(decoded.freshness, ResultFreshness::fresh());
}

#[test]
fn fresh_result_roundtrips_through_disk_positional_msgpack() {
    // The DISK uses positional `rmp_serde::to_vec`. Freshness is a serve-time property,
    // so the disk only ever stores `Fresh`, which is skipped on serialize and decodes
    // back to the `Fresh` default — the round-trip that actually happens on disk.
    let result = base_result(); // freshness == Fresh
    let bytes = rmp_serde::to_vec(&result).expect("encode");
    let decoded: ImportResult = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(decoded.freshness, ResultFreshness::fresh());
    assert_eq!(decoded.specifier, "lib");
}

#[test]
fn stale_and_unverified_roundtrip_through_ipc_named_msgpack() {
    // Non-`Fresh` values travel only over the IPC path, which uses named
    // `rmp_serde::to_vec_named` (position-independent) — so the flag survives intact.
    for freshness in [
        ResultFreshness::stale(true),
        ResultFreshness::unverified("locked"),
    ] {
        let mut result = base_result();
        result.freshness = freshness.clone();
        let bytes = rmp_serde::to_vec_named(&result).expect("encode");
        let decoded: ImportResult = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded.freshness, freshness);
        assert_eq!(decoded.specifier, "lib");
    }
}

#[test]
fn freshness_json_shape_matches_extension_mirror() {
    // Flat struct: a snake_case `kind` discriminant plus `revalidating`, with `reason`
    // omitted when absent. The extension mirror matches this shape.
    let fresh = serde_json::to_value(ResultFreshness::fresh()).expect("json");
    assert_eq!(
        fresh,
        serde_json::json!({ "kind": "fresh", "revalidating": false })
    );
    let stale = serde_json::to_value(ResultFreshness::stale(true)).expect("json");
    assert_eq!(
        stale,
        serde_json::json!({ "kind": "stale", "revalidating": true })
    );
    let unverified = serde_json::to_value(ResultFreshness::unverified("locked")).expect("json");
    assert_eq!(
        unverified,
        serde_json::json!({ "kind": "unverified", "revalidating": false, "reason": "locked" })
    );
}

#[test]
fn unknown_freshness_graduates_quietly_then_surfaces_and_resets() {
    // §4.3.1: a transient stat/read error (`Freshness::Unknown` — e.g. a dependency
    // file locked for milliseconds by a save or an AV scan) must NOT immediately flash
    // an alarming `Unverified`. The first few Unknowns keep serving the last value
    // QUIETLY flagged `Stale { revalidating: true }`; only once the error PERSISTS past
    // `UNKNOWN_MAX_ATTEMPTS` is it surfaced as `Unverified`. A subsequent `Fresh` must
    // reset the window, so a later blip graduates from `Stale` again.
    //
    // Deterministic `Unknown` injection with no mocking of the graduation itself: a
    // fingerprint whose path is a DIRECTORY (with a content hash, and a deliberately
    // mismatched len/mtime so the cheap pre-filter always falls through) forces
    // `check_fingerprint` down its `fs::read` branch, which fails on a directory with a
    // NON-`NotFound` error (PermissionDenied on Windows, IsADirectory on Unix) →
    // `Freshness::Unknown`. Flipping that same path between a directory and a real file
    // holding the hashed bytes toggles the raw freshness between `Unknown` and `Fresh`,
    // exercising the real `get_with_result_freshness` graduation + reset end to end.
    let root = std::env::temp_dir().join(format!(
        "il-unknown-grad-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&root).expect("root dir");
    let probe = root.join("dep-probe");
    std::fs::create_dir_all(&probe).expect("probe starts as a directory → Unknown");

    // Bytes whose hash the fingerprint stores. When `probe` is later a FILE holding
    // exactly these bytes, the content-hash read matches → Fresh.
    let fresh_bytes: &[u8] = b"export const x = 1;";
    let fingerprint = FileFingerprint {
        path: probe.to_string_lossy().into_owned(),
        // Deliberately-wrong len/mtime: the mtime+len pre-filter never matches, so
        // every check falls through to the content-hash `fs::read` — the branch that
        // yields `Unknown` on a directory and `Fresh` on the matching file.
        len: 999_999,
        modified_millis: 1,
        content_hash: Some(content_hash(fresh_bytes)),
    };

    let cache = ImportCache::new(None, false);
    // A non-decodable key is NOT first-party → the cheap `check_fingerprints` path, and
    // its verified stamp lets a generation bump force the slow re-check path on each get.
    let key = "v4:unknown-graduation-test";
    cache.insert_with_fingerprints(key.to_owned(), base_result(), vec![fingerprint]);

    // Force the slow path (re-check) via a generation bump, then serve, returning the
    // freshness. Only this test touches `CACHE_GENERATION` in this test binary, so the
    // bump-then-get window cannot race a sibling.
    let serve = |cache: &ImportCache| -> ResultFreshness {
        bump_cache_generation();
        cache
            .get_with_result_freshness(key)
            .expect("an Unknown/Fresh entry is kept, never evicted")
            .1
    };

    // First Unknown → QUIET Stale{revalidating}, NOT Unverified.
    let first = serve(&cache);
    assert_eq!(
        first.kind,
        FreshnessKind::Stale,
        "the first transient Unknown must graduate to Stale, not flash Unverified"
    );
    assert!(
        first.revalidating,
        "a graduated Stale must be flagged revalidating (quiet recheck)"
    );

    // Still transient across the next attempts (UNKNOWN_MAX_ATTEMPTS == 3) → still Stale.
    assert_eq!(serve(&cache).kind, FreshnessKind::Stale);
    assert_eq!(serve(&cache).kind, FreshnessKind::Stale);

    // Past UNKNOWN_MAX_ATTEMPTS the persistent error surfaces as Unverified…
    let persisted = serve(&cache);
    assert_eq!(
        persisted.kind,
        FreshnessKind::Unverified,
        "a persistent Unknown must surface as Unverified once past the window"
    );
    assert!(
        persisted.reason.is_some(),
        "Unverified carries a reason string"
    );
    // …and stays Unverified while it persists.
    assert_eq!(serve(&cache).kind, FreshnessKind::Unverified);

    // A Fresh outcome resets the window: flip `probe` from a directory to a real file
    // whose bytes hash to the stored content hash.
    std::fs::remove_dir(&probe).expect("remove probe directory");
    std::fs::write(&probe, fresh_bytes).expect("write probe as a matching file");
    assert_eq!(
        serve(&cache).kind,
        FreshnessKind::Fresh,
        "a file matching the stored content hash reads Fresh"
    );

    // Flip back to a directory (Unknown again). Because the Fresh above reset the
    // window, the NEXT Unknown is a brand-new graduation → Stale again, NOT Unverified.
    std::fs::remove_file(&probe).expect("remove probe file");
    std::fs::create_dir(&probe).expect("recreate probe directory");
    let after_reset = serve(&cache);
    assert_eq!(
        after_reset.kind,
        FreshnessKind::Stale,
        "a Fresh must reset the window so the next Unknown graduates from Stale again"
    );
    assert!(after_reset.revalidating);

    std::fs::remove_dir_all(&root).ok();
}

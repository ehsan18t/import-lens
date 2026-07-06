use crate::{
    cache::key::{
        FileFingerprint, content_hash, file_fingerprint_from_read_time, path_is_definitely_gone,
        read_time_len_mtime,
    },
    ipc::protocol::{ImportDiagnostic, ImportRuntime, ModuleContribution},
    pipeline::{
        cjs_scan::scan_cjs_source,
        graph::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES},
        resolver::{normalize_existing_path, resolve_module_path, shared_resolvers},
        util::diagnostic,
    },
};
use oxc_resolver::Resolver;
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

/// Read-time fingerprints of a CommonJS require-graph's modules, keyed by the
/// canonical entry path + runtime. The CJS analyzer walks and reads every
/// transitively `require()`d module but produces no [`crate::pipeline::graph::ModuleGraph`],
/// so without this, L2 (`dependency_fingerprints`) and L1 (`first_party_module_token`)
/// degrade to manifest+entry and a first-party CJS dependency edit never invalidates
/// (RB-5). Mirrors `GRAPH_CACHE`: bounded with LRU eviction, and cleared / invalidated
/// / purged on the same seams (see the pub fns below, wired into `service.rs`).
static CJS_MODULE_CACHE: OnceLock<papaya::HashMap<(PathBuf, ImportRuntime), CachedCjsModules>> =
    OnceLock::new();

/// Bounds the CJS module cache, mirroring `MAX_CACHED_GRAPHS`. Each entry retains only
/// module paths + fingerprints (no source), so it is far lighter than a cached graph.
const MAX_CACHED_CJS_MODULE_SETS: usize = 64;

#[derive(Debug, Clone)]
struct CachedCjsModules {
    /// Canonical module paths, for L1's per-poll `stat_token` (no content read).
    module_paths: Vec<PathBuf>,
    /// Read-time fingerprints (len+mtime+content hash captured DURING the walk, before
    /// each read), for L2's content-hash-verified freshness.
    fingerprints: Vec<FileFingerprint>,
    last_used_millis: Arc<AtomicU64>,
}

/// Store a CJS walk's module set. `entry_path` MUST already be canonical (the analyzer
/// canonicalizes it before walking); the peek callers canonicalize their raw input to
/// match. Evicts the least-recently-used set when over the bound.
fn cache_cjs_module_set(
    entry_path: PathBuf,
    runtime: ImportRuntime,
    module_paths: Vec<PathBuf>,
    fingerprints: Vec<FileFingerprint>,
) {
    let cache = CJS_MODULE_CACHE.get_or_init(papaya::HashMap::new);
    let pinned = cache.pin();
    pinned.insert(
        (entry_path, runtime),
        CachedCjsModules {
            module_paths,
            fingerprints,
            last_used_millis: Arc::new(AtomicU64::new(crate::time::unix_millis_now())),
        },
    );
    if pinned.len() > MAX_CACHED_CJS_MODULE_SETS {
        let oldest = pinned
            .iter()
            .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            pinned.remove(&key);
        }
    }
}

/// L2: read-time fingerprints for a cached CJS module set, or `None` if nothing is
/// cached for `(entry, runtime)`. Promotes the set's recency (a real freshness
/// consumption). The entry is canonicalized to match the stored key.
pub fn cjs_module_fingerprints(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Option<Vec<FileFingerprint>> {
    let entry_path = normalize_existing_path(entry_path).ok()?;
    let cache = CJS_MODULE_CACHE.get()?;
    let pinned = cache.pin();
    let cached = pinned.get(&(entry_path, runtime))?;
    cached
        .last_used_millis
        .store(crate::time::unix_millis_now(), Ordering::Relaxed);
    Some(cached.fingerprints.clone())
}

/// L1: canonical module paths for a cached CJS module set (no freshness gate — L1
/// re-stats each path itself), or `None` if nothing is cached. Leaves recency
/// untouched (a cheap signature peek is not a real consumption), mirroring
/// [`crate::pipeline::graph::peek_cached_module_paths`].
pub fn peek_cjs_module_paths(entry_path: &Path, runtime: ImportRuntime) -> Option<Vec<PathBuf>> {
    let entry_path = normalize_existing_path(entry_path).ok()?;
    let cache = CJS_MODULE_CACHE.get()?;
    let pinned = cache.pin();
    let cached = pinned.get(&(entry_path, runtime))?;
    Some(cached.module_paths.clone())
}

/// Drop every cached CJS module set (cache-clear / invalidate-all / orphan sweep).
pub fn clear_cjs_module_cache() {
    if let Some(cache) = CJS_MODULE_CACHE.get() {
        cache.pin().clear();
    }
}

/// Drop cached CJS module sets whose entry is inside `node_modules/<package>/` (a
/// reinstall changed its bytes without an mtime the pre-filter trusts). Mirrors
/// [`crate::pipeline::graph::invalidate_module_graph_cache_for_package`], which keys
/// off the entry path; a first-party entry that merely *requires* a node_modules
/// package is re-validated by the `cache_generation` bump that accompanies the
/// invalidation, so it needs no per-key drop here.
pub fn invalidate_cjs_module_cache_for_package(package_name: &str) {
    let Some(cache) = CJS_MODULE_CACHE.get() else {
        return;
    };
    let package_segment = format!("node_modules/{package_name}/");
    let pinned = cache.pin();
    let keys = pinned
        .iter()
        .filter(|((entry_path, _runtime), _)| {
            entry_path
                .to_string_lossy()
                .replace('\\', "/")
                .contains(&package_segment)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in keys {
        pinned.remove(&key);
    }
}

/// Drop cached CJS module sets whose entry-path key no longer exists on disk
/// (uninstalled/moved package). Used by the orphan purge; drive-safe (a transient
/// stat error keeps the entry, X-3). Returns the number removed.
pub fn purge_missing_cjs_module_sets() -> usize {
    let Some(cache) = CJS_MODULE_CACHE.get() else {
        return 0;
    };
    let pinned = cache.pin();
    let missing = pinned
        .iter()
        .filter(|((entry_path, _runtime), _)| path_is_definitely_gone(entry_path))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in &missing {
        pinned.remove(key);
    }
    missing.len()
}

#[derive(Debug, Default)]
pub struct CjsGraphAnalysis {
    pub source: String,
    pub module_breakdown: Vec<ModuleContribution>,
    pub full_module_breakdown: Vec<ModuleContribution>,
    pub exports: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub unsupported: bool,
}

pub fn analyze_cjs_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<CjsGraphAnalysis, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    // Retained for the CJS module cache key (RB-5); `entry_path` itself is moved into
    // the walk queue below.
    let cache_key_entry = entry_path.clone();
    let mut queue = VecDeque::from([entry_path]);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    let mut module_breakdown = Vec::new();
    // Canonical module paths + read-time fingerprints captured during the walk, cached
    // so L1/L2 can fingerprint first-party CJS deps (RB-5).
    let mut module_paths = Vec::new();
    let mut module_fingerprints = Vec::new();
    let mut exports = Vec::new();
    let mut diagnostics = Vec::new();
    let mut total_source_bytes = 0_usize;
    let mut unsupported = false;
    let resolvers = shared_resolvers();
    let resolver = resolvers.resolver(runtime);

    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if seen.len() > MAX_GRAPH_MODULES {
            return Err(format!(
                "CommonJS module count limit exceeded while loading {}; limit: {}",
                path.display(),
                MAX_GRAPH_MODULES
            ));
        }

        // Capture len+mtime BEFORE the read (never after): a change landing between the
        // stat and the read then mismatches the stored len/mtime, so the freshness check
        // falls through to the content hash instead of pairing fresh len/mtime with a
        // stale hash — the same discipline `ModuleRecord` uses for the ESM graph.
        let (content_len, content_mtime_millis) = read_time_len_mtime(&path);
        let source = fs::read_to_string(&path).map_err(|error| {
            format!("failed to read CommonJS module {}: {error}", path.display())
        })?;
        let source_bytes = source.len();
        if source_bytes > MAX_MODULE_SOURCE_BYTES {
            return Err(format!(
                "CommonJS module source size {} exceeds limit {} in {}",
                source_bytes,
                MAX_MODULE_SOURCE_BYTES,
                path.display()
            ));
        }
        total_source_bytes = total_source_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| format!("CommonJS graph source size overflow in {}", path.display()))?;
        if total_source_bytes > MAX_GRAPH_SOURCE_BYTES {
            return Err(format!(
                "CommonJS graph source size {} exceeds limit {} while loading {}",
                total_source_bytes,
                MAX_GRAPH_SOURCE_BYTES,
                path.display()
            ));
        }

        let scan = scan_cjs_source(&source);
        unsupported |= scan.unsupported;
        for specifier in scan.requires {
            match resolve_require(resolver, &path, &specifier) {
                Ok(Some(resolved_path)) => queue.push_back(resolved_path),
                Ok(None) => diagnostics.push(diagnostic(
                    "cjs_resolution",
                    format!("CommonJS require '{specifier}' was kept external"),
                    vec![format!("from_path: {}", path.display())],
                )),
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "cjs_resolution",
                        error,
                        vec![format!("from_path: {}", path.display())],
                    ));
                    unsupported = true;
                }
            }
        }

        if seen.len() == 1 {
            exports.extend(scan.exports);
        }

        module_breakdown.push(ModuleContribution {
            path: path.to_string_lossy().to_string(),
            bytes: source_bytes as u64,
        });
        // `path` is canonical (every queued path came through `normalize_existing_path`),
        // so `file_fingerprint_from_read_time` may skip re-canonicalizing. The hash is of
        // the exact bytes read: `read_to_string` succeeded, so `source.as_bytes()` equals
        // the raw file bytes `check_fingerprint_strict` will later re-read and compare.
        module_fingerprints.push(file_fingerprint_from_read_time(
            &path,
            content_len,
            content_mtime_millis,
            content_hash(source.as_bytes()),
        ));
        module_paths.push(path.clone());
        sources.push(format!(";(() => {{\n{source}\n}})();"));
    }

    exports.sort();
    exports.dedup();
    module_breakdown.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let full_module_breakdown = module_breakdown.clone();
    module_breakdown.truncate(10);

    // Cache the module set keyed by the canonical entry so L1/L2 can fingerprint the
    // first-party CJS deps (RB-5). Populated for every completed walk, including one
    // that the caller later rejects to static-entry sizing (unsupported dynamic require
    // / no export shape): the cached set is then unused or yields harmless
    // over-coverage — never a stale-serve.
    cache_cjs_module_set(cache_key_entry, runtime, module_paths, module_fingerprints);

    Ok(CjsGraphAnalysis {
        source: sources.join("\n"),
        module_breakdown,
        full_module_breakdown,
        exports,
        diagnostics,
        unsupported,
    })
}

fn resolve_require(
    resolver: &Resolver,
    from_path: &Path,
    specifier: &str,
) -> Result<Option<PathBuf>, String> {
    if !specifier.starts_with('.') {
        return Ok(None);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "CommonJS module path has no parent directory: {}",
            from_path.display()
        )
    })?;
    resolve_module_path(resolver, from_dir, specifier)
        .map(|resolved| Some(resolved.path))
        .map_err(|error| {
            format!(
                "failed to resolve CommonJS require '{specifier}' from {}: {error}",
                from_path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::key::{Freshness, check_fingerprints_strict};

    /// `CJS_MODULE_CACHE` is a process-global shared by every test here; the `clear` /
    /// `purge` cases mutate it wholesale, so the four serialize on this lock to avoid
    /// one wiping another's set mid-flow. `into_inner` ignores a prior test's panic
    /// poison so a single failure does not cascade into misleading lock panics.
    static CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_cache() -> std::sync::MutexGuard<'static, ()> {
        CACHE_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// entry.js → require('./a.js') → require('./b.js'). Returns (root, canonical entry).
    fn write_cjs_chain(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "il-cjs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).expect("fixture dir");
        std::fs::write(
            root.join("entry.js"),
            "const a = require('./a.js');\nmodule.exports = { a };\n",
        )
        .expect("entry");
        std::fs::write(
            root.join("a.js"),
            "const b = require('./b.js');\nmodule.exports = b;\n",
        )
        .expect("a");
        std::fs::write(root.join("b.js"), "module.exports = 42;\n").expect("b");
        let entry = std::fs::canonicalize(root.join("entry.js")).expect("canonical entry");
        (root, entry)
    }

    #[test]
    fn cjs_walk_caches_transitive_module_set_for_l1_and_l2() {
        // RB-5: the CJS analyzer walks + reads every transitive require, but before the
        // fix discarded the set — so a deep first-party CJS dep was unfingerprinted in
        // BOTH L2 (dependency_fingerprints) and L1 (first_party_module_token). It now
        // caches the module set keyed by the canonical entry.
        let _guard = lock_cache();
        let (root, entry) = write_cjs_chain("cache");
        let runtime = ImportRuntime::Component;
        analyze_cjs_graph_with_runtime(&entry, runtime).expect("cjs walk");

        // L1: every transitively-required module path is exposed (entry + a + b).
        let paths = peek_cjs_module_paths(&entry, runtime).expect("cached module paths");
        assert_eq!(
            paths.len(),
            3,
            "entry + a.js + b.js are all cached: {paths:?}"
        );
        let b_path = std::fs::canonicalize(root.join("b.js")).expect("canonical b");
        assert!(
            paths.iter().any(|p| p == &b_path),
            "the deep transitive require b.js is in the L1 set: {paths:?}"
        );

        // L2: every module carries a read-time content hash (so the strict gate can
        // hash-verify a first-party CJS edit).
        let fingerprints = cjs_module_fingerprints(&entry, runtime).expect("cached fingerprints");
        assert_eq!(fingerprints.len(), 3, "one fingerprint per module");
        assert!(
            fingerprints.iter().all(|fp| fp.content_hash.is_some()),
            "every CJS module fingerprint carries a read-time content hash"
        );
        assert_eq!(
            check_fingerprints_strict(&fingerprints),
            Freshness::Fresh,
            "unchanged fixture verifies Fresh"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cjs_fingerprints_detect_mtime_preserving_deep_edit() {
        // The tightest RB-5 regression: an EQUAL-LENGTH, MTIME-PRESERVING rewrite of the
        // deepest transitive require. Manifest+entry alone never see it; the cheap
        // mtime+len pre-filter never sees it. Only a content hash over the (now-cached)
        // deep module catches it — which is exactly what the fix threads through.
        let _guard = lock_cache();
        let (root, entry) = write_cjs_chain("strict");
        let runtime = ImportRuntime::Component;
        analyze_cjs_graph_with_runtime(&entry, runtime).expect("cjs walk");
        let fingerprints = cjs_module_fingerprints(&entry, runtime).expect("cached fingerprints");

        let b = root.join("b.js");
        let original = std::fs::metadata(&b)
            .expect("stat b")
            .modified()
            .expect("mtime");
        // Same byte length (`42` → `43`), different content.
        std::fs::write(&b, "module.exports = 43;\n").expect("rewrite b");
        std::fs::File::options()
            .write(true)
            .open(&b)
            .expect("open b")
            .set_modified(original)
            .expect("restore b mtime");

        assert_eq!(
            check_fingerprints_strict(&fingerprints),
            Freshness::Stale,
            "an equal-length, mtime-preserving deep CJS edit is caught by the content hash"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn clear_cjs_module_cache_drops_the_set() {
        let _guard = lock_cache();
        let (root, entry) = write_cjs_chain("clear");
        let runtime = ImportRuntime::Component;
        analyze_cjs_graph_with_runtime(&entry, runtime).expect("cjs walk");
        assert!(
            peek_cjs_module_paths(&entry, runtime).is_some(),
            "populated"
        );

        clear_cjs_module_cache();
        assert!(
            peek_cjs_module_paths(&entry, runtime).is_none(),
            "clear drops the cached CJS module set (cache-clear / invalidate-all seam)"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn purge_missing_cjs_module_sets_drops_deleted_entries() {
        let _guard = lock_cache();
        let (root, entry) = write_cjs_chain("purge");
        let runtime = ImportRuntime::Component;
        analyze_cjs_graph_with_runtime(&entry, runtime).expect("cjs walk");
        assert!(
            peek_cjs_module_paths(&entry, runtime).is_some(),
            "populated"
        );

        // Delete the whole package: the entry-path key is now definitively gone.
        std::fs::remove_dir_all(&root).ok();
        assert!(
            purge_missing_cjs_module_sets() >= 1,
            "the orphan purge reclaims a CJS module set whose entry no longer exists"
        );
        assert!(
            peek_cjs_module_paths(&entry, runtime).is_none(),
            "the purged set is gone"
        );
    }
}

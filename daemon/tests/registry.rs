use import_lens_daemon::{
    ipc::protocol::{
        RegistryHint, RegistryHintMode as ProtocolRegistryHintMode, RegistryHintTarget,
    },
    registry::{
        cache::RegistryMetadataCache,
        constants::{
            FRESH_HINT_TTL_MS, REGISTRY_MANUAL_RATE_LIMIT_REQUESTS, REGISTRY_RATE_LIMIT_REQUESTS,
            REGISTRY_REFRESH_CONCURRENCY, REGISTRY_RETENTION_MS,
        },
        service::{RegistryHintMode, RegistryHintService},
        types::{HttpRegistryResponse, RegistryHttpClient, RegistryPackageMetadata},
    },
    service::ImportLensService,
};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default)]
struct FakeRegistryHttpClient {
    calls: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<Vec<Result<HttpRegistryResponse, String>>>>,
}

impl FakeRegistryHttpClient {
    fn with_response(response: HttpRegistryResponse) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(vec![Ok(response)])),
        }
    }

    fn with_responses(responses: Vec<Result<HttpRegistryResponse, String>>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RegistryHttpClient for FakeRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(package_name.to_owned());
        self.responses.lock().expect("responses lock").remove(0)
    }
}

#[derive(Clone, Default)]
struct SlowRegistryHttpClient {
    calls: Arc<Mutex<Vec<String>>>,
}

impl SlowRegistryHttpClient {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RegistryHttpClient for SlowRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(package_name.to_owned());
        thread::sleep(Duration::from_millis(250));
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{
              "dist-tags":{"latest":"19.0.0"},
              "versions":{"18.2.0":{},"17.0.0":{"deprecated":"legacy release"}},
              "time":{"19.0.0":"2026-06-25T00:00:00.000Z"}
            }"#
            .to_owned(),
        })
    }
}

fn temp_cache_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "import-lens-registry-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("cache dir");
    path
}

#[test]
fn registry_service_builds_hint_from_metadata() {
    let cache_path = temp_cache_path("metadata");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"19.0.0"},
          "versions":{"18.2.0":{}},
          "modified":"2026-06-25T00:00:00.000Z"
        }"#
        .to_owned(),
    });
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 100);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(lookup.error, None);
    assert_eq!(
        lookup.hint,
        Some(RegistryHint {
            latest_version: Some("19.0.0".to_owned()),
            latest_published_at: Some("2026-06-25T00:00:00.000Z".to_owned()),
            is_latest: Some(false),
            deprecated: Some(false),
            fetched_at: Some(100),
        })
    );
}

#[test]
fn registry_hint_sources_published_at_from_abbreviated_modified_field() {
    let cache_path = temp_cache_path("abbreviated");
    // Genuine abbreviated ("corgi") metadata: dist-tags + versions + a top-level
    // `modified` timestamp, and no per-version `time` map (which the abbreviated
    // format the client requests does not include).
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"19.0.0"},
          "versions":{"18.2.0":{}},
          "modified":"2026-06-25T00:00:00.000Z"
        }"#
        .to_owned(),
    });
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 100);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(
        lookup.hint.and_then(|hint| hint.latest_published_at),
        Some("2026-06-25T00:00:00.000Z".to_owned())
    );
}

#[test]
fn registry_service_uses_cached_metadata_without_network_in_cached_mode() {
    let cache_path = temp_cache_path("cached");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: vec!["17.0.0".to_owned()],
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::default();
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for(
        "react",
        Some("18.2.0"),
        RegistryHintMode::Cached,
        10_000_000,
    );

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert!(client.calls().is_empty());
    assert_eq!(lookup.error, None);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned())
    );
}

#[test]
fn registry_service_derives_multiple_version_hints_from_one_cached_package_metadata() {
    let cache_path = temp_cache_path("cached-versions");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: vec!["17.0.0".to_owned()],
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::default();
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let current = service.hint_for(
        "react",
        Some("18.2.0"),
        RegistryHintMode::Cached,
        10_000_000,
    );
    let deprecated = service.hint_for(
        "react",
        Some("17.0.0"),
        RegistryHintMode::Cached,
        10_000_000,
    );

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert!(client.calls().is_empty());
    assert_eq!(current.hint.and_then(|item| item.deprecated), Some(false));
    assert_eq!(deprecated.hint.and_then(|item| item.deprecated), Some(true));
}

#[test]
fn registry_service_force_refresh_bypasses_fresh_cache() {
    let cache_path = temp_cache_path("force-refresh");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"20.0.0"},
          "versions":{"18.2.0":{}},
          "time":{"20.0.0":"2026-07-01T00:00:00.000Z"}
        }"#
        .to_owned(),
    });
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("20.0.0".to_owned()),
    );
}

#[test]
fn registry_service_refreshes_expired_package_metadata() {
    let cache_path = temp_cache_path("expired");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"20.0.0"},
          "versions":{"18.2.0":{}},
          "time":{"20.0.0":"2026-07-01T00:00:00.000Z"}
        }"#
        .to_owned(),
    });
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for(
        "react",
        Some("18.2.0"),
        RegistryHintMode::RefreshStale,
        51 + FRESH_HINT_TTL_MS,
    );

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("20.0.0".to_owned()),
    );
}

#[test]
fn registry_service_persists_retry_window_for_rate_limits() {
    let cache_path = temp_cache_path("retry-after");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 429,
        retry_after_ms: Some(1_000),
        body: r#"{"error":"rate limited"}"#.to_owned(),
    });
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 100);
    let second = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 500);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(lookup.hint, None);
    assert_eq!(lookup.error.as_deref(), Some("npm registry rate limit"));
    assert_eq!(second.hint, None);
    assert_eq!(second.error.as_deref(), Some("npm registry rate limit"));
}

#[test]
fn registry_service_retries_transient_failures_and_returns_stale_hint_with_error() {
    let cache_path = temp_cache_path("transient");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_responses(vec![
        Err("temporary registry failure 1".to_owned()),
        Err("temporary registry failure 2".to_owned()),
        Err("temporary registry failure 3".to_owned()),
    ]);
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for(
        "react",
        Some("18.2.0"),
        RegistryHintMode::ForceRefresh,
        10_000,
    );

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react", "react", "react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned()),
    );
    assert_eq!(
        lookup.error.as_deref(),
        Some("temporary registry failure 3")
    );
}

#[test]
fn registry_service_dedupes_in_flight_duplicate_targets() {
    let cache_path = temp_cache_path("singleflight");
    let client = SlowRegistryHttpClient::default();
    let service = Arc::new(RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    ));
    let start = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let service = Arc::clone(&service);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100)
            })
        })
        .collect::<Vec<_>>();

    start.wait();
    let lookups = handles
        .into_iter()
        .map(|handle| handle.join().expect("registry lookup should not panic"))
        .collect::<Vec<_>>();

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert!(lookups.iter().all(|lookup| lookup.error.is_none()));
    assert!(lookups.iter().all(|lookup| {
        lookup
            .hint
            .as_ref()
            .and_then(|hint| hint.latest_version.as_deref())
            == Some("19.0.0")
    }));
}

#[derive(Clone)]
struct PanicOnceRegistryHttpClient {
    calls: Arc<Mutex<Vec<String>>>,
    owner_started: Arc<Barrier>,
}

impl PanicOnceRegistryHttpClient {
    fn new(owner_started: Arc<Barrier>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            owner_started,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RegistryHttpClient for PanicOnceRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        let call_index = {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.push(package_name.to_owned());
            calls.len()
        };
        if call_index == 1 {
            self.owner_started.wait();
            // Give the follower time to join the in-flight fetch before the
            // owner unwinds.
            thread::sleep(Duration::from_millis(250));
            panic!("simulated registry client panic");
        }
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{
              "dist-tags":{"latest":"19.0.0"},
              "versions":{"18.2.0":{}},
              "time":{"19.0.0":"2026-06-25T00:00:00.000Z"}
            }"#
            .to_owned(),
        })
    }
}

#[test]
fn registry_service_releases_waiters_and_recovers_after_owner_panic() {
    let cache_path = temp_cache_path("owner-panic");
    let owner_started = Arc::new(Barrier::new(2));
    let client = PanicOnceRegistryHttpClient::new(Arc::clone(&owner_started));
    let service = Arc::new(RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    ));

    let owner = {
        let service = Arc::clone(&service);
        thread::spawn(move || {
            service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100)
        })
    };
    owner_started.wait();
    let waiter = {
        let service = Arc::clone(&service);
        thread::spawn(move || {
            service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100)
        })
    };

    assert!(owner.join().is_err());
    let waiter_lookup = waiter.join().expect("waiter should not hang or panic");
    let recovered = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 200);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react", "react"]);
    assert_eq!(waiter_lookup.hint, None);
    assert_eq!(
        waiter_lookup.error.as_deref(),
        Some("registry fetch panicked")
    );
    assert_eq!(recovered.error, None);
    assert_eq!(
        recovered.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned()),
    );
}

#[test]
fn registry_cache_persists_latest_snapshot_under_concurrent_writes() {
    let cache_path = temp_cache_path("concurrent-cache");
    let cache = Arc::new(RegistryMetadataCache::new(cache_path.clone()));
    let handles = (0..16)
        .map(|index| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                cache
                    .write_metadata(
                        &format!("pkg-{index}"),
                        RegistryPackageMetadata {
                            latest_version: Some("2.0.0".to_owned()),
                            latest_published_at: None,
                            deprecated_versions: Vec::new(),
                        },
                        index,
                    )
                    .expect("concurrent cache write");
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("cache writer should not panic");
    }

    let persisted =
        fs::read_to_string(cache_path.join("registry-metadata.json")).expect("cache file");
    let value = serde_json::from_str::<serde_json::Value>(&persisted).expect("cache json");

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    // The snapshot is now a versioned envelope; the 16 concurrent writes land
    // under `entries`, not at the top level.
    assert_eq!(value["schema_version"], 1);
    assert_eq!(
        value["entries"].as_object().expect("entries object").len(),
        16
    );
}

#[test]
fn purge_expired_drops_stale_registry_entries_and_they_stay_gone_on_reload() {
    let cache_path = temp_cache_path("purge-expired");
    let now = 1_000 * REGISTRY_RETENTION_MS;
    {
        let cache = RegistryMetadataCache::new(cache_path.clone());
        cache
            .write_metadata("fresh", sample_metadata("1.0.0"), now)
            .expect("write fresh");
        cache
            .write_metadata(
                "stale",
                sample_metadata("1.0.0"),
                now - REGISTRY_RETENTION_MS - 1,
            )
            .expect("write stale");
        cache.flush().expect("flush");

        assert_eq!(cache.purge_expired(now, REGISTRY_RETENTION_MS), 1);
    }

    // Reload: the on-disk union must not resurrect the pruned entry.
    let reloaded = RegistryMetadataCache::new(cache_path.clone());
    assert!(reloaded.get("fresh").is_some());
    assert!(reloaded.get("stale").is_none());
    fs::remove_dir_all(cache_path).expect("cleanup");
}

fn sample_metadata(latest: &str) -> RegistryPackageMetadata {
    RegistryPackageMetadata {
        latest_version: Some(latest.to_owned()),
        latest_published_at: None,
        deprecated_versions: Vec::new(),
    }
}

#[test]
fn registry_metadata_defers_persistence_until_flush() {
    let cache_path = temp_cache_path("flush-debounce");
    let file = cache_path.join("registry-metadata.json");
    {
        let cache = RegistryMetadataCache::new(cache_path.clone());
        for i in 0..5u64 {
            cache
                .write_metadata(
                    &format!("pkg{i}"),
                    sample_metadata(&format!("1.0.{i}")),
                    1000 + i,
                )
                .expect("write");
        }
        // Below the persist threshold: nothing written to disk yet.
        assert!(
            !file.exists(),
            "writes below the threshold should defer persistence"
        );
        cache.flush().expect("flush");
        assert!(file.exists(), "flush should persist the snapshot");
    }

    let reloaded = RegistryMetadataCache::new(cache_path.clone());
    for i in 0..5u64 {
        assert!(
            reloaded.get(&format!("pkg{i}")).is_some(),
            "pkg{i} should reload after flush"
        );
    }
    fs::remove_dir_all(cache_path).expect("cleanup");
}

#[test]
fn run_cache_maintenance_invokes_registry_retention() {
    let cache_path = temp_cache_path("service-maintenance");
    let registry = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(FakeRegistryHttpClient::default()),
    );
    let service = ImportLensService::new_with_registry_hints_for_tests(registry);

    // `run_cache_maintenance` reads the real wall clock for `now`, so seed one
    // entry stamped at the epoch (unconditionally past the 30-day retention
    // window) and one stamped "now" that must survive the pass.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64;
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("fresh", "1.0.0", now);
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("expired", "1.0.0", 0);

    service.run_cache_maintenance();

    // Reconstruct over the same shared file: the service-level maintenance pass
    // must have invoked registry retention and written it authoritatively.
    let reloaded = RegistryMetadataCache::new(cache_path.clone());
    let fresh_present = reloaded.get("fresh").is_some();
    let expired_present = reloaded.get("expired").is_some();
    fs::remove_dir_all(cache_path).expect("cleanup");
    assert!(
        fresh_present,
        "a fresh registry entry must survive service cache maintenance"
    );
    assert!(
        !expired_present,
        "run_cache_maintenance must invoke registry retention and drop the expired entry"
    );
}

#[test]
fn manual_refresh_cooldown_coalesces_to_cached() {
    let cache_path = temp_cache_path("manual-cooldown");
    // Two distinct responses are queued, but the cooldown must let only the FIRST
    // manual fetch reach the network; an immediate re-click coalesces to the
    // just-cached value and never consumes the second response.
    let client = FakeRegistryHttpClient::with_responses(vec![
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"19.0.0"},"versions":{"18.2.0":{}}}"#.to_owned(),
        }),
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"20.0.0"},"versions":{"18.2.0":{}}}"#.to_owned(),
        }),
    ]);
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let first = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100);
    // Immediate re-click: within the manual cooldown it must NOT fetch again.
    let second = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 200);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(
        client.calls(),
        vec!["react"],
        "a re-click within the cooldown must coalesce to the cached value, not refetch"
    );
    assert_eq!(
        first.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned())
    );
    // The second lookup returns the FIRST fetch's cached value (19.0.0), not the
    // un-consumed 20.0.0 response, and is not an error.
    assert_eq!(second.error, None);
    assert_eq!(
        second.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned())
    );
}

#[test]
fn retry_after_backs_off_all_manual_fetches() {
    let cache_path = temp_cache_path("global-backoff");
    // Package A returns a 429 with a Retry-After; package B (distinct) would
    // return 200 but must be held off by the GLOBAL backoff the 429 installed on
    // the shared rate limiter — proving Retry-After suppresses ALL fetches, not
    // just the rate-limited package's own per-entry retry window.
    let client = FakeRegistryHttpClient::with_responses(vec![
        Ok(HttpRegistryResponse {
            status: 429,
            retry_after_ms: Some(250),
            body: r#"{"error":"rate limited"}"#.to_owned(),
        }),
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"19.0.0"},"versions":{"18.2.0":{}}}"#.to_owned(),
        }),
    ]);
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let _a = service.hint_for("pkg-a", Some("1.0.0"), RegistryHintMode::ForceRefresh, 100);
    let started = Instant::now();
    let b = service.hint_for("pkg-b", Some("1.0.0"), RegistryHintMode::ForceRefresh, 200);
    let elapsed = started.elapsed();

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["pkg-a", "pkg-b"]);
    // B is delayed, not dropped: it still fetches once the backoff elapses.
    assert_eq!(
        b.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned())
    );
    // The distinct package's manual fetch waited ~the Retry-After A asked for.
    // `thread::sleep` never returns early, so this lower bound is not flaky.
    assert!(
        elapsed >= Duration::from_millis(180),
        "the 429 Retry-After must globally delay a distinct package's manual fetch (waited {elapsed:?})"
    );
}

#[test]
fn manual_rate_is_stricter_than_background() {
    // The manual `ForceRefresh` budget is deliberately stricter than the
    // background `RefreshStale` budget (D6 / §6.1). The behavioral proof — that a
    // burst throttles at the lower cap while background does not — lives in the
    // limiter's own unit test; here we pin the public consts' relationship so the
    // stricter-manual guarantee cannot silently regress. Compile-time asserts so a
    // regression fails the build, not just the run.
    const {
        assert!(
            REGISTRY_MANUAL_RATE_LIMIT_REQUESTS < REGISTRY_RATE_LIMIT_REQUESTS,
            "manual fetches must reserve against a stricter per-window budget than background"
        );
    }
    const {
        assert!(
            REGISTRY_MANUAL_RATE_LIMIT_REQUESTS >= 1,
            "the manual budget must still admit at least one fetch per window"
        );
    }
}

/// Records the peak number of `get_package_metadata` calls in flight at once so
/// a bulk block can prove its fan-out never overlaps more than the isolated
/// registry pool's `REGISTRY_REFRESH_CONCURRENCY` fetches.
struct ConcurrencyProbeRegistryClient {
    concurrent: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
}

impl RegistryHttpClient for ConcurrencyProbeRegistryClient {
    fn get_package_metadata(&self, _package_name: &str) -> Result<HttpRegistryResponse, String> {
        let in_flight = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(in_flight, Ordering::SeqCst);
        // Hold the "connection" open long enough that, were the fan-out not
        // bounded by the pool, far more than the pool size would overlap here.
        thread::sleep(Duration::from_millis(50));
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"1.0.0"},"versions":{}}"#.to_owned(),
        })
    }
}

/// Counts every network fetch and blocks each one on a shared gate the test
/// opens on demand, so a bulk block can be frozen with exactly the pool size in
/// flight before cancellation is triggered.
struct GatedCountingRegistryClient {
    calls: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl RegistryHttpClient for GatedCountingRegistryClient {
    fn get_package_metadata(&self, _package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (open, ready) = &*self.gate;
        let mut opened = open.lock().expect("gate lock");
        while !*opened {
            opened = ready.wait(opened).expect("gate wait");
        }
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"1.0.0"},"versions":{}}"#.to_owned(),
        })
    }
}

/// Lets one manual seed package fetch immediately, while every other package
/// blocks on the shared gate. This isolates ForceRefresh cooldown behavior from
/// the worker-pool queue.
struct ManualSeedThenGatedRegistryClient {
    calls: Arc<Mutex<Vec<String>>>,
    gated_calls: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl RegistryHttpClient for ManualSeedThenGatedRegistryClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(package_name.to_owned());
        if package_name == "manual-cached" {
            return Ok(HttpRegistryResponse {
                status: 200,
                retry_after_ms: None,
                body: r#"{"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{}}}"#.to_owned(),
            });
        }

        self.gated_calls.fetch_add(1, Ordering::SeqCst);
        let (open, ready) = &*self.gate;
        let mut opened = open.lock().expect("gate lock");
        while !*opened {
            opened = ready.wait(opened).expect("gate wait");
        }
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"1.0.0"},"versions":{}}"#.to_owned(),
        })
    }
}

fn bulk_targets(count: usize) -> Vec<RegistryHintTarget> {
    (0..count)
        .map(|index| RegistryHintTarget {
            name: format!("pkg-{index}"),
            installed_version: Some("1.0.0".to_owned()),
        })
        .collect()
}

fn open_gate(gate: &Arc<(Mutex<bool>, Condvar)>) {
    let (open, ready) = &**gate;
    *open.lock().expect("gate lock") = true;
    ready.notify_all();
}

#[test]
fn bulk_refresh_in_flight_is_bounded() {
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let registry = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(ConcurrencyProbeRegistryClient {
            concurrent: Arc::clone(&concurrent),
            max_concurrent: Arc::clone(&max_concurrent),
        }),
    );
    let service = Arc::new(ImportLensService::new_with_registry_hints_for_tests(
        registry,
    ));

    // A block several times the pool size. If the fan-out spawned a thread per
    // target (rather than dispatching onto the bounded pool), every fetch would
    // overlap at once. Kept within one rate-limit window so the shared limiter
    // never serializes the fan-out and masks the bound.
    let target_count = 16;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    service.spawn_registry_refresh_block(
        bulk_targets(target_count),
        ProtocolRegistryHintMode::RefreshStale,
        1_000,
        cancelled,
        move |_index, _result| {
            let _ = done_tx.send(());
        },
    );

    for _ in 0..target_count {
        done_rx
            .recv()
            .expect("every bulk job should report completion");
    }

    let peak = max_concurrent.load(Ordering::SeqCst);
    assert!(
        peak <= REGISTRY_REFRESH_CONCURRENCY,
        "bulk fan-out exceeded the pool's in-flight cap: {peak} > {REGISTRY_REFRESH_CONCURRENCY}"
    );
    assert!(
        peak >= 2,
        "bulk fetches never overlapped ({peak} peak); the concurrency probe is vacuous"
    );
}

#[test]
fn bulk_refresh_streams_fresh_cached_targets_before_worker_pool_queue() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let cache = RegistryMetadataCache::empty();
    cache
        .write_metadata(
            &format!("pkg-{}", REGISTRY_REFRESH_CONCURRENCY),
            RegistryPackageMetadata {
                latest_version: Some("2.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            1_000,
        )
        .expect("cache write");
    let registry = RegistryHintService::new(
        cache,
        Box::new(GatedCountingRegistryClient {
            calls: Arc::clone(&calls),
            gate: Arc::clone(&gate),
        }),
    );
    let service = Arc::new(ImportLensService::new_with_registry_hints_for_tests(
        registry,
    ));

    let target_count = REGISTRY_REFRESH_CONCURRENCY + 1;
    let cached_index = REGISTRY_REFRESH_CONCURRENCY;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<(usize, Option<String>, Option<String>)>();
    service.spawn_registry_refresh_block(
        bulk_targets(target_count),
        ProtocolRegistryHintMode::RefreshStale,
        1_000 + FRESH_HINT_TTL_MS / 2,
        cancelled,
        move |index, result| {
            let origin = result
                .as_ref()
                .and_then(|result| result.origin.as_deref().map(str::to_owned));
            let latest = result
                .and_then(|result| result.hint)
                .and_then(|hint| hint.latest_version);
            let _ = done_tx.send((index, origin, latest));
        },
    );

    while calls.load(Ordering::SeqCst) < REGISTRY_REFRESH_CONCURRENCY {
        thread::yield_now();
    }
    let early = done_rx.recv_timeout(Duration::from_millis(100));
    open_gate(&gate);
    for _ in 1..target_count {
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("remaining registry refresh result should arrive after opening the gate");
    }

    let (index, origin, latest) =
        early.expect("fresh cached target should stream before queued network workers finish");
    assert_eq!(index, cached_index);
    assert_eq!(origin.as_deref(), Some("cache"));
    assert_eq!(latest.as_deref(), Some("2.0.0"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        REGISTRY_REFRESH_CONCURRENCY,
        "the cached target must not consume a network worker"
    );
}

#[test]
fn bulk_force_refresh_streams_manual_cooldown_cache_before_worker_pool_queue() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let gated_calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let registry = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(ManualSeedThenGatedRegistryClient {
            calls: Arc::clone(&calls),
            gated_calls: Arc::clone(&gated_calls),
            gate: Arc::clone(&gate),
        }),
    );
    let service = Arc::new(ImportLensService::new_with_registry_hints_for_tests(
        registry,
    ));
    let manual_target = RegistryHintTarget {
        name: "manual-cached".to_owned(),
        installed_version: Some("1.0.0".to_owned()),
    };
    let seeded = service.refresh_registry_hint_target(
        manual_target.clone(),
        ProtocolRegistryHintMode::ForceRefresh,
        1_000,
    );
    assert_eq!(seeded.origin.as_deref(), Some("network"));

    let mut targets = bulk_targets(REGISTRY_REFRESH_CONCURRENCY);
    targets.push(manual_target);
    let cached_index = REGISTRY_REFRESH_CONCURRENCY;
    let target_count = targets.len();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<(usize, Option<String>, Option<String>)>();
    service.spawn_registry_refresh_block(
        targets,
        ProtocolRegistryHintMode::ForceRefresh,
        1_100,
        cancelled,
        move |index, result| {
            let origin = result
                .as_ref()
                .and_then(|result| result.origin.as_deref().map(str::to_owned));
            let latest = result
                .and_then(|result| result.hint)
                .and_then(|hint| hint.latest_version);
            let _ = done_tx.send((index, origin, latest));
        },
    );

    while gated_calls.load(Ordering::SeqCst) < REGISTRY_REFRESH_CONCURRENCY {
        thread::yield_now();
    }
    let early = done_rx.recv_timeout(Duration::from_millis(100));
    open_gate(&gate);
    for _ in 1..target_count {
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("remaining registry refresh result should arrive after opening the gate");
    }

    let (index, origin, latest) =
        early.expect("manual cooldown cached target should stream before queued workers finish");
    assert_eq!(index, cached_index);
    assert_eq!(origin.as_deref(), Some("cache"));
    assert_eq!(latest.as_deref(), Some("2.0.0"));
    let calls = calls.lock().expect("calls lock").clone();
    assert_eq!(calls.first().map(String::as_str), Some("manual-cached"));
    assert_eq!(
        calls
            .iter()
            .filter(|name| name.as_str() == "manual-cached")
            .count(),
        1,
        "the cooldown target must not refetch during the bulk ForceRefresh"
    );
    assert_eq!(calls.len(), REGISTRY_REFRESH_CONCURRENCY + 1);
    for index in 0..REGISTRY_REFRESH_CONCURRENCY {
        assert!(
            calls.contains(&format!("pkg-{index}")),
            "missing gated worker fetch for pkg-{index}: {calls:?}"
        );
    }
}

#[test]
fn bulk_refresh_stops_after_cancel() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let registry = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(GatedCountingRegistryClient {
            calls: Arc::clone(&calls),
            gate: Arc::clone(&gate),
        }),
    );
    let service = Arc::new(ImportLensService::new_with_registry_hints_for_tests(
        registry,
    ));

    let target_count = 20;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    service.spawn_registry_refresh_block(
        bulk_targets(target_count),
        ProtocolRegistryHintMode::RefreshStale,
        1_000,
        Arc::clone(&cancelled),
        move |_index, result| {
            let _ = done_tx.send(result.is_some());
        },
    );

    // Freeze the block with exactly the pool size in flight: each of the first
    // `REGISTRY_REFRESH_CONCURRENCY` jobs is parked in the gated client, and the
    // shut gate stops any worker returning to pick up a further target, so the
    // call count parks here rather than racing ahead.
    while calls.load(Ordering::SeqCst) < REGISTRY_REFRESH_CONCURRENCY {
        thread::yield_now();
    }

    // Supersede the block, THEN release the in-flight fetches. Every worker that
    // now returns and picks up a remaining target observes the cancel flag and
    // skips its fetch, so no further network call is made.
    cancelled.store(true, Ordering::Release);
    open_gate(&gate);

    let mut ran = 0;
    for _ in 0..target_count {
        if done_rx
            .recv()
            .expect("every bulk job should report completion")
        {
            ran += 1;
        }
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        REGISTRY_REFRESH_CONCURRENCY,
        "a cancelled block must not fetch beyond what was already in flight"
    );
    assert_eq!(
        ran, REGISTRY_REFRESH_CONCURRENCY,
        "only the already-in-flight jobs should complete with a fetched result"
    );
    assert!(
        ran < target_count,
        "cancellation must have skipped the remaining targets"
    );
}

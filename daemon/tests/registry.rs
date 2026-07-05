use import_lens_daemon::{
    ipc::protocol::RegistryHint,
    registry::{
        cache::RegistryMetadataCache,
        constants::{FRESH_HINT_TTL_MS, REGISTRY_RETENTION_MS},
        service::{RegistryHintMode, RegistryHintService},
        types::{HttpRegistryResponse, RegistryHttpClient, RegistryPackageMetadata},
    },
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::Duration,
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
    assert_eq!(value.as_object().expect("cache object").len(), 16);
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

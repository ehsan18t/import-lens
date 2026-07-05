use super::{
    cache::{self, RegistryMetadataCache},
    constants::{
        FRESH_HINT_TTL_MS, MAX_ATTEMPTS, NOT_FOUND_TTL_MS, REGISTRY_BODY_TOO_LARGE_ERROR,
        REGISTRY_RATE_LIMIT_REQUESTS, REGISTRY_RATE_LIMIT_WINDOW_MS, REGISTRY_RETRY_BASE_DELAY_MS,
        TRANSIENT_ERROR_RETRY_MS,
    },
    types::{
        HttpRegistryResponse, RegistryHintLookup, RegistryHintOrigin, RegistryHttpClient,
        RegistryPackageMetadata, RegistryPackageMetadataEntry,
    },
};
use crate::{ipc::protocol::RegistryHint, logging};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryHintMode {
    Off,
    Cached,
    RefreshStale,
    ForceRefresh,
}

pub struct RegistryHintService {
    cache: RegistryMetadataCache,
    client: Box<dyn RegistryHttpClient>,
    in_flight: Mutex<HashMap<String, Arc<InflightRegistryPackageFetch>>>,
    rate_limiter: Mutex<RegistryRateLimiter>,
}

struct InflightRegistryPackageFetch {
    result: Mutex<Option<RegistryPackageMetadataEntry>>,
    ready: Condvar,
}

impl InflightRegistryPackageFetch {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

/// Cleans up after the owning fetch on both success and unwind. If the owner
/// panics before publishing a result, waiters would otherwise block on the
/// condvar forever and the stale in-flight entry would wedge every future
/// fetch for the same package.
struct InflightFetchGuard<'a> {
    service: &'a RegistryHintService,
    key: String,
    flight: Arc<InflightRegistryPackageFetch>,
}

impl Drop for InflightFetchGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut result) = self.flight.result.lock()
            && result.is_none()
        {
            *result = Some(RegistryPackageMetadataEntry {
                metadata: None,
                updated_at: 0,
                retry_after: None,
                error: Some("registry fetch panicked".to_owned()),
                not_found: false,
            });
        }
        self.flight.ready.notify_all();
        if let Ok(mut in_flight) = self.service.in_flight.lock()
            && in_flight
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            in_flight.remove(&self.key);
        }
    }
}

struct RegistryRateLimiter {
    window_opens_at: Instant,
    request_count: usize,
}

impl RegistryRateLimiter {
    fn new() -> Self {
        Self {
            window_opens_at: Instant::now(),
            request_count: 0,
        }
    }

    /// Reserves a rate-limit slot and returns how long the caller must sleep
    /// *after releasing the lock*. Sleeping while holding the mutex would
    /// serialize every registry worker during backoff and defeat the bounded
    /// concurrency this refresh path is built around.
    ///
    /// `window_opens_at` may point into the future when a full window forced a
    /// caller to reserve the next one. Later callers must count against that
    /// reserved window (and sleep until it opens) instead of treating the
    /// reservation as a fresh open window, otherwise a burst fires everything
    /// after the boundary caller immediately.
    fn reserve_slot(&mut self) -> Option<Duration> {
        let window = Duration::from_millis(REGISTRY_RATE_LIMIT_WINDOW_MS);
        let now = Instant::now();
        if now >= self.window_opens_at + window {
            // The most recently reserved window has fully elapsed: start a
            // fresh one right now.
            self.window_opens_at = now;
            self.request_count = 1;
            return None;
        }
        if self.request_count < REGISTRY_RATE_LIMIT_REQUESTS {
            self.request_count += 1;
            // Sleep until the reserved window opens; a zero wait means the
            // window is already open and the caller may proceed immediately.
            let wait = self.window_opens_at.saturating_duration_since(now);
            return if wait.is_zero() { None } else { Some(wait) };
        }
        // The reserved window is full: reserve the first slot of the next one.
        self.window_opens_at += window;
        self.request_count = 1;
        Some(self.window_opens_at.saturating_duration_since(now))
    }
}

impl RegistryHintService {
    pub fn new(cache: RegistryMetadataCache, client: Box<dyn RegistryHttpClient>) -> Self {
        Self {
            cache,
            client,
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            cache: RegistryMetadataCache::empty(),
            client: Box::new(NoopRegistryHttpClient),
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
        }
    }

    /// Persists any registry metadata fetched since the last flush. Called at the
    /// end of a package.json analysis or a registry-hint refresh so per-package
    /// writes collapse into one snapshot rewrite.
    pub fn flush(&self) {
        if let Err(error) = self.cache.flush() {
            logging::log_warn(
                "registry",
                format!("failed to persist registry metadata: {error}"),
            );
        }
    }

    /// Prunes registry metadata past the retention window. Called from the
    /// user-triggered orphan purge so the shared metadata file stops growing
    /// monotonically. No-op for the disabled service. Returns the count removed.
    pub fn purge_expired_metadata(&self) -> usize {
        self.cache.purge_expired(
            crate::time::unix_millis_now(),
            crate::registry::constants::REGISTRY_RETENTION_MS,
        )
    }

    pub fn hint_for(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        mode: RegistryHintMode,
        now_ms: u64,
    ) -> RegistryHintLookup {
        if mode == RegistryHintMode::Off {
            return RegistryHintLookup {
                hint: None,
                error: None,
                origin: RegistryHintOrigin::Cache,
            };
        }

        let cached = self.cache.get(package_name);
        if mode == RegistryHintMode::Cached {
            return cached
                .as_ref()
                .map(|entry| lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache))
                .unwrap_or(RegistryHintLookup {
                    hint: None,
                    error: None,
                    origin: RegistryHintOrigin::Cache,
                });
        }
        if mode != RegistryHintMode::ForceRefresh
            && let Some(entry) = cached.as_ref()
        {
            if mode == RegistryHintMode::RefreshStale && is_usable_without_fetch(entry, now_ms) {
                return lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache);
            }
            if entry
                .retry_after
                .is_some_and(|retry_after| retry_after > now_ms)
            {
                return lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache);
            }
        }

        let entry = self.fetch_package_singleflight(package_name, now_ms);
        lookup_from_entry(&entry, installed_version, RegistryHintOrigin::Network)
    }

    fn fetch_package_singleflight(
        &self,
        package_name: &str,
        now_ms: u64,
    ) -> RegistryPackageMetadataEntry {
        let key = cache::cache_key(package_name);
        let (flight, is_owner) = match self.in_flight.lock() {
            Ok(mut in_flight) => {
                if let Some(flight) = in_flight.get(&key) {
                    (Arc::clone(flight), false)
                } else {
                    let flight = Arc::new(InflightRegistryPackageFetch::new());
                    in_flight.insert(key.clone(), Arc::clone(&flight));
                    (flight, true)
                }
            }
            // Poisoned in-flight map: skip de-duplication and fetch directly.
            Err(_) => return self.fetch_package_with_retries(package_name, now_ms),
        };

        if is_owner {
            // Registered as guard before fetching so an unwinding fetch still
            // publishes a failure result, wakes waiters, and clears the map
            // entry on drop.
            let _cleanup = InflightFetchGuard {
                service: self,
                key,
                flight: Arc::clone(&flight),
            };
            let result = self.fetch_package_with_retries(package_name, now_ms);
            if let Ok(mut guard) = flight.result.lock() {
                *guard = Some(result.clone());
            }
            return result;
        }

        let Ok(mut guard) = flight.result.lock() else {
            // Poisoned in-flight result: fall back to fetching directly.
            return self.fetch_package_with_retries(package_name, now_ms);
        };
        while guard.is_none() {
            match flight.ready.wait(guard) {
                Ok(next) => guard = next,
                // Poisoned while waiting: fall back to fetching directly.
                Err(_) => return self.fetch_package_with_retries(package_name, now_ms),
            }
        }
        // The loop above only exits once the owner (or its drop guard)
        // published a result, so this is a logic invariant rather than a
        // recoverable lock failure.
        guard.clone().expect("registry in-flight result")
    }

    fn fetch_package_with_retries(
        &self,
        package_name: &str,
        now_ms: u64,
    ) -> RegistryPackageMetadataEntry {
        let mut last_error = None;
        let mut permanent = false;
        let mut attempts_made = 0;
        for attempt in 1..=MAX_ATTEMPTS {
            attempts_made = attempt;
            self.wait_for_rate_limit_slot();
            let started = Instant::now();
            match self.client.get_package_metadata(package_name) {
                Ok(response) if response.status == 200 => {
                    let body_bytes = response.body.len();
                    let elapsed_ms = started.elapsed().as_millis();
                    let metadata = match package_metadata_from_response(response) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            logging::log_warn(
                                "registry",
                                format!("failed to parse npm metadata for {package_name}: {error}"),
                            );
                            last_error = Some(error);
                            break;
                        }
                    };
                    logging::log_debug(
                        "registry",
                        format!(
                            "fetched npm metadata for {package_name}: 200, {body_bytes} bytes, {elapsed_ms}ms"
                        ),
                    );
                    let entry = RegistryPackageMetadataEntry {
                        metadata: Some(metadata),
                        updated_at: now_ms,
                        retry_after: None,
                        error: None,
                        not_found: false,
                    };
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!("failed to persist npm metadata for {package_name}: {error}"),
                        );
                    }
                    return entry;
                }
                Ok(response) if response.status == 404 => {
                    let entry = RegistryPackageMetadataEntry {
                        metadata: None,
                        updated_at: now_ms,
                        retry_after: None,
                        error: None,
                        not_found: true,
                    };
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!(
                                "failed to persist npm not-found metadata for {package_name}: {error}"
                            ),
                        );
                    }
                    return entry;
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = now_ms
                        + response
                            .retry_after_ms
                            .unwrap_or_else(|| transient_backoff_ms(attempt));
                    logging::log_warn(
                        "registry",
                        format!(
                            "npm registry rate limited {package_name}; retry after {retry_after}"
                        ),
                    );
                    let entry = failed_entry_from_cache(
                        self.cache.get(package_name).as_ref(),
                        "npm registry rate limit".to_owned(),
                        retry_after,
                    );
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!(
                                "failed to persist npm rate-limit metadata for {package_name}: {error}"
                            ),
                        );
                    }
                    return entry;
                }
                Ok(response) => {
                    last_error = Some(format!("npm registry responded with {}", response.status));
                    if attempt == MAX_ATTEMPTS || !is_transient_status(response.status) {
                        break;
                    }
                    logging::log_debug(
                        "registry",
                        format!(
                            "retrying npm metadata fetch for {package_name} after HTTP {} attempt {attempt}",
                            response.status,
                        ),
                    );
                    sleep_before_retry(attempt);
                }
                Err(error) => {
                    if is_permanent_fetch_error(&error) {
                        last_error = Some(error);
                        permanent = true;
                        break;
                    }
                    last_error = Some(error);
                    if attempt == MAX_ATTEMPTS {
                        break;
                    }
                    logging::log_debug(
                        "registry",
                        format!(
                            "retrying npm metadata fetch for {package_name} after network failure attempt {attempt}"
                        ),
                    );
                    sleep_before_retry(attempt);
                }
            }
        }

        let retry_after_ms = if permanent {
            now_ms + NOT_FOUND_TTL_MS
        } else {
            now_ms + TRANSIENT_ERROR_RETRY_MS
        };
        logging::log_warn(
            "registry",
            format!(
                "failed to refresh npm metadata for {package_name} after {attempts_made} attempt(s){}: {}",
                if permanent {
                    " (permanent, cached 6h)"
                } else {
                    ""
                },
                last_error.as_deref().unwrap_or("unknown error"),
            ),
        );
        let entry = failed_entry_from_cache(
            self.cache.get(package_name).as_ref(),
            last_error
                .clone()
                .unwrap_or_else(|| "unknown registry error".to_owned()),
            retry_after_ms,
        );
        if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
            logging::log_warn(
                "registry",
                format!("failed to persist npm error metadata for {package_name}: {error}"),
            );
        }
        entry
    }

    /// Test-only: seeds the cache directly so integration tests (which
    /// compile this crate as an external dependency and cannot see
    /// `#[cfg(test)]` items) can exercise cached-hint lookups without a real
    /// network fetch.
    pub fn write_metadata_for_tests(
        &self,
        package_name: &str,
        latest_version: &str,
        fetched_at: u64,
    ) -> Result<(), String> {
        self.cache.write_metadata(
            package_name,
            RegistryPackageMetadata {
                latest_version: Some(latest_version.to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            fetched_at,
        )
    }

    fn wait_for_rate_limit_slot(&self) {
        // Poisoned rate limiter: proceed without throttling rather than
        // failing the fetch.
        let wait = match self.rate_limiter.lock() {
            Ok(mut rate_limiter) => rate_limiter.reserve_slot(),
            Err(_) => None,
        };
        if let Some(delay) = wait {
            thread::sleep(delay);
        }
    }
}

struct NoopRegistryHttpClient;

impl RegistryHttpClient for NoopRegistryHttpClient {
    fn get_package_metadata(&self, _package_name: &str) -> Result<HttpRegistryResponse, String> {
        Err("registry client disabled".to_owned())
    }
}

fn is_usable_without_fetch(entry: &RegistryPackageMetadataEntry, now_ms: u64) -> bool {
    if entry.metadata.is_some() {
        return now_ms.saturating_sub(entry.updated_at) <= FRESH_HINT_TTL_MS;
    }
    entry.not_found && now_ms.saturating_sub(entry.updated_at) <= NOT_FOUND_TTL_MS
}

fn lookup_from_entry(
    entry: &RegistryPackageMetadataEntry,
    installed_version: Option<&str>,
    origin: RegistryHintOrigin,
) -> RegistryHintLookup {
    RegistryHintLookup {
        hint: entry.metadata.as_ref().map(|metadata| {
            registry_hint_from_metadata(metadata, installed_version, entry.updated_at)
        }),
        error: entry.error.clone(),
        origin,
    }
}

fn registry_hint_from_metadata(
    metadata: &RegistryPackageMetadata,
    installed_version: Option<&str>,
    fetched_at: u64,
) -> RegistryHint {
    RegistryHint {
        is_latest: installed_version
            .zip(metadata.latest_version.as_deref())
            .map(|(installed, latest)| installed == latest),
        latest_version: metadata.latest_version.clone(),
        latest_published_at: metadata.latest_published_at.clone(),
        deprecated: installed_version.map(|version| {
            metadata
                .deprecated_versions
                .iter()
                .any(|item| item == version)
        }),
        fetched_at: Some(fetched_at),
    }
}

fn package_metadata_from_response(
    response: HttpRegistryResponse,
) -> Result<RegistryPackageMetadata, String> {
    let document =
        serde_json::from_str::<Value>(&response.body).map_err(|error| error.to_string())?;
    let latest_version = document
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // The abbreviated ("corgi") packument the client requests omits the
    // per-version `time` map but includes a top-level `modified` timestamp,
    // which reflects the latest publish in the common case. Sourcing from
    // `modified` keeps this field populated without fetching the full packument.
    let latest_published_at = document
        .get("modified")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut deprecated_versions = document
        .get("versions")
        .and_then(Value::as_object)
        .map(|versions| {
            versions
                .iter()
                .filter_map(|(version, metadata)| {
                    metadata
                        .get("deprecated")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .map(|_| version.clone())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    deprecated_versions.sort();

    Ok(RegistryPackageMetadata {
        latest_version,
        latest_published_at,
        deprecated_versions,
    })
}

fn failed_entry_from_cache(
    cached: Option<&RegistryPackageMetadataEntry>,
    error: String,
    retry_after: u64,
) -> RegistryPackageMetadataEntry {
    RegistryPackageMetadataEntry {
        metadata: cached.and_then(|entry| entry.metadata.clone()),
        updated_at: cached.map(|entry| entry.updated_at).unwrap_or(0),
        retry_after: Some(retry_after),
        error: Some(error),
        not_found: false,
    }
}

fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 425 || status == 429 || status >= 500
}

/// A permanent fetch failure will not succeed on retry within a short window, so
/// we skip the remaining attempts and cache it for the not-found TTL instead of
/// the 5-minute transient window. An oversize response body (exceeds
/// `MAX_REGISTRY_BODY_BYTES`) is the current instance; the client normalizes it
/// to `REGISTRY_BODY_TOO_LARGE_ERROR` so this check is stable across ureq versions.
fn is_permanent_fetch_error(message: &str) -> bool {
    message == REGISTRY_BODY_TOO_LARGE_ERROR
}

fn transient_backoff_ms(attempt: usize) -> u64 {
    REGISTRY_RETRY_BASE_DELAY_MS * attempt as u64
}

fn sleep_before_retry(attempt: usize) {
    thread::sleep(Duration::from_millis(transient_backoff_ms(attempt)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rate_limiter_throttles_every_caller_once_the_window_limit_is_hit() {
        let mut limiter = RegistryRateLimiter::new();
        let window = Duration::from_millis(REGISTRY_RATE_LIMIT_WINDOW_MS);

        for _ in 0..REGISTRY_RATE_LIMIT_REQUESTS {
            assert_eq!(limiter.reserve_slot(), None);
        }

        let boundary = limiter
            .reserve_slot()
            .expect("boundary caller should wait for the next window");
        assert!(boundary <= window);

        // Callers arriving while the next window is reserved must also wait
        // instead of firing immediately; otherwise a burst blows through the
        // per-window request limit.
        let follower = limiter
            .reserve_slot()
            .expect("followers arriving during a reserved window should also wait");
        assert!(follower <= window);
    }

    #[test]
    fn permanent_errors_are_recognized() {
        assert!(is_permanent_fetch_error(REGISTRY_BODY_TOO_LARGE_ERROR));
        assert!(!is_permanent_fetch_error(
            "the response body is larger than request limit: 67108864"
        ));
        assert!(!is_permanent_fetch_error("connection reset by peer"));
        assert!(!is_permanent_fetch_error("timed out"));
    }

    struct CountingOversizeClient {
        calls: Arc<AtomicUsize>,
    }

    impl RegistryHttpClient for CountingOversizeClient {
        fn get_package_metadata(
            &self,
            _package_name: &str,
        ) -> Result<HttpRegistryResponse, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(REGISTRY_BODY_TOO_LARGE_ERROR.to_owned())
        }
    }

    #[test]
    fn permanent_error_does_not_retry_and_caches_long() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = RegistryHintService::new(
            RegistryMetadataCache::empty(),
            Box::new(CountingOversizeClient {
                calls: Arc::clone(&calls),
            }),
        );

        let entry = service.fetch_package_with_retries("next", 1_000);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "permanent error must not retry"
        );
        assert!(entry.error.is_some());
        // Permanent -> cached for the 6h not-found TTL, not the 5-min transient window.
        assert_eq!(entry.retry_after, Some(1_000 + NOT_FOUND_TTL_MS));
    }
}

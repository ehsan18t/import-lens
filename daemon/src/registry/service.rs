use super::{
    cache::{self, RegistryMetadataCache},
    constants::{
        FRESH_HINT_TTL_MS, MANUAL_REFRESH_COOLDOWN_MS, MAX_ATTEMPTS, NOT_FOUND_TTL_MS,
        REGISTRY_BODY_TOO_LARGE_ERROR, REGISTRY_MANUAL_RATE_LIMIT_REQUESTS,
        REGISTRY_MAX_BACKOFF_MS, REGISTRY_RATE_LIMIT_REQUESTS, REGISTRY_RATE_LIMIT_WINDOW_MS,
        REGISTRY_RETRY_BASE_DELAY_MS, TRANSIENT_ERROR_RETRY_MS,
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
    /// Monotonic instant of the last SUCCESSFUL manual (`ForceRefresh`) fetch per
    /// package. A re-click whose entry is younger than `MANUAL_REFRESH_COOLDOWN_MS`
    /// coalesces to the cached value instead of firing a fresh request (D5). Kept
    /// in memory only (never serialized) because `Instant` is monotonic and
    /// process-local.
    manual_cooldowns: Mutex<HashMap<String, Instant>>,
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
    /// Global Retry-After floor: no reservation — manual or background — may
    /// proceed before this monotonic instant. A `429 Retry-After` pushes it
    /// forward so the whole daemon honors the registry's ask (D6), not just the
    /// rate-limited package's per-entry retry window.
    backoff_until: Instant,
}

impl RegistryRateLimiter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            window_opens_at: now,
            request_count: 0,
            // Already elapsed: no backoff in effect until a 429 installs one.
            backoff_until: now,
        }
    }

    /// Installs a GLOBAL Retry-After backoff: every subsequent reservation waits
    /// until `delay` from now elapses, regardless of window-slot availability.
    /// Only ever pushes the floor forward — a later, shorter Retry-After never
    /// shortens a longer one already in effect. Monotonic `Instant`, so a
    /// wall-clock change cannot move the floor.
    fn apply_retry_after(&mut self, delay: Duration) {
        // Clamp the server-supplied Retry-After (RB-12): the pool is only
        // `REGISTRY_REFRESH_CONCURRENCY` threads and a backing-off worker holds
        // its single-flight slot across the wait, so an unclamped `Retry-After:
        // 3600` would wedge every worker (and its waiters) for an hour,
        // uncancellable. Cap the floor at `REGISTRY_MAX_BACKOFF_MS`.
        let delay = delay.min(Duration::from_millis(REGISTRY_MAX_BACKOFF_MS));
        let until = Instant::now() + delay;
        if until > self.backoff_until {
            self.backoff_until = until;
        }
    }

    /// Reserves a rate-limit slot and returns how long the caller must sleep
    /// *after releasing the lock*. Sleeping while holding the mutex would
    /// serialize every registry worker during backoff and defeat the bounded
    /// concurrency this refresh path is built around.
    ///
    /// `request_limit` is the per-window cap to enforce: background sweeps pass
    /// the looser `REGISTRY_RATE_LIMIT_REQUESTS`, manual `ForceRefresh` the
    /// stricter `REGISTRY_MANUAL_RATE_LIMIT_REQUESTS` (D6). Both share the one
    /// window and `request_count`, so a manual fetch also counts against the
    /// background budget; it simply throttles at the lower threshold.
    ///
    /// `window_opens_at` may point into the future when a full window forced a
    /// caller to reserve the next one. Later callers must count against that
    /// reserved window (and sleep until it opens) instead of treating the
    /// reservation as a fresh open window, otherwise a burst fires everything
    /// after the boundary caller immediately.
    fn reserve_slot(&mut self, request_limit: usize) -> Option<Duration> {
        let window = Duration::from_millis(REGISTRY_RATE_LIMIT_WINDOW_MS);
        let now = Instant::now();
        let window_wait = if now >= self.window_opens_at + window {
            // The most recently reserved window has fully elapsed: start a
            // fresh one right now.
            self.window_opens_at = now;
            self.request_count = 1;
            Duration::ZERO
        } else if self.request_count < request_limit {
            self.request_count += 1;
            // Sleep until the reserved window opens; a zero wait means the
            // window is already open and the caller may proceed immediately.
            self.window_opens_at.saturating_duration_since(now)
        } else {
            // The reserved window is full for this budget: reserve the first
            // slot of the next one.
            self.window_opens_at += window;
            self.request_count = 1;
            self.window_opens_at.saturating_duration_since(now)
        };
        // The global Retry-After floor delays the reservation past whatever
        // window slot it would otherwise get.
        let wait = window_wait.max(self.backoff_until.saturating_duration_since(now));
        if wait.is_zero() { None } else { Some(wait) }
    }
}

impl RegistryHintService {
    pub fn new(cache: RegistryMetadataCache, client: Box<dyn RegistryHttpClient>) -> Self {
        Self {
            cache,
            client,
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
            manual_cooldowns: Mutex::new(HashMap::new()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            cache: RegistryMetadataCache::empty(),
            client: Box::new(NoopRegistryHttpClient),
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
            manual_cooldowns: Mutex::new(HashMap::new()),
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

    /// Serialized size in bytes of the shared npm-registry metadata snapshot, for
    /// cache-status observability (§8/X-24). Delegates to the metadata cache's
    /// single-measurement [`RegistryMetadataCache::serialized_size_bytes`]; the
    /// disabled service's empty cache reports its small empty-envelope size.
    pub fn registry_size_bytes(&self) -> u64 {
        self.cache.serialized_size_bytes()
    }

    /// Clears the ENTIRE npm-hint metadata store via D-a's authoritative,
    /// union-bypassing [`RegistryMetadataCache::clear`], so the cleared entries do
    /// not resurrect from the shared on-disk file on the next save (X-14). Wired
    /// to the `Registry` and `All` cache-remove scopes. No-op for the disabled
    /// service (its empty cache has no backing file).
    ///
    /// Concurrent-write note: a background refresh that lands between the
    /// in-memory wipe and the authoritative persist could re-seed one entry (the
    /// dirty-flag race D-a flagged). That is acceptable for a user-triggered
    /// clear — the stray entry is a fresh fetch, not stale data, and the next
    /// maintenance pass reconciles it — and not worth serializing the refresh hot
    /// path against clears.
    pub fn clear(&self) {
        // D-a: `clear` now writes the empty snapshot authoritatively and reports a
        // failed write. Surface it at the service boundary (a user-triggered clear
        // does not fail the request, but the failure must not be invisible).
        if let Err(error) = self.cache.clear() {
            logging::log_warn(
                "registry",
                format!("failed to persist cleared registry snapshot: {error}"),
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

    /// Runs the registry-store maintenance pass — 30-day retention prune plus the
    /// `max_bytes` size cap, written authoritatively (D3 + D4 / §6.1) — and sweeps the
    /// in-memory manual-refresh cooldown map (D-c). Called at daemon startup and on the
    /// periodic cache-maintenance tick. No-op for the disabled service (its empty cache
    /// has no backing file). Returns the total store entries removed (the cooldown
    /// sweep is in-memory only and not counted).
    pub fn run_maintenance(&self, now_ms: u64, max_bytes: u64) -> usize {
        self.sweep_manual_cooldowns();
        self.cache.run_maintenance(now_ms, max_bytes)
    }

    /// Prunes manual-refresh cooldown stamps whose window has fully elapsed. The map
    /// gains one `(package, Instant)` per distinct manually-refreshed package and is
    /// otherwise never pruned; once a stamp is older than `MANUAL_REFRESH_COOLDOWN_MS`
    /// it can never suppress a refresh again (`manual_cooldown_active` tests
    /// `elapsed() < cooldown`), so it is pure dead weight. Swept on the registry
    /// maintenance pass (D-c). Uses monotonic `Instant::elapsed` — never a wall clock —
    /// so a backward clock jump can neither wrongly retain nor wrongly drop a stamp.
    fn sweep_manual_cooldowns(&self) {
        let cooldown = Duration::from_millis(MANUAL_REFRESH_COOLDOWN_MS);
        if let Ok(mut cooldowns) = self.manual_cooldowns.lock() {
            cooldowns.retain(|_, last| last.elapsed() < cooldown);
        }
    }

    pub fn hint_for(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        mode: RegistryHintMode,
        now_ms: u64,
    ) -> RegistryHintLookup {
        if let Some(lookup) =
            self.cached_lookup_for_mode(package_name, installed_version, mode, now_ms)
        {
            return lookup;
        }

        let manual = mode == RegistryHintMode::ForceRefresh;
        let entry = self.fetch_package_singleflight(package_name, now_ms, manual);
        // Record the cooldown only on a definitive success (200/404 -> no error),
        // so a failed manual fetch stays retryable while the global backoff and
        // stricter manual budget throttle the retries.
        if manual && entry.error.is_none() {
            self.record_manual_fetch(package_name);
        }
        lookup_from_entry(&entry, installed_version, RegistryHintOrigin::Network)
    }

    pub(crate) fn cached_lookup_for_mode(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        mode: RegistryHintMode,
        now_ms: u64,
    ) -> Option<RegistryHintLookup> {
        if mode == RegistryHintMode::Off {
            return Some(RegistryHintLookup {
                hint: None,
                error: None,
                origin: RegistryHintOrigin::Cache,
            });
        }

        let cached = self.cache.get(package_name);
        if mode == RegistryHintMode::Cached {
            return Some(
                cached
                    .as_ref()
                    .map(|entry| {
                        lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache)
                    })
                    .unwrap_or(RegistryHintLookup {
                        hint: None,
                        error: None,
                        origin: RegistryHintOrigin::Cache,
                    }),
            );
        }

        let entry = cached.as_ref()?;
        if mode == RegistryHintMode::RefreshStale
            && (is_usable_without_fetch(entry, now_ms)
                || entry
                    .retry_after
                    .is_some_and(|retry_after| retry_after > now_ms))
        {
            return Some(lookup_from_entry(
                entry,
                installed_version,
                RegistryHintOrigin::Cache,
            ));
        }

        // D5: a manual re-click within the cooldown coalesces to the value the
        // previous manual fetch just cached — no new request, no error. Ordered
        // before single-flight and the rate limiter so an accidental double-click
        // is a pure no-op returning the cached entry.
        if mode == RegistryHintMode::ForceRefresh && self.manual_cooldown_active(package_name) {
            return Some(lookup_from_entry(
                entry,
                installed_version,
                RegistryHintOrigin::Cache,
            ));
        }

        None
    }

    /// Whether package `P` had a successful manual fetch within the last
    /// `MANUAL_REFRESH_COOLDOWN_MS`. Uses a monotonic `Instant::elapsed`, never a
    /// wall clock, so a clock jump cannot suppress or release a refresh.
    fn manual_cooldown_active(&self, package_name: &str) -> bool {
        let cooldown = Duration::from_millis(MANUAL_REFRESH_COOLDOWN_MS);
        match self.manual_cooldowns.lock() {
            Ok(cooldowns) => cooldowns
                .get(&cache::cache_key(package_name))
                .is_some_and(|last| last.elapsed() < cooldown),
            // Poisoned cooldown map: do not suppress the refresh.
            Err(_) => false,
        }
    }

    /// Stamps a successful manual fetch of `P` at the current monotonic instant so
    /// an immediate re-click coalesces to the cached value.
    fn record_manual_fetch(&self, package_name: &str) {
        if let Ok(mut cooldowns) = self.manual_cooldowns.lock() {
            cooldowns.insert(cache::cache_key(package_name), Instant::now());
        }
    }

    fn fetch_package_singleflight(
        &self,
        package_name: &str,
        now_ms: u64,
        manual: bool,
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
            Err(_) => return self.fetch_package_with_retries(package_name, now_ms, manual),
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
            let result = self.fetch_package_with_retries(package_name, now_ms, manual);
            if let Ok(mut guard) = flight.result.lock() {
                *guard = Some(result.clone());
            }
            return result;
        }

        let Ok(mut guard) = flight.result.lock() else {
            // Poisoned in-flight result: fall back to fetching directly.
            return self.fetch_package_with_retries(package_name, now_ms, manual);
        };
        while guard.is_none() {
            match flight.ready.wait(guard) {
                Ok(next) => guard = next,
                // Poisoned while waiting: fall back to fetching directly.
                Err(_) => return self.fetch_package_with_retries(package_name, now_ms, manual),
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
        manual: bool,
    ) -> RegistryPackageMetadataEntry {
        let mut last_error = None;
        let mut permanent = false;
        let mut attempts_made = 0;
        for attempt in 1..=MAX_ATTEMPTS {
            attempts_made = attempt;
            self.wait_for_rate_limit_slot(manual);
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
                    let delay_ms = response
                        .retry_after_ms
                        .unwrap_or_else(|| transient_backoff_ms(attempt));
                    // D6: honor Retry-After GLOBALLY — back off every subsequent
                    // fetch (manual and background) through the shared limiter,
                    // not just this package's per-entry retry window below.
                    self.apply_global_backoff(delay_ms);
                    let retry_after = now_ms + delay_ms;
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

    fn wait_for_rate_limit_slot(&self, manual: bool) {
        // Manual `ForceRefresh` reserves against the stricter budget; background
        // sweeps keep the looser one (D6).
        let request_limit = if manual {
            REGISTRY_MANUAL_RATE_LIMIT_REQUESTS
        } else {
            REGISTRY_RATE_LIMIT_REQUESTS
        };
        // Poisoned rate limiter: proceed without throttling rather than
        // failing the fetch.
        let wait = match self.rate_limiter.lock() {
            Ok(mut rate_limiter) => rate_limiter.reserve_slot(request_limit),
            Err(_) => None,
        };
        if let Some(delay) = wait {
            thread::sleep(delay);
        }
    }

    /// Feeds a parsed `429 Retry-After` delay into the SHARED rate limiter so it
    /// suppresses every subsequent fetch — manual and background — for that
    /// duration (D6). Locks only the limiter (never held across the network
    /// call), so it cannot deadlock with the in-flight or cooldown maps.
    fn apply_global_backoff(&self, delay_ms: u64) {
        if let Ok(mut rate_limiter) = self.rate_limiter.lock() {
            rate_limiter.apply_retry_after(Duration::from_millis(delay_ms));
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
            assert_eq!(limiter.reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS), None);
        }

        let boundary = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("boundary caller should wait for the next window");
        assert!(boundary <= window);

        // Callers arriving while the next window is reserved must also wait
        // instead of firing immediately; otherwise a burst blows through the
        // per-window request limit.
        let follower = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("followers arriving during a reserved window should also wait");
        assert!(follower <= window);
    }

    #[test]
    fn manual_budget_throttles_sooner_than_background() {
        // The stricter manual cap is a compile-time invariant of the two consts.
        const {
            assert!(REGISTRY_MANUAL_RATE_LIMIT_REQUESTS < REGISTRY_RATE_LIMIT_REQUESTS);
        }

        // Fill exactly the manual budget within one window: the next MANUAL
        // reservation is throttled to the following window.
        let mut manual = RegistryRateLimiter::new();
        for _ in 0..REGISTRY_MANUAL_RATE_LIMIT_REQUESTS {
            assert_eq!(
                manual.reserve_slot(REGISTRY_MANUAL_RATE_LIMIT_REQUESTS),
                None
            );
        }
        assert!(
            manual
                .reserve_slot(REGISTRY_MANUAL_RATE_LIMIT_REQUESTS)
                .is_some(),
            "a manual burst must throttle once it hits the stricter manual cap"
        );

        // At the very same request count, a BACKGROUND reservation is still free:
        // the looser budget has not been reached, proving manual is stricter.
        let mut background = RegistryRateLimiter::new();
        for _ in 0..REGISTRY_MANUAL_RATE_LIMIT_REQUESTS {
            assert_eq!(background.reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS), None);
        }
        assert_eq!(
            background.reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS),
            None,
            "background must not be throttled at the manual cap — it keeps the looser budget"
        );
    }

    #[test]
    fn retry_after_backs_off_shared_limiter_globally() {
        let mut limiter = RegistryRateLimiter::new();
        // A fresh window would normally admit the first request with no wait.
        limiter.apply_retry_after(Duration::from_secs(30));

        let wait = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("a Retry-After backoff must delay even the first reservation");
        // The floor is global: the wait tracks the Retry-After, not the window.
        assert!(wait > Duration::from_secs(29));
        assert!(wait <= Duration::from_secs(30));

        // A later, shorter Retry-After must not shorten the longer floor already
        // in effect.
        limiter.apply_retry_after(Duration::from_millis(1));
        let still_backed_off = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("the longer backoff must still be in force");
        assert!(still_backed_off > Duration::from_secs(29));
    }

    #[test]
    fn retry_after_is_clamped_so_a_hostile_header_cannot_wedge_the_pool() {
        // RB-12: a proxy-supplied `Retry-After: 3600` must not park the bounded
        // worker pool for an hour — the global floor is capped at
        // `REGISTRY_MAX_BACKOFF_MS`.
        let mut limiter = RegistryRateLimiter::new();
        limiter.apply_retry_after(Duration::from_secs(3600));

        let wait = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("the clamped backoff still delays the reservation");
        assert!(
            wait <= Duration::from_millis(REGISTRY_MAX_BACKOFF_MS),
            "an hour-long Retry-After must be clamped to at most {REGISTRY_MAX_BACKOFF_MS}ms, got {wait:?}"
        );
        assert!(
            wait > Duration::from_millis(REGISTRY_MAX_BACKOFF_MS) - Duration::from_secs(5),
            "the clamp must still install a ~5 min floor, not drop the backoff entirely: {wait:?}"
        );
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
    fn maintenance_sweeps_expired_manual_cooldowns() {
        let service = RegistryHintService::disabled();
        // One fresh stamp (kept) and one already past the cooldown window (swept). The
        // stale Instant is built by subtracting more than the cooldown from now;
        // checked_sub only returns None if the monotonic clock is younger than the
        // cooldown, which never happens in practice (host uptime >> 10s cooldown).
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(MANUAL_REFRESH_COOLDOWN_MS + 5_000))
            .expect("monotonic clock predates the manual-refresh cooldown");
        {
            let mut cooldowns = service.manual_cooldowns.lock().expect("cooldowns lock");
            cooldowns.insert(cache::cache_key("fresh"), Instant::now());
            cooldowns.insert(cache::cache_key("stale"), expired);
        }

        // run_maintenance drives the sweep; the empty disabled cache + u64::MAX budget
        // make the store maintenance a no-op, isolating the cooldown sweep.
        let removed = service.run_maintenance(crate::time::unix_millis_now(), u64::MAX);
        assert_eq!(removed, 0, "the empty registry store removes nothing");

        let cooldowns = service.manual_cooldowns.lock().expect("cooldowns lock");
        assert!(
            cooldowns.contains_key(&cache::cache_key("fresh")),
            "a still-fresh cooldown stamp is kept"
        );
        assert!(
            !cooldowns.contains_key(&cache::cache_key("stale")),
            "an elapsed cooldown stamp is swept"
        );
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

        let entry = service.fetch_package_with_retries("next", 1_000, false);

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

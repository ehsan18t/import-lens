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
use crate::{analysis_flight::AnalysisFlightRegistry, ipc::protocol::RegistryHint, logging};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Mutex,
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
    /// One fetch per package at a time; a concurrent request for it joins the fetch in flight.
    fetches: AnalysisFlightRegistry<RegistryPackageMetadataEntry>,
    rate_limiter: Mutex<RegistryRateLimiter>,
    /// Monotonic instant of the last successful manual (`ForceRefresh`) fetch per package, for
    /// `MANUAL_REFRESH_COOLDOWN_MS`. In memory only: `Instant` is process-local.
    manual_cooldowns: Mutex<HashMap<String, Instant>>,
}

struct RegistryRateLimiter {
    window_opens_at: Instant,
    request_count: usize,
    /// Global Retry-After floor: no reservation, manual or background, proceeds before this
    /// instant. A `429 Retry-After` pushes it forward for the whole daemon, not just one package.
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

    /// Installs a global Retry-After backoff: every later reservation waits until `delay` from now.
    /// Only pushes the floor forward; a shorter Retry-After never shortens a longer one.
    fn apply_retry_after(&mut self, delay: Duration) {
        // Clamped: see `REGISTRY_MAX_BACKOFF_MS`.
        let delay = delay.min(Duration::from_millis(REGISTRY_MAX_BACKOFF_MS));
        let until = Instant::now() + delay;
        if until > self.backoff_until {
            self.backoff_until = until;
        }
    }

    /// Reserves a rate-limit slot and returns how long the caller must sleep after releasing the
    /// lock; sleeping under the mutex would serialize every registry worker.
    ///
    /// `request_limit` is the per-window cap: `REGISTRY_RATE_LIMIT_REQUESTS` for background
    /// sweeps, the stricter `REGISTRY_MANUAL_RATE_LIMIT_REQUESTS` for `ForceRefresh`. Both share
    /// one window and `request_count`.
    ///
    /// `window_opens_at` may lie in the future after a full window made a caller reserve the next
    /// one. Later callers count against that reserved window and sleep until it opens; treating it
    /// as open would let a burst fire immediately.
    fn reserve_slot(&mut self, request_limit: usize) -> Option<Duration> {
        let window = Duration::from_millis(REGISTRY_RATE_LIMIT_WINDOW_MS);
        let now = Instant::now();
        let window_wait = if now >= self.window_opens_at + window {
            // The most recently reserved window has elapsed: start a fresh one.
            self.window_opens_at = now;
            self.request_count = 1;
            Duration::ZERO
        } else if self.request_count < request_limit {
            self.request_count += 1;
            // Zero when the reserved window is already open.
            self.window_opens_at.saturating_duration_since(now)
        } else {
            // Full for this budget: reserve the first slot of the next window.
            self.window_opens_at += window;
            self.request_count = 1;
            self.window_opens_at.saturating_duration_since(now)
        };
        // The global Retry-After floor overrides the window slot.
        let wait = window_wait.max(self.backoff_until.saturating_duration_since(now));
        if wait.is_zero() { None } else { Some(wait) }
    }
}

impl RegistryHintService {
    pub fn new(cache: RegistryMetadataCache, client: Box<dyn RegistryHttpClient>) -> Self {
        Self {
            cache,
            client,
            fetches: AnalysisFlightRegistry::new(),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
            manual_cooldowns: Mutex::new(HashMap::new()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            cache: RegistryMetadataCache::empty(),
            client: Box::new(NoopRegistryHttpClient),
            fetches: AnalysisFlightRegistry::new(),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
            manual_cooldowns: Mutex::new(HashMap::new()),
        }
    }

    /// Persists registry metadata fetched since the last flush. Called at the end of a
    /// package.json analysis or a registry-hint refresh, so writes collapse into one rewrite.
    pub fn flush(&self) {
        if let Err(error) = self.cache.flush() {
            logging::log_warn(
                "registry",
                format!("failed to persist registry metadata: {error}"),
            );
        }
    }

    /// Serialized size in bytes of the shared npm-registry metadata snapshot, for cache status.
    pub fn registry_size_bytes(&self) -> u64 {
        self.cache.serialized_size_bytes()
    }

    /// Clears the entire npm-hint metadata store authoritatively
    /// ([`RegistryMetadataCache::clear`]), for the `Registry` and `All` cache-remove scopes.
    ///
    /// A background refresh landing during the clear can leave one fresh entry behind. That is
    /// acceptable for a user-triggered clear and not worth serializing the refresh path against.
    pub fn clear(&self) {
        // A failed write does not fail the request, but must not be invisible.
        if let Err(error) = self.cache.clear() {
            logging::log_warn(
                "registry",
                format!("failed to persist cleared registry snapshot: {error}"),
            );
        }
    }

    /// Prunes registry metadata past the retention window, for the user-triggered orphan purge.
    /// Returns the count removed.
    pub fn purge_expired_metadata(&self) -> usize {
        self.cache.purge_expired(
            crate::time::unix_millis_now(),
            crate::registry::constants::REGISTRY_RETENTION_MS,
        )
    }

    /// Runs the registry-store maintenance pass (retention prune plus the `max_bytes` cap, written
    /// authoritatively) and sweeps the manual-refresh cooldown map. Called from the per-open
    /// cache-maintenance pass. Returns the store entries removed; swept cooldowns are not counted.
    pub fn run_maintenance(&self, now_ms: u64, max_bytes: u64) -> usize {
        self.sweep_manual_cooldowns();
        self.cache.run_maintenance(now_ms, max_bytes)
    }

    /// Prunes cooldown stamps whose window has elapsed: such a stamp can never suppress a refresh
    /// again, and nothing else prunes the map.
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
        // Only a definitive success (200/404) starts the cooldown, so a failed manual fetch
        // stays retryable.
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
        let cached = self.cache.get(package_name);
        let manual_cooldown_active = mode == RegistryHintMode::ForceRefresh
            && cached.is_some()
            && self.manual_cooldown_active(package_name);
        cached_lookup_from_entry(
            cached.as_ref(),
            installed_version,
            mode,
            now_ms,
            manual_cooldown_active,
        )
    }

    pub(crate) fn cached_lookups_for_mode(
        &self,
        targets: &[(&str, Option<&str>)],
        mode: RegistryHintMode,
        now_ms: u64,
    ) -> Vec<Option<RegistryHintLookup>> {
        if mode == RegistryHintMode::Off {
            return targets
                .iter()
                .map(|_| {
                    Some(RegistryHintLookup {
                        hint: None,
                        error: None,
                        origin: RegistryHintOrigin::Cache,
                    })
                })
                .collect();
        }

        let cached_entries = self
            .cache
            .get_many(targets.iter().map(|(package_name, _)| *package_name));
        let manual_cooldown_hits = if mode == RegistryHintMode::ForceRefresh {
            self.manual_cooldown_hits(targets.iter().map(|(package_name, _)| *package_name))
        } else {
            vec![false; targets.len()]
        };

        targets
            .iter()
            .zip(cached_entries.iter())
            .zip(manual_cooldown_hits.iter())
            .map(
                |(((.., installed_version), cached), manual_cooldown_active)| {
                    cached_lookup_from_entry(
                        cached.as_ref(),
                        *installed_version,
                        mode,
                        now_ms,
                        *manual_cooldown_active,
                    )
                },
            )
            .collect()
    }

    /// Whether the package had a successful manual fetch within `MANUAL_REFRESH_COOLDOWN_MS`.
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

    fn manual_cooldown_hits<'a>(
        &self,
        package_names: impl IntoIterator<Item = &'a str>,
    ) -> Vec<bool> {
        let keys: Vec<_> = package_names.into_iter().map(cache::cache_key).collect();
        let cooldown = Duration::from_millis(MANUAL_REFRESH_COOLDOWN_MS);
        match self.manual_cooldowns.lock() {
            Ok(cooldowns) => keys
                .iter()
                .map(|key| {
                    cooldowns
                        .get(key)
                        .is_some_and(|last| last.elapsed() < cooldown)
                })
                .collect(),
            // Poisoned cooldown map: do not suppress any refresh.
            Err(_) => vec![false; keys.len()],
        }
    }

    /// Stamps a successful manual fetch so an immediate re-click coalesces to the cached value.
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
        // Registry metadata has no cache generation, so every fetch of a package shares one.
        // A leader that panics leaves its followers to elect a replacement and fetch again.
        self.fetches
            .run_or_join(cache::cache_key(package_name), 0, || {
                self.fetch_package_with_retries(package_name, now_ms, manual)
            })
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
                    // Capped once, for both the global floor and this package's window: a
                    // hostile or broken proxy's Retry-After must not outlast the cap either way.
                    let delay_ms = response
                        .retry_after_ms
                        .unwrap_or_else(|| transient_backoff_ms(attempt))
                        .min(REGISTRY_MAX_BACKOFF_MS);
                    // Honored globally through the shared limiter, not only for this package.
                    self.apply_global_backoff(delay_ms);
                    let retry_after = now_ms.saturating_add(delay_ms);
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

    /// Test-only: seeds the cache directly. Public because integration tests cannot see
    /// `#[cfg(test)]` items.
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
        let request_limit = if manual {
            REGISTRY_MANUAL_RATE_LIMIT_REQUESTS
        } else {
            REGISTRY_RATE_LIMIT_REQUESTS
        };
        // Poisoned rate limiter: proceed unthrottled rather than fail the fetch.
        let wait = match self.rate_limiter.lock() {
            Ok(mut rate_limiter) => rate_limiter.reserve_slot(request_limit),
            Err(_) => None,
        };
        if let Some(delay) = wait {
            thread::sleep(delay);
        }
    }

    /// Feeds a `429 Retry-After` delay into the shared rate limiter, delaying every later fetch.
    /// Locks only the limiter, never across the network call.
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

fn cached_lookup_from_entry(
    cached: Option<&RegistryPackageMetadataEntry>,
    installed_version: Option<&str>,
    mode: RegistryHintMode,
    now_ms: u64,
    manual_cooldown_active: bool,
) -> Option<RegistryHintLookup> {
    if mode == RegistryHintMode::Off {
        return Some(RegistryHintLookup {
            hint: None,
            error: None,
            origin: RegistryHintOrigin::Cache,
        });
    }

    if mode == RegistryHintMode::Cached {
        return Some(
            cached
                .map(|entry| lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache))
                .unwrap_or(RegistryHintLookup {
                    hint: None,
                    error: None,
                    origin: RegistryHintOrigin::Cache,
                }),
        );
    }

    let entry = cached?;
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

    // A manual re-click within the cooldown coalesces to the cached value.
    if mode == RegistryHintMode::ForceRefresh && manual_cooldown_active {
        return Some(lookup_from_entry(
            entry,
            installed_version,
            RegistryHintOrigin::Cache,
        ));
    }

    None
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
    // The abbreviated packument has no per-version `time` map; its top-level `modified`
    // reflects the latest publish in the common case.
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
    // 429 never reaches here: it has its own arm, which honors Retry-After.
    status == 408 || status == 425 || status >= 500
}

/// A permanent fetch failure skips the remaining attempts and is cached for the not-found TTL,
/// not the 5-minute transient window. Currently only an oversize body
/// (`REGISTRY_BODY_TOO_LARGE_ERROR`).
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

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

        // Callers arriving while the next window is reserved must also wait.
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

        // Fill the manual budget: the next manual reservation is throttled.
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

        // At the same count, a background reservation is still free.
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

        // A later, shorter Retry-After must not shorten the floor.
        limiter.apply_retry_after(Duration::from_millis(1));
        let still_backed_off = limiter
            .reserve_slot(REGISTRY_RATE_LIMIT_REQUESTS)
            .expect("the longer backoff must still be in force");
        assert!(still_backed_off > Duration::from_secs(29));
    }

    #[test]
    fn retry_after_is_clamped_so_a_hostile_header_cannot_wedge_the_pool() {
        // A `Retry-After: 3600` must not park the pool for an hour.
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
        // One fresh stamp (kept) and one past the cooldown window (swept).
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(MANUAL_REFRESH_COOLDOWN_MS + 5_000))
            .expect("monotonic clock predates the manual-refresh cooldown");
        {
            let mut cooldowns = service.manual_cooldowns.lock().expect("cooldowns lock");
            cooldowns.insert(cache::cache_key("fresh"), Instant::now());
            cooldowns.insert(cache::cache_key("stale"), expired);
        }

        // The empty cache and u64::MAX budget isolate the cooldown sweep.
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
    fn batched_cached_lookup_preserves_refresh_and_manual_cooldown_policy() {
        let service = RegistryHintService::disabled();
        let now_ms = FRESH_HINT_TTL_MS + 2_000;
        service
            .write_metadata_for_tests("fresh", "2.0.0", now_ms - 1_000)
            .expect("write fresh");
        service
            .write_metadata_for_tests("stale", "2.0.0", 0)
            .expect("write stale");
        service
            .write_metadata_for_tests("manual-cached", "2.0.0", 1_000)
            .expect("write manual cached");
        service
            .write_metadata_for_tests("manual-no-cooldown", "2.0.0", 1_000)
            .expect("write manual no cooldown");
        service.record_manual_fetch("manual-cached");

        let refresh = service.cached_lookups_for_mode(
            &[
                ("fresh", Some("1.0.0")),
                ("stale", Some("1.0.0")),
                ("missing", Some("1.0.0")),
            ],
            RegistryHintMode::RefreshStale,
            now_ms,
        );

        assert_eq!(refresh.len(), 3);
        assert_eq!(
            refresh[0].as_ref().map(|lookup| lookup.origin),
            Some(RegistryHintOrigin::Cache)
        );
        assert!(
            refresh[1].is_none(),
            "stale cached refresh target must still go to the network pool"
        );
        assert!(
            refresh[2].is_none(),
            "missing refresh target must still go to the network pool"
        );

        let force = service.cached_lookups_for_mode(
            &[
                ("manual-cached", Some("1.0.0")),
                ("manual-no-cooldown", Some("1.0.0")),
                ("missing", Some("1.0.0")),
            ],
            RegistryHintMode::ForceRefresh,
            1_100,
        );

        assert_eq!(force.len(), 3);
        assert_eq!(
            force[0].as_ref().map(|lookup| lookup.origin),
            Some(RegistryHintOrigin::Cache)
        );
        assert!(
            force[1].is_none(),
            "manual refresh without an active cooldown must go to the network pool"
        );
        assert!(
            force[2].is_none(),
            "missing manual refresh target must go to the network pool"
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

pub const FRESH_HINT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const NOT_FOUND_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const TRANSIENT_ERROR_RETRY_MS: u64 = 5 * 60 * 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 3_000;
pub const MAX_ATTEMPTS: usize = 3;
pub const REGISTRY_REFRESH_CONCURRENCY: usize = 4;
pub const REGISTRY_RATE_LIMIT_REQUESTS: usize = 20;
pub const REGISTRY_RATE_LIMIT_WINDOW_MS: u64 = 1_000;
pub const REGISTRY_RETRY_BASE_DELAY_MS: u64 = 100;

/// Upper bound on a `429 Retry-After` global backoff (decision-log D4). A backing-off worker
/// holds its package's single-flight slot across the wait and the pool has only
/// `REGISTRY_REFRESH_CONCURRENCY` threads, so an unclamped `Retry-After: 3600` would wedge every
/// worker for an hour, uncancellable. 5 min matches `TRANSIENT_ERROR_RETRY_MS`.
pub const REGISTRY_MAX_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// Per-window request cap for manual `ForceRefresh` fetches, stricter than the background
/// `REGISTRY_RATE_LIMIT_REQUESTS` budget; both share `REGISTRY_RATE_LIMIT_WINDOW_MS` on one
/// limiter. Must stay below `REGISTRY_RATE_LIMIT_REQUESTS` and above `MAX_ATTEMPTS`, so a single
/// retrying manual fetch never self-throttles.
pub const REGISTRY_MANUAL_RATE_LIMIT_REQUESTS: usize = 5;

/// Minimum spacing between manual `ForceRefresh` fetches of the same package. A re-click within
/// the window coalesces to the value the previous fetch cached, with no request and no error.
/// Measured with a monotonic `Instant`, so a wall-clock jump cannot change it.
pub const MANUAL_REFRESH_COOLDOWN_MS: u64 = 10_000;
pub const REGISTRY_CACHE_FILE_NAME: &str = "registry-metadata.json";
/// How long a registry entry is retained before the retention prune drops it. Distinct from the
/// 6h refetch TTL. Enforced on the per-open maintenance pass and by the orphan purge.
pub const REGISTRY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Default byte budget for the shared registry metadata store, enforced on the maintenance pass
/// by evicting oldest-`updated_at` entries once the serialized snapshot exceeds it. The store
/// holds one small record per package, so 32 MiB only bounds pathological growth. Matches the
/// `importLens.registryCacheMaxSizeMB` default; Hello carries the user's value.
pub const REGISTRY_CACHE_MAX_SIZE_BYTES: u64 = 32 * 1024 * 1024;

/// Upper bound for a single npm packument body. High-churn packages exceed ureq's 10 MiB default
/// (`next`'s abbreviated packument is ~25 MB); 64 MB leaves headroom. Larger bodies are a
/// permanent fetch failure (`is_permanent_fetch_error`). Only extracted metadata is cached; the
/// body is held during parse (peak ~2-3x its size), bounded by `REGISTRY_REFRESH_CONCURRENCY`.
pub const MAX_REGISTRY_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Stable marker the registry client emits when a body exceeds `MAX_REGISTRY_BODY_BYTES`, so
/// permanent-failure classification never depends on ureq's error wording.
pub const REGISTRY_BODY_TOO_LARGE_ERROR: &str = "registry response body exceeds size limit";

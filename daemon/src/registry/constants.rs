pub const FRESH_HINT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const NOT_FOUND_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const TRANSIENT_ERROR_RETRY_MS: u64 = 5 * 60 * 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 3_000;
pub const MAX_ATTEMPTS: usize = 3;
pub const REGISTRY_REFRESH_CONCURRENCY: usize = 4;
pub const REGISTRY_RATE_LIMIT_REQUESTS: usize = 20;
pub const REGISTRY_RATE_LIMIT_WINDOW_MS: u64 = 1_000;
pub const REGISTRY_RETRY_BASE_DELAY_MS: u64 = 100;
pub const REGISTRY_CACHE_FILE_NAME: &str = "registry-metadata.json";
/// How long a registry entry is retained before the orphan-purge action drops
/// it. Distinct from the 6h refetch TTL: this bounds retention, that bounds
/// refetch.
pub const REGISTRY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Upper bound for a single npm packument body. npm's abbreviated ("corgi")
/// metadata for very high-churn packages exceeds ureq's 10 MiB default
/// `read_to_string` cap — `next`'s corgi packument measures ~25 MB (its full
/// packument ~31 MB) because its `versions` map holds thousands of releases.
/// 64 MB clears that with headroom; bodies larger than this are treated as a
/// permanent fetch failure (see `is_permanent_fetch_error`). Only the extracted
/// metadata (latest_version, latest_published_at, deprecated_versions) is cached,
/// never the multi-MB body, so the on-disk cache stays small. The body is held
/// transiently during parse (peak ~2-3x its size in the serde_json value tree),
/// bounded by `REGISTRY_REFRESH_CONCURRENCY`.
pub const MAX_REGISTRY_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Stable marker the registry client emits when a response body exceeds
/// `MAX_REGISTRY_BODY_BYTES`. Classifying permanent failures against this marker
/// (rather than ureq's internal error text) keeps the behavior stable across
/// ureq upgrades that might reword their message.
pub const REGISTRY_BODY_TOO_LARGE_ERROR: &str = "registry response body exceeds size limit";

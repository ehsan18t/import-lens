pub const FRESH_HINT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const NOT_FOUND_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const TRANSIENT_ERROR_RETRY_MS: u64 = 5 * 60 * 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 3_000;
pub const MAX_ATTEMPTS: usize = 3;
pub const REGISTRY_REFRESH_CONCURRENCY: usize = 4;
pub const REGISTRY_RATE_LIMIT_REQUESTS: usize = 20;
pub const REGISTRY_RATE_LIMIT_WINDOW_MS: u64 = 1_000;
pub const REGISTRY_RETRY_BASE_DELAY_MS: u64 = 100;

/// Upper bound on a `429 Retry-After` global backoff (RB-12). The registry pool
/// is only `REGISTRY_REFRESH_CONCURRENCY` threads and a backing-off worker holds
/// its package's single-flight slot across the wait, so an unclamped
/// server-supplied `Retry-After: 3600` would wedge every worker (and the waiters
/// behind them) for an hour, uncancellable. Cap it so a hostile/misconfigured
/// proxy cannot; 5 min still honors a genuine rate-limit ask (matches
/// `TRANSIENT_ERROR_RETRY_MS`).
pub const REGISTRY_MAX_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// Per-window request cap for MANUAL `ForceRefresh` fetches — deliberately
/// stricter than the background `REGISTRY_RATE_LIMIT_REQUESTS` budget (D6 /
/// §6.1). Both budgets share the one `REGISTRY_RATE_LIMIT_WINDOW_MS` window on
/// the shared limiter; a user mashing the refresh action across many packages is
/// throttled after this many fetches per window, while the daemon's own
/// background staleness sweeps keep the looser cap. Must stay below
/// `REGISTRY_RATE_LIMIT_REQUESTS` (and comfortably above `MAX_ATTEMPTS`, so a
/// single retrying manual fetch never self-throttles).
pub const REGISTRY_MANUAL_RATE_LIMIT_REQUESTS: usize = 5;

/// Minimum spacing between manual `ForceRefresh` network fetches of the SAME
/// package (D5 / §6.1). A re-click within this monotonic window coalesces to the
/// value the previous manual fetch just cached — no new request, no error — so an
/// accidental double-click cannot double-hit the registry. Measured with a
/// monotonic `Instant`, so a wall-clock jump can neither shrink nor extend it.
pub const MANUAL_REFRESH_COOLDOWN_MS: u64 = 10_000;
pub const REGISTRY_CACHE_FILE_NAME: &str = "registry-metadata.json";
/// How long a registry entry is retained before the retention prune drops it.
/// Distinct from the 6h refetch TTL: this bounds retention, that bounds refetch.
/// Enforced automatically on the maintenance pass (daemon startup + periodic
/// tick), not only from the user-triggered orphan purge.
pub const REGISTRY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Default byte budget for the shared registry metadata store, enforced on the
/// maintenance pass by evicting oldest-`updated_at` entries once the serialized
/// snapshot exceeds it (D4 / §6.1). Modest by design: the store holds one small
/// metadata record per npm package looked at across every workspace, so 32 MiB
/// is far beyond any realistic working set while still bounding pathological
/// growth. Mirrors the user setting `importLens.registryCacheMaxSizeMB`
/// (default 32); threading a user override through the Hello IPC is a small
/// follow-up, so today the daemon always applies this default.
pub const REGISTRY_CACHE_MAX_SIZE_BYTES: u64 = 32 * 1024 * 1024;

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

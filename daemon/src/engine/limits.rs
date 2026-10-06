//! Product limits enforced by the native Rolldown plugin.

use std::sync::LazyLock;

pub const MAX_GRAPH_MODULES: usize = 2_000;
pub const MAX_MODULE_SOURCE_BYTES: usize = 20 * 1024 * 1024;

/// Default total-source ceiling for one module graph.
pub const DEFAULT_MAX_GRAPH_SOURCE_BYTES: usize = 100 * 1024 * 1024;

/// Overrides `MAX_GRAPH_SOURCE_BYTES` for tests, so the total-source breach runs without
/// writing >100 MiB of fixtures. An environment variable rather than a `#[cfg(test)]`
/// constant, because integration tests link the library compiled without `cfg(test)`.
///
/// Never set in production: nothing in the shipped extension sets it.
const MAX_GRAPH_SOURCE_BYTES_ENV: &str = "IMPORT_LENS_MAX_GRAPH_SOURCE_BYTES";

pub static MAX_GRAPH_SOURCE_BYTES: LazyLock<usize> = LazyLock::new(|| {
    std::env::var(MAX_GRAPH_SOURCE_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_GRAPH_SOURCE_BYTES)
});

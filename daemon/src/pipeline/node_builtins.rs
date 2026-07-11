/// Sorted for binary search and shared by document filtering and the engine's
/// exact external-specifier list.
pub const NODE_BUILTIN_MODULES: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "timers",
    "timers/promises",
    "tls",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

pub fn is_node_builtin_specifier(specifier: &str) -> bool {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    NODE_BUILTIN_MODULES.binary_search(&bare).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{NODE_BUILTIN_MODULES, is_node_builtin_specifier};

    #[test]
    fn builtin_list_stays_sorted_and_matches_node_prefixes() {
        assert!(
            NODE_BUILTIN_MODULES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(is_node_builtin_specifier("fs/promises"));
        assert!(is_node_builtin_specifier("node:fs/promises"));
        assert!(!is_node_builtin_specifier("not-a-node-builtin"));
    }
}

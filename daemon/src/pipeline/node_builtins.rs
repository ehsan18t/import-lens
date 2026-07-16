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
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Builtins Node exposes ONLY under the `node:` prefix.
///
/// These are deliberately not reachable bare, so that `import x from "test"` keeps
/// meaning the npm package named `test` — treating the bare form as a builtin would
/// externalize a real dependency and report its size as zero. Sorted for binary
/// search, and stored with the prefix because the bare spelling must never match.
pub const NODE_PREFIX_ONLY_MODULES: &[&str] = &[
    "node:sea",
    "node:sqlite",
    "node:test",
    "node:test/reporters",
];

pub fn is_node_builtin_specifier(specifier: &str) -> bool {
    if NODE_PREFIX_ONLY_MODULES.binary_search(&specifier).is_ok() {
        return true;
    }
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    NODE_BUILTIN_MODULES.binary_search(&bare).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{NODE_BUILTIN_MODULES, NODE_PREFIX_ONLY_MODULES, is_node_builtin_specifier};

    /// Both lists are binary-searched, so an unsorted entry is not a style problem —
    /// it is a builtin that stops being recognized.
    #[test]
    fn builtin_list_stays_sorted_and_matches_node_prefixes() {
        assert!(
            NODE_BUILTIN_MODULES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(
            NODE_PREFIX_ONLY_MODULES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(is_node_builtin_specifier("fs/promises"));
        assert!(is_node_builtin_specifier("node:fs/promises"));
        assert!(!is_node_builtin_specifier("not-a-node-builtin"));
    }

    /// `node:test` and friends have no bare form. If the bare spelling matched, an
    /// npm package actually named `test` or `sqlite` would be externalized and
    /// reported as weighing nothing.
    #[test]
    fn prefix_only_builtins_never_match_their_bare_spelling() {
        for module in NODE_PREFIX_ONLY_MODULES {
            assert!(is_node_builtin_specifier(module), "{module}");
            let bare = module
                .strip_prefix("node:")
                .expect("prefix-only modules are stored with their prefix");
            assert!(
                !is_node_builtin_specifier(bare),
                "{bare} must stay resolvable as an npm package"
            );
        }
    }
}

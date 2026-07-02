---
name: rust-concurrent-cache
description: "Two-tier caching architecture using papaya (lock-free pin) and redb v4 with schema versioning. Use when implementing the daemon's cache/ module (FR-025, FR-026, FR-026a)."
---

# Instructions

The daemon uses a two-tier cache for import size results:

- memory tier: `papaya::HashMap`
- persistent tier: per-project `redb` v4 database shards

Computed cache entries are write-through: successful cacheable results are inserted into `redb` and `papaya` on the hot path. Shutdown only needs to flush pending recency touches, not rewrite the whole memory tier to disk.

## 1. Cache Key Format

Cache keys for both tiers use the structured v3 identity format:

```text
v3:<hex-msgpack-cache-identity>
```

The MessagePack payload is `CacheIdentityV3` and includes:

- `analyzer_version`
- full import `specifier`
- root `package_name`
- `package_version`
- optional canonical `package_root`
- optional canonical `entry_path`
- `runtime`
- `import_kind`
- sorted and deduplicated `named_exports`
- manifest and entry fingerprints when available

Do not reintroduce the legacy `<package>@<version>::exports` key format. It collides across runtime, import kind, subpath, resolver output, and file freshness dimensions.

## 2. Memory Tier: papaya

Use `papaya::HashMap<String, CachedImport>` for the memory tier. Pin before iterating, reading, inserting, or removing.

```rust
let memory = self.memory.pin();
if let Some(cached) = memory.get(key) {
    return Some(cached.clone());
}
memory.insert(key.to_owned(), cached);
```

Do not use `dashmap` for daemon cache state. `dashmap` can deadlock when references are held across nested operations; this workload is read-heavy and fits papaya's pinning model.

## 3. Persistent Tier: redb v4

Each project gets a stable cache shard directory under the extension-owned cache base. The shard id is derived from the normalized analysis root, so multi-root workspaces and loose files do not share one growing database.

Each shard contains:

- `importlens.redb`
- `importlens-project-cache.json`

The JSON metadata records `shard_id`, `project_root`, `normalized_root`, and `last_used_millis` for cache management and LRU cleanup. Loaded shards update `last_used_millis` in memory on every access, but JSON writes should be throttled to avoid repeated filesystem writes during parallel import batches.

## 4. redb Schema Versioning

The current redb schema version is `4`.

Tables:

```rust
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const RECENTS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cache_recents");
```

- `metadata["schema_version"]` stores `4`.
- `size_cache` maps v3 cache keys to MessagePack cache envelopes.
- `cache_recents` maps v3 cache keys to last-used Unix milliseconds.

On open, read `schema_version` before loading entries. If the value is missing or mismatched, delete the database file, create a fresh database with schema version `4`, and log a warning. If the file is corrupted, use the same recreate path; if recreation fails, skip only the persistent tier and continue in memory.

## 5. Stored Value Shape

`size_cache` values are MessagePack cache envelopes containing:

- analyzer version
- public `ImportResult`
- decoded package identity when available
- dependency fingerprints
- full module contributions for shared-byte accounting

Normalize `cache_hit` to `false` before writing. Set `cache_hit` to `true` only when serving a memory or disk hit.

## 6. Recency

Every disk insert updates `cache_recents` immediately. Memory and disk hits may batch recency touches in memory to avoid a redb write on every hot cache hit.

Rules:

- Flush pending recency touches on drop and graceful shutdown.
- A failed pending-touch flush must log and requeue the touches.
- Recent-key selection should avoid sorting every row when only the top N keys are needed.
- Opening disk-only shards for package invalidation must use recent preload limit `0`.

## 7. Invalidation

`CacheInvalidate` invalidates one package across all loaded project shards and disk-only shards. Remove matching keys from both `papaya` and `redb`.

`NodeModulesChanged` carries 1 through 20 changed `node_modules/**/package.json` paths. Derive package names from the paths and invalidate those packages. If the message contains more than 20 paths or any path cannot be mapped to a package, call `invalidate_all`.

`CacheInvalidateAll` clears all project shards and the module graph cache.

For disk-only shard invalidation, avoid recent-entry preload and avoid opening the recents table once per removed key.

## 8. Cacheability

Only cache successful results that are stable for the cache identity. Results with request-specific export diagnostics are not cacheable.

Malformed or versionless manifest fallback results are intentionally not cached. They use approximate directory sizing and cannot be cheaply proven fresh until the daemon has a directory-wide fingerprint or package file index.

## Rules

- Do not use `dashmap`, `sled`, or `num_cpus`.
- Use `std::thread::available_parallelism()` for thread counts.
- Keep cache folders under extension-owned storage, never inside the user's project tree.
- Keep schema, key format, and SRS updates together when cache persistence changes.

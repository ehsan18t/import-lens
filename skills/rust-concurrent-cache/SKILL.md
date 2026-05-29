---
name: rust-concurrent-cache
description: "Two-tier caching architecture using papaya (lock-free pin) and redb v4 with schema versioning. Use when implementing the daemon's cache/ module (FR-025, FR-026, FR-026a)."
---

# Instructions

The project uses a strict two-tier caching strategy to avoid re-parsing massive `node_modules` ASTs.

## 1. Cache Key Format

Cache keys MUST follow this exact string format for both stores:
`<package>@<version>::<export1>,<export2>,...<exportN>`

- The exports string MUST be sorted lexicographically before joining.
- A default import is keyed as `default`.
- A namespace import is keyed as `*`.
- A dynamic import is keyed as `dynamic`.

Examples:

```
lodash-es@4.17.21::debounce,throttle
lodash-es@4.17.21::*
react@18.3.1::default
date-fns@3.6.0::dynamic
@babel/core@7.24.0::default
@tanstack/react-query@5.28.0::useMutation,useQuery
```

## 2. Memory Tier: `papaya` (v0.2.4)

`papaya` is lock-free, avoiding the deadlocks present in `dashmap`. It requires a pinning API for memory reclamation.

```rust
use papaya::HashMap;

let map = HashMap::new();

// You MUST pin the thread context to perform lookups or insertions
let pin = map.pin();

if let Some(val) = map.get(&key, &pin) {
    return val;
} else {
    map.insert(key, computed_val, &pin);
}
```

## 3. Persistent Tier: `redb` (v4.0.0)

> [!IMPORTANT]
> The SRS pins `redb` at v4.0.0 (`^4`), NOT v3.x. Do not use `redb` v3.

If `enable_disk_cache` is true, results are flushed to disk using `redb`. `redb` provides a stable ACID architecture without a C FFI requirement (unlike SQLite).

```rust
use redb::{Database, TableDefinition};

const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
```

## 4. Schema Versioning (FR-026a) — CRITICAL

The `redb` database MUST include a metadata table with a `schema_version` integer (initially `1`).

On startup, the daemon must:

1. Open the database.
2. Read `schema_version` from the metadata table.
3. If the key is missing or the value does not match the current daemon's expected version, **delete the database file and create a fresh empty database**.
4. Log a warning when a migration wipe occurs.

```rust
const EXPECTED_SCHEMA_VERSION: u64 = 1;

fn open_or_create_db(path: &Path) -> Database {
    match Database::open(path) {
        Ok(db) => {
            let rtx = db.begin_read().unwrap();
            let table = rtx.open_table(META_TABLE);
            match table {
                Ok(t) => {
                    if let Some(v) = t.get("schema_version").ok().flatten() {
                        let version: u64 = rmp_serde::from_slice(v.value()).unwrap_or(0);
                        if version == EXPECTED_SCHEMA_VERSION {
                            return db;
                        }
                    }
                    // Schema mismatch — wipe and recreate
                    drop(rtx);
                    drop(db);
                    std::fs::remove_file(path).ok();
                }
                Err(_) => {
                    drop(rtx);
                    drop(db);
                    std::fs::remove_file(path).ok();
                }
            }
        }
        Err(_) => {
            // Corrupted or incompatible — delete
            std::fs::remove_file(path).ok();
        }
    }
    create_fresh_db(path)
}
```

## 5. CachedResult Schema

The value stored in both `papaya` and `redb` must be a MessagePack-encoded struct:

```rust
#[derive(Serialize, Deserialize)]
struct CachedResult {
    raw_bytes: u64,
    minified_bytes: u64,
    gzip_bytes: u64,
    brotli_bytes: u64,
    zstd_bytes: u64,
    side_effects: bool,
    truly_treeshakeable: bool,
    is_cjs: bool,
    computed_at: u64,        // Unix timestamp in seconds
}
```

## Rules

- **Do not** use `dashmap` or `sled`. They are explicitly forbidden (§9.4.4).
- **Do not** use `num_cpus`. It is banned. Use `std::thread::available_parallelism()`.
- On `CacheInvalidate` IPC, you must clear entries belonging to that specific package from BOTH `papaya` and `redb`.
- Suppress OS permission errors gracefully: if the VS Code global storage directory is inaccessible, the daemon skips `redb` and runs solely on `papaya`.
- The self-recycle threshold for `papaya` is **200,000 entries** (NFR-004a). See the `rust-daemon-lifecycle` skill.

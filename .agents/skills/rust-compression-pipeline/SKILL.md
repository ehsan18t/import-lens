---
name: rust-compression-pipeline
description: "Parallel multi-format string compression using nested rayon::join — gzip (flate2), brotli, and zstd. Use when implementing the daemon's compress.rs module (FR-020)."
---

# Instructions

After we generate a minified string, we must estimate its final size under standard web compression metrics simultaneously to reduce latency.

## 1. The Compression Crates

You must use `flate2` (v1.1.x), `brotli` (v8.0.x), and `zstd` (v0.13.x).

- **Gzip**: Level 6
- **Brotli**: Level 4
- **Zstd**: Level 3

## 2. The Rayon Constraint

We execute these three blocking compression passes concurrently on the background pool.

> [!WARNING]
> `rayon::join` accepts **exactly two closures**. The SRS strictly requires that for three parallel tasks, you must nest a second `rayon::join` inside the first. DO NOT pass three closures to `join()`.

```rust
use brotli;
use flate2::write::GzEncoder;
use zstd;

// Let string be minified_string ...

let (gzip_bytes, (brotli_bytes, zstd_bytes)) = rayon::join(
    || {
        // Gzip logic (Level 6)
        // ...
        0 // return bytes
    },
    || {
        rayon::join(
            || {
                // Brotli logic (Level 4)
                // ...
                0
            },
            || {
                // Zstd logic (Level 3)
                // ...
                0
            }
        )
    }
);
```

## Rules

- The Rayon global thread pool must be sized to `max(1, available_parallelism - 2)` — NOT 1x logical cores. This leaves headroom for VS Code's renderer and extension host. Use `std::thread::available_parallelism()` from the stdlib; do NOT use the `num_cpus` crate (it is banned — see §9.4.4 in the SRS).
- All strings must be UTF-8 before compression takes place.

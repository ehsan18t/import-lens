---
name: rust-daemon-lifecycle
description: "Daemon startup, self-recycle for memory fragmentation (NFR-004a), and graceful shutdown implementation. Use when implementing daemon/src/lifecycle.rs and daemon/src/main.rs."
---

# Instructions

The daemon must start quickly, handle shutdown gracefully, and self-recycle to prevent long-term memory fragmentation.

## 1. Startup Sequence

The daemon's `main()` function must:

1. Parse the socket path from command-line arguments.
2. Initialize the Rayon global thread pool:
   ```rust
   rayon::ThreadPoolBuilder::new()
       .num_threads(
           std::thread::available_parallelism()
               .map(|n| n.get().saturating_sub(2).max(1))
               .unwrap_or(1)
       )
       .build_global()
       .unwrap();
   ```
3. Open `redb` database from the path provided in the HelloMessage (after receiving it), verifying schema version (FR-026a).
4. Load all entries from `redb` into `papaya` (pre-warm).
5. Begin listening on the socket.
6. Be ready to accept connections within **500ms** (NFR-005).

> [!IMPORTANT]
> Do NOT use `num_cpus` for thread pool sizing. It is banned. Use `std::thread::available_parallelism()`.

## 2. Self-Recycle (NFR-004a) — CRITICAL

Developers leave VS Code open for days or weeks. Even a well-behaved Rust process accumulates allocator fragmentation over time. The daemon must monitor two conditions and gracefully restart itself when **either** is met:

### Condition A: Idle Timer

- The daemon has been continuously running for more than **4 hours** AND
- No active request has been received in the last **15 minutes**

### Condition B: Cache Size

- The `papaya` in-memory cache exceeds **200,000 entries** (approximately 80–100 MB at ~500 bytes per entry, consistent with the 100 MB idle memory limit in NFR-004)

### Graceful Restart Procedure

1. Flush all in-memory `papaya` entries to `redb`.
2. Close the `redb` database cleanly.
3. Remove the socket file (Unix) or release the named pipe (Windows).
4. Exit cleanly with exit code `0` (no signal kill).
5. The extension host's watchdog (FR-015) detects the clean exit and respawns the daemon.

```rust
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};

static LAST_REQUEST_TIME: AtomicU64 = AtomicU64::new(0);
static STARTUP_TIME: AtomicU64 = AtomicU64::new(0);

fn should_self_recycle(cache_entry_count: usize) -> bool {
    let now = unix_timestamp_secs();
    let uptime = now - STARTUP_TIME.load(Ordering::Relaxed);
    let idle_time = now - LAST_REQUEST_TIME.load(Ordering::Relaxed);

    // Condition A: 4 hours uptime + 15 min idle
    let condition_a = uptime > 4 * 3600 && idle_time > 15 * 60;

    // Condition B: Cache exceeds 200,000 entries
    let condition_b = cache_entry_count > 200_000;

    condition_a || condition_b
}
```

### Silent Operation

The restart MUST be silent to the user. No status bar change or notification unless the restart fails. The extension host's FR-015 watchdog treats exit code `0` as a normal recycle.

## 3. Memory Constraints (NFR-004)

- **Idle with cache populated**: ≤ 100 MB resident memory
- **Active computation (batch of 20 imports)**: ≤ 400 MB peak
- The OXC allocator (`oxc_allocator::Allocator`) is arena-based and drops all memory when the allocator is dropped. Ensure each pipeline run creates and drops its own allocator.

## 4. Graceful Shutdown (daemon side)

On receiving the `Shutdown` IPC message:

1. Stop accepting new connections.
2. Finish any in-flight computations.
3. Flush `papaya` to `redb`.
4. Close `redb`.
5. Remove socket file.
6. Exit with code `0`.

The daemon must complete this within 5 seconds or the extension host will escalate to SIGTERM/SIGKILL.

## Rules

- Do NOT call `std::process::abort()` or `panic!()` for lifecycle events. Always exit cleanly.
- The self-recycle timer should run on a dedicated Tokio task, not on the Rayon pool.
- Log all recycle events at `info` level for diagnostics.

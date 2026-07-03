# Perf Plan C — Persistence Write Amplification (DF-4, DF-5)

> **STATUS: plan ready; execute task-by-task, one commit per task.** Third grouped follow-up from `2026-07-03-daemon-review-fixes.md` (Part C). Sequence: B → A → **C (this)** → D. **DF-10 pulled from this plan** — see the recommendation at the end.
>
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use `- [ ]` checkboxes.

**Goal:** Collapse repeated durable writes into batched/debounced ones on two paths: the disk import cache commits one redb transaction per entry during cold analysis (DF-4), and the registry metadata cache rewrites its entire JSON snapshot per package fetched (DF-5). Both persist *perf caches* — losing the most recent, unflushed entries to a hard crash only costs a re-computation, never correctness — so deferral is safe as long as graceful shutdown/recycle drains.

**Architecture:** Both changes are internal to `cache/disk.rs` and `registry/cache.rs`; the deferral is drained at the existing lifecycle chokepoints (recycle → `flush_to_disk`, shutdown, and `Drop`). The in-memory source of truth is unchanged in both, so reads always see the latest value regardless of persist timing. No IPC/protocol change; no on-disk *format* change (DF-4 keeps the same redb schema; DF-5 keeps the same JSON shape).

**Tech stack:** Rust 2024, redb 4, serde_json, std sync primitives (no new deps).

## Global constraints

- **No lost durability on graceful exit.** Every path that flushes recency touches today must also drain the new insert queue (DF-4); the registry cache must flush on its known completion points and on `Drop` (DF-5). The existing `daemon/tests/cache_disk.rs` reload tests and `daemon/tests/registry.rs` persistence tests are the guard — an entry inserted then reloaded in a fresh cache must still be found after a graceful flush.
- **Read-your-writes preserved.** A `get` immediately after an `insert`/`write_entry` must return the new value (satisfied by the in-memory layer in both; DF-4 additionally consults its queue on a disk-layer read).
- Each task ends with `cargo test -p import-lens-daemon` green and `cargo clippy -p import-lens-daemon --all-targets` introducing no new warnings.
- One commit per task; conventional-commit messages with a body naming the effect and the crash-durability tradeoff.
- Perf measurement extends `daemon/tests/performance.rs` where useful (`#[ignore]` release-only).

---

## Verification notes (checked against current code)

- **DF-4 confirmed.** `DiskCache::insert` ([disk.rs:103](../../daemon/src/cache/disk.rs)) opens a write transaction, inserts into `CACHE_TABLE` + `RECENTS_TABLE`, and `commit()`s (a durable fsync) for **each** entry. Its only callers are `ImportCache::insert_with_fingerprints` (once, sometimes twice via the namespace alias, per import) and `ImportCache::flush_to_disk` (the T16 dirty-replay). `handle_batch` runs `imports.par_iter()`, so a cold batch's parallel workers each open a redb write txn and serialize on redb's single writer. The `pending_touches` design ([disk.rs:143-206](../../daemon/src/cache/disk.rs): `touch` accumulates in a `Mutex<HashMap>`, flushes at `RECENCY_TOUCH_FLUSH_BATCH = 64` or on demand) is the exact pattern to mirror for inserts.
  - **Read-your-writes:** in-session, `ImportCache::get` hits the papaya memory map (populated synchronously by `insert_with_fingerprints`) before ever touching disk, so a queued-but-unflushed disk insert is invisible only to (a) `recent_keys`/`load_recent` and (b) a memory-miss disk read after eviction. Handle both: `recent_keys` already `flush_pending_touches()` first (add insert-flush there), and `get_entry` consults the pending queue before the table.
  - **T16 interaction:** `insert_with_fingerprints` marks a key "dirty" (in `ImportCache`) when `disk.insert` returns `Err`. After DF-4, `disk.insert` only *serializes + enqueues* — serialization failure is the remaining `Err` case (still → dirty), while commit failure is handled by re-queuing inside the flush. `flush_to_disk` must drain the disk queue **and** replay the memory dirty set.
  - **Durability tradeoff:** inserts stop being individually durable; a hard crash (not a graceful recycle/shutdown) between enqueue and flush loses those entries from disk. They are re-derivable analyses — acceptable for a perf cache. Graceful recycle (`flush_to_disk`), `recent_keys`, threshold, and `Drop` all drain.
- **DF-5 confirmed.** `RegistryMetadataCache::write_entry` ([cache.rs:47](../../daemon/src/registry/cache.rs)) updates the in-memory `entries` map then calls `persist_latest_snapshot` ([cache.rs:79](../../daemon/src/registry/cache.rs)) — serialize the **entire** map + `tmp`-write + `rename` — on every write. Callers are the four `fetch_package_with_retries` outcome branches (200/404/429/error, [registry/service.rs](../../daemon/src/registry/service.rs)) and the test-only `write_metadata`. Refreshing N packages ⇒ N full-map rewrites (O(N²) bytes). `get` ([cache.rs:38](../../daemon/src/registry/cache.rs)) reads the map, not the file, so deferring only the persist has **no** read-your-writes impact. There is currently no `Drop` on `RegistryMetadataCache` (it persisted synchronously); DF-5 adds one.
- **DF-10 pulled — see the end.** Not "write amplification"; different risk/payoff.

---

### Task C1: Batch disk-cache inserts into shared transactions (DF-4)

**Files:**
- Modify: `daemon/src/cache/disk.rs` (add a pending-insert queue mirroring `pending_touches`; `insert` enqueues; `get_entry`/`recent_keys` consult/drain it; `Drop` and the flush paths drain it)
- Modify: `daemon/src/cache/memory.rs` (`flush_to_disk` drains the disk queue in addition to replaying the memory dirty set)
- Test: `daemon/tests/cache_disk.rs` (append)

**Interfaces (produced):**
- `DiskCache` gains `pending_inserts: Mutex<HashMap<String, PendingInsert>>` where `PendingInsert { bytes: Vec<u8>, recorded_at_millis: u64 }` (the serialized envelope + its recents timestamp).
- `pub fn flush_pending_inserts(&self)` (public so `ImportCache::flush_to_disk` can drain it); an internal `const INSERT_FLUSH_BATCH: usize = 64;`.

- [ ] **Step 1: Write the tests** (append to `daemon/tests/cache_disk.rs`)

```rust
#[test]
fn insert_is_readable_before_flush_and_persists_after_flush() {
    let storage_path = temp_storage();
    let key = "react@18.3.1::default".to_owned();

    let cache = ImportCache::new(Some(storage_path.clone()), true);
    cache.insert(key.clone(), result("react"));

    // Read-your-writes: visible immediately (memory layer + disk queue).
    assert!(cache.get(&key).is_some());

    // A fresh cache over the same storage only sees it after a graceful flush.
    cache.flush_to_disk().expect("flush should succeed");
    let reloaded = ImportCache::new(Some(storage_path.clone()), true);
    assert!(reloaded.get(&key).is_some(), "flushed insert should survive reload");

    fs::remove_dir_all(storage_path).expect("cleanup");
}

#[test]
fn many_inserts_flush_in_batches_without_loss() {
    let storage_path = temp_storage();
    let cache = ImportCache::new(Some(storage_path.clone()), true);
    for index in 0..200 {
        cache.insert(format!("pkg{index}@1.0.0::default"), result("pkg"));
    }
    cache.flush_to_disk().expect("flush should succeed");

    let reloaded = ImportCache::new(Some(storage_path.clone()), true);
    // recent_keys drains the queue; all 200 keys must be persisted.
    assert_eq!(reloaded.recent_keys(1000).len(), 200);

    fs::remove_dir_all(storage_path).expect("cleanup");
}
```

- [ ] **Step 2: Run** — the reload/`recent_keys` assertions define the new batching contract; the first test likely passes today (synchronous insert), the second passes today too, so these are *characterization* tests that must stay green through the refactor. Run to confirm green now: `cargo test -p import-lens-daemon --test cache_disk insert_is_readable many_inserts`.

- [ ] **Step 3: Add the pending-insert queue** in `disk.rs`, mirroring `pending_touches`:
  - Field `pending_inserts: Mutex<HashMap<String, PendingInsert>>`, initialized in `new`/`disabled`.
  - `insert` becomes: build the envelope, `rmp_serde::to_vec` it (return `Err` on serialize failure — preserves T16's dirty path), then under the lock `pending_inserts.insert(key, PendingInsert { bytes, recorded_at_millis: unix_millis_now() })`, remove any pending touch for the key, and if `pending_inserts.len() >= INSERT_FLUSH_BATCH` call `flush_pending_inserts()`. It no longer opens a per-entry txn.
  - `flush_pending_inserts`: `std::mem::take` the map; in one write txn, insert every `(key, bytes)` into `CACHE_TABLE` and every `(key, recorded_at_millis)` into `RECENTS_TABLE`; commit. On failure, merge the drained entries back into the map (mirror `merge_pending_touches`) and `cache_warn`.
  - `get_entry`: before opening the read txn, check `pending_inserts` for the key; if present, decode those bytes (same `decode_cached_result` path, then the fingerprint check) and return — this is the disk-layer read-your-writes for a post-eviction memory miss.
  - `recent_keys`: call `flush_pending_inserts()` (alongside the existing `flush_pending_touches()`) before reading the recents table.
  - `remove`/`invalidate_package`/`clear`: also remove matching keys from `pending_inserts` (so a queued insert can't resurrect an invalidated entry). `clear` empties it.
  - `Drop`: drain `pending_inserts` then `pending_touches` (order: inserts first, since a flushed insert also writes its recents row).

- [ ] **Step 4: Drain in `ImportCache::flush_to_disk`** (`memory.rs`): after replaying the memory dirty set (T16 logic, unchanged), call `self.disk.flush_pending_inserts()` (before the existing `flush_pending_touches()`), so a recycle persists everything.

- [ ] **Step 5: Run the full suite** — `cargo test -p import-lens-daemon`. The existing `flush_to_disk_persists_memory_entries_for_reload`, `flush_to_disk_succeeds_with_nothing_dirty`, and recents-ordering tests must stay green. Clippy clean.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/disk.rs daemon/src/cache/memory.rs daemon/tests/cache_disk.rs
git commit -m "perf(cache): batch disk-cache inserts into shared transactions" -m "DiskCache::insert committed one durable redb transaction per entry, so a cold parallel batch serialized N fsyncs on redb's single writer. Queue serialized envelopes and flush them in one transaction at a size threshold, on recent_keys, on recycle (flush_to_disk), and on Drop, mirroring the existing pending-touches path. Reads still see queued entries (memory layer plus a disk-queue probe). Inserts are no longer individually durable; a hard crash before a graceful flush loses at most the most recent re-derivable analyses."
```

---

### Task C2: Debounce registry metadata snapshot persistence (DF-5)

**Files:**
- Modify: `daemon/src/registry/cache.rs` (defer `persist_latest_snapshot`; add `flush` + threshold + `Drop`)
- Modify: `daemon/src/registry/service.rs` (expose `flush`; flush at the end of a refresh batch)
- Modify: `daemon/src/service.rs` (flush after the `analyze_package_json` registry-hint loop)
- Test: `daemon/tests/registry.rs` (append)

**Interfaces (produced):**
- `RegistryMetadataCache` gains `dirty: Mutex<bool>` (or an `AtomicUsize` write counter) and `pub fn flush(&self) -> Result<(), String>` (persist if dirty). `write_entry`/`write_metadata` update the map + mark dirty + threshold-flush; they no longer persist every call.
- `RegistryHintService::flush(&self)` delegates to the cache; `RegistryHintService::disabled()` / no-op cache flush is a no-op.
- `const REGISTRY_PERSIST_BATCH: usize = 16;` (persist at most every N writes as a backstop).

- [ ] **Step 1: Write the test** (append to `daemon/tests/registry.rs`, following its existing metadata-cache helpers)

```rust
#[test]
fn registry_metadata_persists_once_on_flush_not_per_write() {
    let dir = common::temp_workspace("import-lens-registry-flush");
    let cache = RegistryMetadataCache::new(dir.clone());

    // Several writes below the batch threshold do not have to touch disk...
    for i in 0..5 {
        cache
            .write_metadata(&format!("pkg{i}"), sample_metadata(&format!("1.0.{i}")), 1000 + i)
            .expect("write");
    }
    // ...but a flush makes all of them durable and reloadable.
    cache.flush().expect("flush");

    let reloaded = RegistryMetadataCache::new(dir.clone());
    for i in 0..5 {
        assert!(
            reloaded.get(&format!("pkg{i}")).is_some(),
            "pkg{i} should reload after flush"
        );
    }

    fs::remove_dir_all(dir).expect("cleanup");
}
```

(Add a `sample_metadata(latest: &str) -> RegistryPackageMetadata` helper if the file lacks one; `write_metadata` already exists on the cache.)

- [ ] **Step 2: Run** → passes today only incidentally (writes persist eagerly). Keep it as the durability characterization test through the refactor.

- [ ] **Step 3: Defer the persist** in `cache.rs`:
  - Add `dirty: Mutex<bool>` (init `false`) and a write counter, or a single `Mutex<usize>` pending-write count.
  - `write_entry`: update the map (as now), set dirty, increment the counter; if the counter `>= REGISTRY_PERSIST_BATCH`, call `self.flush()` and reset. Remove the unconditional `persist_latest_snapshot()` call.
  - `flush`: if not dirty, `Ok(())`; else `persist_latest_snapshot()` (unchanged atomic tmp+rename) and clear dirty/counter. Keep the `persist_lock` + snapshot-clone logic exactly.
  - `impl Drop for RegistryMetadataCache { fn drop(&mut self) { let _ = self.flush(); } }` — the safety net for graceful teardown.

- [ ] **Step 4: Flush at completion points:**
  - `RegistryHintService::flush(&self)` → `self.cache.flush()`; `disabled()`'s empty cache flush is a no-op (`path.is_empty()` → `Ok(())`, already handled by `persist_latest_snapshot`).
  - `service.rs::analyze_package_json`: after the dependency loop that calls `registry_hints.hint_for(...)`, call `self.registry_hints.flush()` once (best-effort, log on error) so a package.json analysis persists its fetched metadata in one write.
  - `server.rs` RefreshRegistryHints aggregation: after the final aggregate response is sent, call the service's registry flush. (The aggregation `tokio::spawn` must capture the `Arc<ImportLensService>`; thread it in, or expose `service.flush_registry_hints()` and call it there.)

- [ ] **Step 5: Run the full suite** — `cargo test -p import-lens-daemon`. Existing `tests/registry.rs` persistence/round-trip tests stay green (a reload after the service's flush still sees the metadata). Clippy clean.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/registry/cache.rs daemon/src/registry/service.rs daemon/src/service.rs
git commit -m "perf(registry): debounce metadata snapshot persistence" -m "write_entry rewrote the entire registry-metadata.json (serialize + tmp + rename) on every package fetched, so refreshing N packages wrote O(N^2) bytes. Keep the in-memory map as the source of truth (reads are unaffected) and persist the full snapshot once per refresh batch, at a write-count threshold, and on Drop, instead of per write. A hard crash before flush loses at most recently fetched, re-fetchable metadata."
```

---

## DF-10 — pulled from this plan (recommendation)

**Do not fold DF-10 in here.** Verification shows it is a poor fit for a "write amplification" plan and the highest-risk item in the backlog:

- **It is a different concern** — key *size/shape*, not write batching. Cache keys are `v3:` + hex(msgpack(`CacheIdentityV3`)) ([key.rs:107](../../daemon/src/cache/key.rs)), routinely 400–1000+ chars because `package_root` + `entry_path` are embedded.
- **It forces a schema change and two dependent reworks.** A short-hash key means `cache_key_matches_package` ([key.rs](../../daemon/src/cache/key.rs)) can no longer decode the package name from the key — it needs a new `package_name → keys` secondary index (a new redb table, maintained on every insert/remove/invalidate). Prewarm's `cached_import_request_from_key` ([prefetch.rs](../../daemon/src/prefetch.rs)) reconstructs the request *from the key* and would have to read the envelope's `package_identity` instead. And `CURRENT_SCHEMA_VERSION` bumps 4→5 (the mismatch path recreates the DB — a one-time cold cache).
- **Its payoff is soft and unmeasured** — the backlog itself says "memory + invalidation speed; not latency-critical." Invalidation is not on the hot path (it fires on `node_modules` changes), so the decode-per-key cost it removes is rarely paid.

**Recommendation:** give DF-10 its own plan **only if** memory footprint or invalidation latency is shown to be a real problem, and split it so the risk is isolated:
- *DF-10a (low-risk, optional):* an in-memory `package_name → HashSet<key>` index so `invalidate_package` stops decoding every key — no key-format or schema change.
- *DF-10b (the risky part):* short-hash keys + envelope-sourced identity + secondary index + schema bump. Treat as a standalone plan with its own migration test matrix.

Until then, DF-10 stays parked in the backlog alongside Plan D's items.

## Exit

C1 → C2. On completion, Plan D remains (DF-7 deferred-here, DF-8 report ignore memo, DF-9 Windows delete race, DF-12 remainder; DF-11 kept as-is per decision; DF-10 parked per above).

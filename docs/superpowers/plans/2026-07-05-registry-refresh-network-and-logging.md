# Registry Refresh — Network Reduction & Observable Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop ImportLens from wasting network on oversized npm packuments (e.g. `next`) and from firing ~100 redundant registry-refresh requests per package.json analysis, and make every refresh's cache-vs-network behavior and per-package failures visible in the logs.

**Architecture:** Three fixes plus logging. (1) The Rust daemon raises ureq's body-read cap so large packuments succeed, and classifies genuinely-oversized responses as *permanent* failures cached for 6h instead of retried every 5 minutes. (2) The daemon tags each hint result with its `origin` (cache vs network) and logs actual network fetches with status/size/duration. (3) The TS extension stops queuing a registry refresh per streaming partial (queues once at the end) and skips re-analysis when a package.json's content is unchanged (kills the tab-focus storm), while still honoring explicit re-analysis triggers. (4) A new `importLens.verboseRegistryLogging` toggle unlocks per-package cache/network logging; a concise per-refresh summary logs by default from the extension using the daemon's `origin` field.

**Tech Stack:** Rust daemon (ureq 3.3 HTTP, rayon, serde IPC over named pipe), TypeScript VS Code extension (tsdown build), `node --test` (extension tests: `node:test` + `node:assert/strict`), `cargo test` (daemon tests). Formatters: Biome (TS), `cargo fmt` (Rust).

## Global Constraints

- Registry metadata cache file stays `registry-metadata.json`; do not touch the unrelated module-size `importlens.redb` cache. The `disk_cache=true` seen in logs is the module cache, not registry metadata.
- Config namespace is `importLens` (camelCase) in both `package.json` contributes and `vscode.workspace.getConfiguration("importLens")`.
- Adding an **optional** field to `RegistryHintResult` is backward-compatible and does NOT require a protocol version bump — the field is `#[serde(default, skip_serializing_if = "Option::is_none")]` on the Rust side and optional (`?`) on the TS side.
- Keep dependency versions as-is. No new crates; no dependency bumps (ureq stays `^3.3`, locked `3.3.0`).
- ureq 3.3.0 body-limit API (verified against source): `response.body_mut().with_config().limit(u64).read_to_string()`. `with_config(&mut self) -> BodyWithConfig`; `BodyWithConfig::limit(self, u64) -> Self`; `BodyWithConfig::read_to_string(self) -> Result<String, Error>`.

## Commands (verified against the repo)

- Daemon crate name: `import-lens-daemon`.
- Run daemon tests: `cargo test -p import-lens-daemon` (or the repo-wide `pnpm test:rust` → `cargo test --workspace`).
- Run extension TS tests: `pnpm test:ts` (removes `dist/test-dist`, runs `tsc -p tsconfig.test.json`, then `node --test "dist/test-dist/**/*.test.js"`). For a single file after compiling: `node --test dist/test-dist/guidance/<name>.test.js`.
- Typecheck: `pnpm check` (`tsc --noEmit`). Lint: `pnpm lint` (`biome check`). Format: `pnpm format` (Biome) + `cargo fmt` after each task.
- Extension tests use `import test from "node:test"; import assert from "node:assert/strict";` — NOT vitest/jest. No `vscode` module is available in tests; only pure (vscode-free) modules are unit-tested. The `PackageJsonAnalysisController` imports `vscode` and is therefore verified by integration (Task 8), not unit tests.

---

## File Structure

**Daemon (Rust):**
- `daemon/src/registry/constants.rs` — add `MAX_REGISTRY_BODY_BYTES`.
- `daemon/src/registry/client.rs` — raise ureq read limit.
- `daemon/src/registry/types.rs` — add `RegistryHintOrigin`; extend `RegistryHintLookup`.
- `daemon/src/registry/service.rs` — permanent-error classification, TTL selection, `origin` propagation, network-fetch success log.
- `daemon/src/ipc/protocol.rs` — add optional `origin` to `RegistryHintResult`.
- `daemon/src/service.rs` — populate `origin` on the built `RegistryHintResult` (+ `origin: None` on fallbacks the compiler flags).
- `daemon/src/registry/cache.rs` — merge-on-persist so concurrent windows sharing the global cache don't clobber each other (`persist_latest_snapshot`).
- `daemon/tests/registry_cache.rs` — new integration test for merge-on-persist.

**Extension (TypeScript):**
- `extension/src/ipc/protocol.ts` — add optional `origin` to `RegistryHintResult`.
- `extension/src/guidance/analyzedContentTracker.ts` — new pure helper for the unchanged-content guard.
- `extension/test/guidance/analyzedContentTracker.test.ts` — its unit test.
- `extension/src/guidance/packageJsonAnalysis.ts` — stop per-partial refresh; wire the content guard; invalidate on explicit refresh.
- `extension/src/guidance/registryRefresh.ts` — summary + verbose per-package logging using `origin` and an injected `isVerbose` getter.
- `extension/test/guidance/registryRefresh.test.ts` — extend for the summary/verbose behavior.
- `extension/src/daemon/nativeTransport.ts` — name packages in the refresh request log.
- `extension/src/config.ts` + root `package.json` — add `verboseRegistryLogging` toggle.

---

## Part 1 — Daemon: stop the oversized-packument waste

### Task 1: Raise the body-read limit

**Files:**
- Modify: `daemon/src/registry/constants.rs`
- Modify: `daemon/src/registry/client.rs:1` (imports) and `:44-47` (body read)

**Interfaces:**
- Produces: `pub const MAX_REGISTRY_BODY_BYTES: u64` — consumed by `client.rs`.

- [ ] **Step 1: Add the constant**

Append to `daemon/src/registry/constants.rs`:

```rust
/// Upper bound for a single npm packument body. npm's abbreviated
/// ("corgi") metadata for very high-churn packages exceeds ureq's 10 MiB
/// default `read_to_string` cap — `next`'s corgi packument measures
/// ~25 MB (its full packument ~31 MB) because its `versions` map holds
/// thousands of releases. 64 MB clears that with headroom; bodies larger
/// than this are treated as a permanent fetch failure (see
/// `is_permanent_fetch_error`). Only the extracted metadata
/// (latest_version, latest_published_at, deprecated_versions) is cached —
/// never the multi-MB body — so the on-disk cache stays small. The body is
/// held transiently during parse (peak ~2-3x its size in the serde_json
/// value tree), bounded by REGISTRY_REFRESH_CONCURRENCY (4).
pub const MAX_REGISTRY_BODY_BYTES: u64 = 64 * 1024 * 1024;
```

- [ ] **Step 2: Apply the limit in the client**

`daemon/src/registry/client.rs` line 1 becomes:

```rust
use super::{
    constants::{DEFAULT_TIMEOUT_MS, MAX_REGISTRY_BODY_BYTES},
    types::HttpRegistryResponse,
};
```

Lines 44-47 (the `read_to_string`) become:

```rust
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REGISTRY_BODY_BYTES)
            .read_to_string()
            .map_err(|error| error.to_string())?;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p import-lens-daemon`
Expected: builds clean.

- [ ] **Step 4: Format + commit**

```bash
cargo fmt
git add daemon/src/registry/constants.rs daemon/src/registry/client.rs
git commit -m "fix(registry): raise npm packument body cap to 64MB"
```

---

### Task 2: Classify oversized/permanent failures and stop retrying them

**Files:**
- Modify: `daemon/src/registry/service.rs` (`fetch_package_with_retries` at `:257-388`; add classifier + tests to the `#[cfg(test)] mod tests` at `:541`)

**Interfaces:**
- Produces: `fn is_permanent_fetch_error(message: &str) -> bool`.
- Behavior: a permanent error does 1 attempt (not 3) and is cached with `NOT_FOUND_TTL_MS` (6h) instead of `TRANSIENT_ERROR_RETRY_MS` (5min).

- [ ] **Step 1: Write the failing unit test**

Add to `mod tests` in `daemon/src/registry/service.rs`:

```rust
    #[test]
    fn permanent_errors_are_recognized() {
        assert!(is_permanent_fetch_error(
            "the response body is larger than request limit: 67108864"
        ));
        assert!(!is_permanent_fetch_error("connection reset by peer"));
        assert!(!is_permanent_fetch_error("timed out"));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p import-lens-daemon permanent_errors_are_recognized`
Expected: FAIL — `is_permanent_fetch_error` not found.

- [ ] **Step 3: Add the classifier**

Add near `is_transient_status` / `transient_backoff_ms` in `service.rs` (module scope, outside the impl):

```rust
/// A permanent fetch failure will not succeed on retry within a short
/// window, so we skip the remaining attempts and cache it for the
/// not-found TTL instead of the 5-minute transient window. The oversize
/// body error from ureq (body exceeds `MAX_REGISTRY_BODY_BYTES`) is the
/// current instance.
fn is_permanent_fetch_error(message: &str) -> bool {
    message.contains("larger than request limit")
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p import-lens-daemon permanent_errors_are_recognized`
Expected: PASS.

- [ ] **Step 5: Wire permanence into the retry loop**

In `fetch_package_with_retries`, add a `permanent` flag. Change the function top (around line 262):

```rust
        let mut last_error = None;
        let mut permanent = false;
        for attempt in 1..=MAX_ATTEMPTS {
```

Replace the `Err(error)` arm (lines 351-363) with:

```rust
                Err(error) => {
                    if is_permanent_fetch_error(&error) {
                        last_error = Some(error);
                        permanent = true;
                        break;
                    }
                    last_error = Some(error);
                    if attempt == MAX_ATTEMPTS {
                        break;
                    }
                    logging::log_debug(
                        "registry",
                        format!(
                            "retrying npm metadata fetch for {package_name} after network failure attempt {attempt}"
                        ),
                    );
                    sleep_before_retry(attempt);
                }
```

Replace the final failed-entry block (lines 367-387) so the TTL depends on `permanent`:

```rust
        let retry_after_ms = if permanent {
            now_ms + NOT_FOUND_TTL_MS
        } else {
            now_ms + TRANSIENT_ERROR_RETRY_MS
        };
        logging::log_warn(
            "registry",
            format!(
                "failed to refresh npm metadata for {package_name} after {} attempt(s){}: {}",
                if permanent { 1 } else { MAX_ATTEMPTS },
                if permanent { " (permanent, cached 6h)" } else { "" },
                last_error.as_deref().unwrap_or("unknown error"),
            ),
        );
        let entry = failed_entry_from_cache(
            self.cache.get(package_name).as_ref(),
            last_error
                .clone()
                .unwrap_or_else(|| "unknown registry error".to_owned()),
            retry_after_ms,
        );
        if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
            logging::log_warn(
                "registry",
                format!("failed to persist npm error metadata for {package_name}: {error}"),
            );
        }
        entry
```

`NOT_FOUND_TTL_MS` is already imported in `service.rs` (used by `is_usable_without_fetch`). No import change needed.

- [ ] **Step 6: Write a failing behavior test (single attempt, no retry)**

Add to `mod tests` in `service.rs`. This uses the real constructor `RegistryHintService::new(cache, client)` with an injected counting client:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingOversizeClient {
        calls: Arc<AtomicUsize>,
    }

    impl RegistryHttpClient for CountingOversizeClient {
        fn get_package_metadata(&self, _package_name: &str) -> Result<HttpRegistryResponse, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("the response body is larger than request limit: 67108864".to_owned())
        }
    }

    #[test]
    fn permanent_error_does_not_retry_and_caches_long() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = RegistryHintService::new(
            RegistryMetadataCache::empty(),
            Box::new(CountingOversizeClient { calls: Arc::clone(&calls) }),
        );

        let entry = service.fetch_package_with_retries("next", 1_000);

        assert_eq!(calls.load(Ordering::SeqCst), 1, "permanent error must not retry");
        assert!(entry.error.is_some());
        // Permanent -> cached for the 6h not-found TTL, not the 5-min transient window.
        assert_eq!(entry.retry_after, Some(1_000 + NOT_FOUND_TTL_MS));
    }
```

If `RegistryMetadataCache::empty()`, `fetch_package_with_retries`, or `RegistryPackageMetadataEntry.retry_after` are not visible from the test module, adjust to the actual field/visibility (all are in-crate and reachable via `use super::*;`, which the module already has). `RegistryMetadataCache::empty()` is the same constructor `RegistryHintService::disabled()` uses.

- [ ] **Step 7: Run the registry tests**

Run: `cargo test -p import-lens-daemon registry`
Expected: PASS (new tests + existing rate-limiter test).

- [ ] **Step 8: Format + commit**

```bash
cargo fmt
git add daemon/src/registry/service.rs
git commit -m "fix(registry): treat oversized packuments as permanent, cache 6h no-retry"
```

---

## Part 2 — Daemon: make cache-vs-network observable

### Task 3: Tag hint lookups with their origin and log real fetches

**Files:**
- Modify: `daemon/src/registry/types.rs` (add `RegistryHintOrigin`, extend `RegistryHintLookup`)
- Modify: `daemon/src/registry/service.rs` (`hint_for`, `lookup_from_entry`, network-fetch success log)

**Interfaces:**
- Produces: `pub enum RegistryHintOrigin { Cache, Network }` and `RegistryHintLookup { hint, error, origin }`.
- Consumes (Task 4): `daemon/src/service.rs` reads `lookup.origin`.

- [ ] **Step 1: Add the origin type and extend the lookup**

In `daemon/src/registry/types.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryHintOrigin {
    Cache,
    Network,
}
```

Find `pub struct RegistryHintLookup` and add the field:

```rust
pub struct RegistryHintLookup {
    pub hint: Option<RegistryHint>,
    pub error: Option<String>,
    pub origin: RegistryHintOrigin,
}
```

- [ ] **Step 2: Compile to enumerate construction sites**

Run: `cargo build -p import-lens-daemon`
Expected: FAIL — each `RegistryHintLookup { .. }` now needs `origin`. Sites in `service.rs` `hint_for`: the `Off` early return (build inline), the `Cached`-mode return (inline), and every `lookup_from_entry(...)` call (fresh-within-TTL, retry-after serve-stale, and the post-`fetch_package_singleflight` path).

- [ ] **Step 3: Give `lookup_from_entry` an origin parameter and populate all sites**

Change `lookup_from_entry`'s signature and returned struct:

```rust
fn lookup_from_entry(
    entry: &RegistryPackageMetadataEntry,
    installed_version: Option<&str>,
    origin: RegistryHintOrigin,
) -> RegistryHintLookup {
    // ...unchanged body..., add `origin,` to the returned RegistryHintLookup
}
```

In `hint_for`:
- `Off` early return → `RegistryHintLookup { hint: None, error: None, origin: RegistryHintOrigin::Cache }`.
- `Cached` mode: pass `RegistryHintOrigin::Cache` to `lookup_from_entry`, and the `unwrap_or` empty return → `origin: RegistryHintOrigin::Cache`.
- fresh `is_usable_without_fetch` return → `lookup_from_entry(entry, installed_version, RegistryHintOrigin::Cache)`.
- `retry_after` serve-stale return → `... RegistryHintOrigin::Cache`.
- final `fetch_package_singleflight` path (line ~200) → `lookup_from_entry(&entry, installed_version, RegistryHintOrigin::Network)`.

Import `RegistryHintOrigin` at the top of `service.rs` (add to the `types::{...}` import list).

- [ ] **Step 4: Add the network-fetch success log (status, size, duration)**

In `fetch_package_with_retries`, time the client call and log on the 200 branch. Change the loop body's client call (line 264-265) and the success arm head (lines 266-291):

```rust
            self.wait_for_rate_limit_slot();
            let started = Instant::now();
            match self.client.get_package_metadata(package_name) {
                Ok(response) if response.status == 200 => {
                    let body_bytes = response.body.len();
                    let elapsed_ms = started.elapsed().as_millis();
                    let metadata = match package_metadata_from_response(response) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            logging::log_warn(
                                "registry",
                                format!("failed to parse npm metadata for {package_name}: {error}"),
                            );
                            last_error = Some(error);
                            break;
                        }
                    };
                    logging::log_debug(
                        "registry",
                        format!(
                            "fetched npm metadata for {package_name}: 200, {body_bytes} bytes, {elapsed_ms}ms"
                        ),
                    );
                    let entry = RegistryPackageMetadataEntry {
                        metadata: Some(metadata),
                        updated_at: now_ms,
                        retry_after: None,
                        error: None,
                        not_found: false,
                    };
                    // ...unchanged persist + return...
```

`Instant` is already imported in `service.rs`. Read `response.body.len()` BEFORE `package_metadata_from_response(response)` (which consumes `response`).

- [ ] **Step 5: Build + test**

Run: `cargo test -p import-lens-daemon`
Expected: PASS.

- [ ] **Step 6: Format + commit**

```bash
cargo fmt
git add daemon/src/registry/types.rs daemon/src/registry/service.rs
git commit -m "feat(registry): tag hints cache/network, log fetch size+duration"
```

---

### Task 4: Surface origin in the IPC result

**Files:**
- Modify: `daemon/src/ipc/protocol.rs:317` (`RegistryHintResult`)
- Modify: `daemon/src/service.rs:186-190` (populate origin)
- Modify: `daemon/src/ipc/server.rs` (add `origin: None` to the two fallback `RegistryHintResult { .. }` constructions)

**Interfaces:**
- Produces: `RegistryHintResult.origin: Option<String>` serialized camelCase `"cache"`/`"network"`, consumed by the extension in Task 7.

- [ ] **Step 1: Add the optional field to the Rust protocol struct**

In `daemon/src/ipc/protocol.rs`, inside `pub struct RegistryHintResult` (after the `error` field):

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
```

- [ ] **Step 2: Populate origin where the result is built**

In `daemon/src/service.rs`, the `RegistryHintResult { target, hint, error }` at line 186 becomes:

```rust
        let origin = match lookup.origin {
            crate::registry::types::RegistryHintOrigin::Cache => "cache",
            crate::registry::types::RegistryHintOrigin::Network => "network",
        };
        RegistryHintResult {
            target,
            hint: lookup.hint,
            error: lookup.error,
            origin: Some(origin.to_owned()),
        }
```

- [ ] **Step 3: Fix the fallback constructions**

Run: `cargo build -p import-lens-daemon`
Expected: FAIL — `RegistryHintResult` needs `origin` at the panic fallback (`server.rs` ~line 536) and the `unwrap_or` fallback (~line 571), plus any other site the compiler flags. Add `origin: None,` to each.

- [ ] **Step 4: Build + test the daemon**

Run: `cargo test -p import-lens-daemon && cargo fmt --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/ipc/protocol.rs daemon/src/service.rs daemon/src/ipc/server.rs
git commit -m "feat(registry): report hint origin (cache/network) over IPC"
```

---

### Task 4b: Merge-on-persist so concurrent projects don't clobber the shared cache

The registry metadata cache lives in **global** storage (`--storage = globalStorageUri` → `<globalStorage>/registry-metadata.json`) and is shared by every workspace's daemon — the cache key is the bare package name, so entries are reusable across projects. But persistence is currently a **last-writer-wins full-snapshot** overwrite ([cache.rs `persist_latest_snapshot`](../../daemon/src/registry/cache.rs)). With two VS Code windows open, one daemon's snapshot write drops packages the other cached after this process loaded, forcing needless re-fetches. Merging the on-disk view in before writing fixes that.

**Files:**
- Modify: `daemon/src/registry/cache.rs` (`persist_latest_snapshot`)
- Create: `daemon/tests/registry_cache.rs` (integration test)

**Interfaces:**
- Consumes: `load_entries(&Path)` (existing free fn in `cache.rs`) and `RegistryPackageMetadataEntry.updated_at`.
- Behavior: before the atomic temp-write+rename, union the on-disk entries into the cloned snapshot, keeping the entry with the newest `updated_at` per package. The cache never evicts, so a union can never resurrect a deliberately-removed entry. A tiny cross-process TOCTOU window between read and rename remains (documented) — this converts "clobber everything another process wrote" into "clobber only what it wrote in the few ms between our read and rename", which is acceptable without introducing cross-process file locking.

- [ ] **Step 1: Write the failing integration test**

Create `daemon/tests/registry_cache.rs`:

```rust
mod common;

use import_lens_daemon::registry::{cache::RegistryMetadataCache, types::RegistryPackageMetadata};

fn metadata(latest: &str) -> RegistryPackageMetadata {
    RegistryPackageMetadata {
        latest_version: Some(latest.to_owned()),
        latest_published_at: None,
        deprecated_versions: Vec::new(),
    }
}

#[test]
fn persist_merges_entries_written_by_another_process() {
    let dir = common::temp_workspace("import-lens-registry-merge");

    // Window A loads and holds only `react`.
    let cache_a = RegistryMetadataCache::new(dir.clone());
    cache_a.write_metadata("react", metadata("18.0.0"), 100).expect("write react");
    cache_a.flush().expect("flush A");

    // Window B loads the same global file and caches a disjoint package after
    // A already holds its in-memory map.
    let cache_b = RegistryMetadataCache::new(dir.clone());
    cache_b.write_metadata("vue", metadata("3.4.0"), 200).expect("write vue");
    cache_b.flush().expect("flush B");

    // A persists again. A plain full-snapshot overwrite would drop `vue`
    // (A never had it); merge-on-persist must keep it.
    cache_a.write_metadata("svelte", metadata("4.0.0"), 300).expect("write svelte");
    cache_a.flush().expect("flush A again");

    let reloaded = RegistryMetadataCache::new(dir);
    assert!(reloaded.get("react").is_some(), "react should survive");
    assert!(reloaded.get("vue").is_some(), "vue must not be clobbered by A's snapshot");
    assert!(reloaded.get("svelte").is_some(), "svelte should be written");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p import-lens-daemon --test registry_cache`
Expected: FAIL — `vue` was clobbered by A's last snapshot (assertion on `vue` fails).

- [ ] **Step 3: Merge the on-disk view before writing**

In `daemon/src/registry/cache.rs` `persist_latest_snapshot`, change the snapshot binding (line ~116) from an immutable to a mutable clone and union the on-disk entries in before serializing:

```rust
        let Ok(mut snapshot) = self.entries.lock().map(|entries| entries.clone()) else {
            return Err("registry cache lock poisoned".to_owned());
        };
        // The registry cache is shared across every workspace's daemon via
        // global storage. Another process may have persisted entries since we
        // loaded, so union the on-disk view in (keeping the newest `updated_at`
        // per package) before this full-snapshot write, instead of clobbering
        // their entries.
        for (key, on_disk) in load_entries(&self.path) {
            let keep_ours = snapshot
                .get(&key)
                .is_some_and(|ours| ours.updated_at >= on_disk.updated_at);
            if !keep_ours {
                snapshot.insert(key, on_disk);
            }
        }
```

The rest of the function (parent `create_dir_all`, `serde_json::to_vec(&snapshot)`, temp-write, rename) is unchanged.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p import-lens-daemon --test registry_cache`
Expected: PASS.

- [ ] **Step 5: Run the full daemon suite (guard against regressions)**

Run: `cargo test -p import-lens-daemon`
Expected: PASS (existing `tests/registry.rs`, `tests/memory_cache.rs`, etc. unaffected — merge is a superset write).

- [ ] **Step 6: Format + commit**

```bash
cargo fmt
git add daemon/src/registry/cache.rs daemon/tests/registry_cache.rs
git commit -m "fix(registry): merge on persist so concurrent windows share the cache"
```

---

## Part 3 — Extension: kill fan-out amplification and focus re-runs

### Task 5: Queue registry refreshes once (not per streaming partial)

**Files:**
- Modify: `extension/src/guidance/packageJsonAnalysis.ts:236-252` (`handlePackageJsonPartial`)

**Interfaces:**
- Behavior change: streaming partials update UI states but no longer trigger `queueRegistryRefreshes`. The single refresh fires from `analyze()` line 151 after the final response, covering all targets.
- No unit test: the controller imports `vscode` and has no test harness in this repo (see Commands section). The existing `extension/test/guidance/registryRefresh.test.ts` continues to cover the refresher. This change is verified by integration in Task 8 (log shows one refresh per analysis, not ~106).

- [ ] **Step 1: Remove the per-partial refresh**

In `handlePackageJsonPartial` (lines 236-252), delete the `queueRegistryRefreshes` call on line 251. The method becomes:

```ts
  private handlePackageJsonPartial(
    uri: vscode.Uri,
    key: string,
    partial: AnalyzePackageJsonResponse,
  ): void {
    if (!this.#freshness.isCurrent(key, partial.request_id) || partial.error) {
      return;
    }

    if (partial.sections.length > 0) {
      this.#sections.set(key, partial.sections);
    }

    const states = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], partial);
    this.setStates(uri, states);
  }
```

Leave the single `this.queueRegistryRefreshes(document.uri, states);` in `analyze()` (line 151) intact — it becomes the sole trigger. (Trade-off: on first load, latest-version markers appear after the final analysis response rather than progressively; hints are secondary, so this is acceptable.)

- [ ] **Step 2: Typecheck + build the extension tests**

Run: `pnpm check && pnpm test:ts`
Expected: PASS — no existing test asserted the per-partial behavior, so all stay green.

- [ ] **Step 3: Format + commit**

```bash
pnpm format
git add extension/src/guidance/packageJsonAnalysis.ts
git commit -m "fix(registry): queue one registry refresh per analysis, not per partial"
```

---

### Task 6: Skip re-analysis of unchanged package.json (focus guard) with explicit-refresh invalidation

**Files:**
- Create: `extension/src/guidance/analyzedContentTracker.ts`
- Create: `extension/test/guidance/analyzedContentTracker.test.ts`
- Modify: `extension/src/guidance/packageJsonAnalysis.ts` (`analyze`, `clear`, `refreshVisibleDocuments`, class field)

**Interfaces:**
- Produces: `class AnalyzedContentTracker { isUnchanged(key: string, text: string): boolean; record(key: string, text: string): void; forget(key: string): void; }`.
- Behavior: passive `onDidOpenTextDocument` / `onDidChangeActiveTextEditor` (focus) → `schedule` → `analyze` returns early when text equals the last successfully-analyzed text. Explicit re-analysis (config change, daemon restart, cache clear, node_modules watcher) funnels through the controller's `refreshVisibleDocuments()`, which forgets the tracked content first so those always re-run.

- [ ] **Step 1: Write the failing helper test**

Create `extension/test/guidance/analyzedContentTracker.test.ts`:

```ts
import assert from "node:assert/strict";
import test from "node:test";
import { AnalyzedContentTracker } from "../../src/guidance/analyzedContentTracker.js";

test("reports unchanged only for the exact recorded text", () => {
  const tracker = new AnalyzedContentTracker();
  const key = "file:///p/package.json";

  assert.equal(tracker.isUnchanged(key, "a"), false);
  tracker.record(key, "a");
  assert.equal(tracker.isUnchanged(key, "a"), true);
  assert.equal(tracker.isUnchanged(key, "a "), false);
});

test("forget clears the recorded text so the next analyze runs", () => {
  const tracker = new AnalyzedContentTracker();
  const key = "file:///p/package.json";
  tracker.record(key, "a");
  tracker.forget(key);
  assert.equal(tracker.isUnchanged(key, "a"), false);
});
```

- [ ] **Step 2: Run it to verify it fails**

Run: `pnpm test:ts` (or after compiling: `node --test dist/test-dist/guidance/analyzedContentTracker.test.js`)
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the helper**

Create `extension/src/guidance/analyzedContentTracker.ts`:

```ts
/**
 * Records, per document key, the exact text of the last successful package.json
 * analysis so passive re-triggers (tab focus, re-open) can skip redundant work.
 * Explicit re-analysis paths call `forget` first to force a fresh run.
 */
export class AnalyzedContentTracker {
  readonly #content = new Map<string, string>();

  isUnchanged(key: string, text: string): boolean {
    return this.#content.get(key) === text;
  }

  record(key: string, text: string): void {
    this.#content.set(key, text);
  }

  forget(key: string): void {
    this.#content.delete(key);
  }
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `pnpm test:ts`
Expected: PASS.

- [ ] **Step 5: Wire the tracker into the controller**

In `extension/src/guidance/packageJsonAnalysis.ts`:

Add the import:

```ts
import { AnalyzedContentTracker } from "./analyzedContentTracker.js";
```

Add the field near the other private maps (around line 34):

```ts
  readonly #analyzedContent = new AnalyzedContentTracker();
```

In `analyze()`, add the guard AFTER the `config.enabled`/`isPackageJsonDocument` check (after line 101) and BEFORE the workspace/daemon work. Capture the text once and reuse it:

```ts
    const currentText = document.getText();
    if (this.#analyzedContent.isUnchanged(key, currentText)) {
      return;
    }
```

Record on success — add immediately after `this.queueRegistryRefreshes(document.uri, states);` (line 151):

```ts
      this.#analyzedContent.record(key, currentText);
```

Invalidate in `clear(uri)` (line 198 body) — add:

```ts
    this.#analyzedContent.forget(key);
```

Invalidate explicit re-analysis in `refreshVisibleDocuments()` (lines 166-170) so config/restart/cache/watcher refreshes bypass the guard:

```ts
  refreshVisibleDocuments(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.#analyzedContent.forget(editor.document.uri.toString());
      this.schedule(editor.document);
    }
  }
```

Rationale (verified): config changes route through `onDidChangeConfiguration → refreshVisibleDocuments(config, mode) → refreshVisibleImportLensDocuments → actions.refreshPackageJsonHints() → packageJsonAnalysis.refreshVisibleDocuments()` for BOTH `reanalyze` and `uiOnly` modes (`refreshPackageJsonHints` is called unconditionally). Daemon restart, cache-clear commands, and node_modules watchers all reach the same method. Forgetting there — and only there — keeps passive focus/open guarded while explicit refreshes always run.

- [ ] **Step 6: Typecheck + run all extension tests**

Run: `pnpm check && pnpm test:ts`
Expected: PASS (new tracker tests + existing suite, including `configChange.test.ts`, `configRefresh.test.ts`, `watcherInvalidation.test.ts` — none instantiate the controller, so they are unaffected).

- [ ] **Step 7: Format + commit**

```bash
pnpm format
git add extension/src/guidance/analyzedContentTracker.ts extension/test/guidance/analyzedContentTracker.test.ts extension/src/guidance/packageJsonAnalysis.ts
git commit -m "fix(registry): skip re-analysis of unchanged package.json on focus"
```

> **Documented trade-off:** an open package.json that is never edited will not re-refresh registry hints after the 6h TTL until an edit or an explicit refresh (the `importLens.refreshPackageJsonRegistryHints...` commands, a config change, or a daemon restart). This is intended — it is the whole point of stopping the focus-driven refresh storm.

---

## Part 4 — Extension: observable logging

### Task 7: Add the verbose toggle and cache/network logging

**Files:**
- Modify: `extension/src/ipc/protocol.ts:157-161` (`RegistryHintResult`)
- Modify: `extension/src/config.ts` (add `verboseRegistryLogging`)
- Modify: root `package.json` (contributes → `importLens.verboseRegistryLogging`)
- Modify: `extension/src/daemon/nativeTransport.ts:533-535` (name packages)
- Modify: `extension/src/guidance/registryRefresh.ts` (summary + per-package logging; inject `isVerbose`)
- Modify: `extension/src/guidance/packageJsonAnalysis.ts:47` (pass the verbose getter)
- Modify: `extension/test/guidance/registryRefresh.test.ts` (add coverage)

**Interfaces:**
- Consumes: `RegistryHintResult.origin?: "cache" | "network"` (Task 4) and `getImportLensConfig().verboseRegistryLogging: boolean`.
- Produces: `new RegistryHintRefresher(daemon, host, logger, isVerbose?)` — 4th optional arg, default `() => false` (keeps existing 3-arg tests valid).

- [ ] **Step 1: Add the optional field to the TS protocol type**

In `extension/src/ipc/protocol.ts`, `RegistryHintResult` becomes:

```ts
export interface RegistryHintResult {
  target: RegistryHintTarget;
  hint?: RegistryHint | null;
  error?: string | null;
  origin?: "cache" | "network";
}
```

- [ ] **Step 2: Add the config field**

In `extension/src/config.ts`: add `verboseRegistryLogging: boolean;` to `ImportLensConfig` (after `enableRegistryHints`), and in `getImportLensConfig()` add:

```ts
    verboseRegistryLogging: config.get("verboseRegistryLogging", false),
```

In root `package.json`, add next to `importLens.enableRegistryHints` (~line 177-181):

```json
        "importLens.verboseRegistryLogging": {
          "type": "boolean",
          "default": false,
          "description": "Log every package's registry refresh outcome (cache hit vs network fetch). Noisy on large dependency lists; enable only when diagnosing refresh behavior."
        },
```

Note: this key is intentionally classified `uiOnly` by `classifyImportLensConfigChange` (it falls through the `importLens` catch-all), so toggling it never restarts the daemon; it takes effect immediately because the refresher reads the getter live.

- [ ] **Step 3: Name packages in the transport request log**

In `extension/src/daemon/nativeTransport.ts`, replace the debug log at lines 533-535:

```ts
    const names = request.targets.map((target) => target.name);
    const preview =
      names.length <= 8 ? names.join(", ") : `${names.slice(0, 8).join(", ")}, +${names.length - 8} more`;
    this.#logger.debug(
      `Requesting registry hint refresh ${request.request_id} for ${request.targets.length} package(s): ${preview}.`,
    );
```

- [ ] **Step 4: Write the failing summary/verbose test**

Extend `extension/test/guidance/registryRefresh.test.ts` (uses the file's existing `createHarness`, `stateFor`, `targetFor` helpers). Add a capturing logger and two tests:

```ts
test("logs a cache/network/failed summary from result origins", async () => {
  const a = stateFor("a");
  const b = stateFor("b");
  const c = stateFor("c");
  const harness = createHarness([a, b, c]);
  const messages: string[] = [];
  const logger = { debug: (m: string) => void messages.push(m), warn: (): void => undefined };
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) =>
      Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results: [
          { target: request.targets[0], hint: { latestVersion: "1", isLatest: true, fetchedAt: 1 }, error: null, origin: "cache" },
          { target: request.targets[1], hint: { latestVersion: "2", isLatest: false, fetchedAt: 1 }, error: null, origin: "network" },
          { target: request.targets[2], hint: null, error: "boom", origin: "network" },
        ],
        error: null,
        diagnostics: [],
      }),
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, logger);

  await refresher.refresh(uriKey, [targetFor(a), targetFor(b), targetFor(c)], "refresh_stale");

  assert.ok(
    messages.some((m) => m.includes("3 target(s): 1 cached, 1 fetched, 1 failed")),
    `expected a summary line, got: ${messages.join(" | ")}`,
  );
});

test("verbose mode logs per-package cache/network lines", async () => {
  const a = stateFor("a");
  const harness = createHarness([a]);
  const messages: string[] = [];
  const logger = { debug: (m: string) => void messages.push(m), warn: (): void => undefined };
  const daemon: RegistryRefreshTransport = {
    refreshRegistryHints: (request) =>
      Promise.resolve({
        version: request.version,
        request_id: request.request_id,
        results: [
          { target: request.targets[0], hint: { latestVersion: "1", isLatest: true, fetchedAt: 1 }, error: null, origin: "network" },
        ],
        error: null,
        diagnostics: [],
      }),
  };
  const refresher = new RegistryHintRefresher(daemon, harness.host, logger, () => true);

  await refresher.refresh(uriKey, [targetFor(a)], "refresh_stale");

  assert.ok(messages.some((m) => m.includes("fetched (network) for a")), messages.join(" | "));
});
```

- [ ] **Step 5: Run to verify they fail**

Run: `pnpm test:ts`
Expected: FAIL — no summary line; no per-package verbose line.

- [ ] **Step 6: Implement summary + verbose logging and inject the getter**

In `extension/src/guidance/registryRefresh.ts`, add the field + constructor param:

```ts
  readonly #isVerbose: () => boolean;

  constructor(
    daemon: RegistryRefreshTransport,
    host: RegistryRefreshHost<TUri, TState>,
    logger: Pick<Logger, "debug" | "warn">,
    isVerbose: () => boolean = () => false,
  ) {
    this.#daemon = daemon;
    this.#host = host;
    this.#logger = logger;
    this.#isVerbose = isVerbose;
  }
```

Rewrite the loop in `#applyResponse` (lines 131-149) to tally and log:

```ts
  #applyResponse(uri: TUri, generation: number, response: RefreshRegistryHintsResponse): void {
    if (response.error) {
      this.#logger.debug(`Registry hint refresh response failed: ${response.error}`);
    }

    let cached = 0;
    let fetched = 0;
    let failed = 0;
    for (const result of response.results) {
      if (result.error) {
        failed += 1;
        this.#logger.debug(`Registry hint unavailable for ${result.target.name}: ${result.error}`);
      } else if (result.origin === "network") {
        fetched += 1;
        if (this.#isVerbose()) {
          this.#logger.debug(`Registry hint fetched (network) for ${result.target.name}.`);
        }
      } else {
        cached += 1;
        if (this.#isVerbose()) {
          this.#logger.debug(`Registry hint cache hit for ${result.target.name}.`);
        }
      }
      this.#updateRegistryHint(
        uri,
        generation,
        result.target.name,
        result.target.installedVersion,
        result.hint ?? undefined,
        result.error ?? null,
      );
    }

    if (response.results.length > 0) {
      this.#logger.debug(
        `Registry refresh applied ${response.results.length} target(s): ${cached} cached, ${fetched} fetched, ${failed} failed.`,
      );
    }
  }
```

Wire the getter where the refresher is constructed in `packageJsonAnalysis.ts` (line 47) — add the 4th argument:

```ts
    this.#registryRefresher = new RegistryHintRefresher(
      daemon,
      {
        keyFor: (uri) => uri.toString(),
        getStates: (uri) => this.#states.get(uri.toString()),
        setStates: (uri, states) => this.setStates(uri, states),
      },
      logger,
      () => getImportLensConfig().verboseRegistryLogging,
    );
```

- [ ] **Step 7: Run to verify they pass**

Run: `pnpm test:ts`
Expected: PASS (new tests + all existing `registryRefresh` tests, which still pass the 3-arg constructor).

- [ ] **Step 8: Typecheck + lint**

Run: `pnpm check && pnpm lint`
Expected: PASS.

- [ ] **Step 9: Format + commit**

```bash
pnpm format
git add extension/src/ipc/protocol.ts extension/src/config.ts package.json extension/src/daemon/nativeTransport.ts extension/src/guidance/registryRefresh.ts extension/src/guidance/packageJsonAnalysis.ts extension/test/guidance/registryRefresh.test.ts
git commit -m "feat(registry): cache/network summary + verbose logging behind a toggle"
```

---

## Part 5 — End-to-end verification

### Task 8: Verify against a large dependency list

**Files:** none (manual + built artifacts)

- [ ] **Step 1: Build both sides**

Run: `cargo build -p import-lens-daemon --release` and `pnpm build`.
Expected: both succeed. (The extension ships a prebuilt daemon binary under `dist/bin/...`; if the running extension uses that path, copy/refresh the built daemon there per the repo's packaging step before manual testing.)

- [ ] **Step 2: Reproduce the original scenario**

Open a package.json with 100+ deps including `next`, log level `debug`. Confirm in the ImportLens output channel:
- The refresh request log now names packages.
- One `Registry refresh applied N target(s): C cached, F fetched, E failed` summary per analysis instead of ~100 single-target requests.
- `next` logs one permanent-failure warn (`... (permanent, cached 6h)`), then no repeated 5-minute re-fetch. Re-open after a few minutes: it serves the cached failure, not a re-download.
- A daemon `fetched npm metadata for X: 200, <bytes> bytes, <ms>ms` line for packages actually fetched; cache hits stay quiet unless `importLens.verboseRegistryLogging` is enabled (then per-package `cache hit` / `fetched (network)` lines appear).

- [ ] **Step 3: Confirm the focus fix and the invalidation**

Switch away from and back to the package.json tab without editing → no new analysis/refresh burst. Then change any `importLens` setting → the package.json re-analyzes (guard correctly bypassed).

- [ ] **Step 4: Commit any wiring adjustments**

```bash
git add -A
git commit -m "chore(registry): verification adjustments"
```

---

## Self-Review Notes

- **Oversize handling (decision: raise limit + permanent cache):** Task 1 raises the cap to 64 MB; Task 2 caches genuine oversize as permanent (6h) with a single attempt. ✔
- **Focus guard (decision: yes):** Task 6 adds a pure, tested `AnalyzedContentTracker`, guards `analyze`, and invalidates on explicit refresh via `refreshVisibleDocuments()`. ✔
- **Logging (decision: both, behind a toggle):** daemon per-fetch line (Task 3) + extension summary always, per-package cache/network gated by `importLens.verboseRegistryLogging` (Task 7). ✔
- **Fan-out:** Task 5 collapses ~106 IPC refreshes to 1. ✔
- **Cross-project cache (decision: merge-on-persist):** Task 4b. The registry cache already lives in global storage keyed by package name, so sequential projects reuse it; merge-on-persist closes the concurrent-window clobber gap. Only curated metadata (never the multi-MB body) is stored. Heavier options (cross-process single-flight, single shared daemon) intentionally deferred. ✔
- **Testability reconciled with the repo:** extension tests are `node --test`; only vscode-free modules are unit-tested (`AnalyzedContentTracker`, `RegistryHintRefresher`). Controller changes (Task 5, and the wiring in Task 6/7) are integration-verified in Task 8 — no fabricated controller unit tests. ✔
- **Type consistency:** `RegistryHintOrigin` (Rust enum) → `origin: Option<String>` over IPC (`"cache"`/`"network"`) → `origin?: "cache" | "network"` (TS). `AnalyzedContentTracker.{isUnchanged,record,forget}`, `#isVerbose`, `verboseRegistryLogging` consistent across tasks. ✔
- **API verified against source:** ureq 3.3.0 `with_config().limit().read_to_string()`; `RegistryHintService::new(cache, Box<dyn RegistryHttpClient>)` for the injected-client test; `RefreshRegistryHintsResponse` carries `diagnostics: []` (included in test fakes). ✔
- **Open risk:** the exact daemon-binary refresh step for manual testing (Task 8 Step 1) depends on the repo's packaging flow; confirm which daemon path the running extension loads before manual verification.

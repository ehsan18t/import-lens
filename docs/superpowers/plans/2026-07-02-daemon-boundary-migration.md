# Daemon Boundary Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move portable ImportLens domain work that still lives in the VS Code extension host into the Rust daemon, especially npm registry hints and workspace report scanning/aggregation.

**Architecture:** The TypeScript extension host remains responsible for VS Code lifecycle, editor events, decorations, webviews, file watchers, daemon spawning, and UI-only Git/history insights. The Rust daemon owns reusable analysis behavior: parsing, package resolution, package sizing, ignore handling, cache, centralized npm package metadata cache/fetching, and workspace report data production. Package.json analysis derives registry hints from cached package metadata only; live npm refresh uses a separate daemon-owned bounded refresh request that streams one partial response per fetched package, retries transient failures with a hard limit, logs failed fetches, and runs on isolated registry workers. Long registry/report jobs send responses through an outbound IPC writer queue so the daemon can keep reading foreground analysis requests while background work continues.

**Tech Stack:** TypeScript 6.x, VS Code API, MessagePack IPC, Rust 2024, Tokio IPC server, Rayon analysis, serde/serde_json, redb/papaya existing caches, `ureq` for bounded daemon registry HTTPS requests.

> **Review amendments (2026-07-02):** This plan was reviewed against the live codebase. Corrections folded in: (1) delete the now-orphaned `extension/src/report/concurrency.ts` and cover it in the removal greps; (2) the registry rate limiter no longer sleeps while holding its mutex; (3) the registry cache persists via atomic temp-file + rename; (4) the IPC outbound queue reuses the existing `send_message!`/`message_frame_codec()` writer instead of a separately-typed `Framed` helper; (5) a missing `current_time_millis()` helper is defined in `server.rs`. Verified sound as written: all referenced symbols exist, protocol is currently v6 (the v7 bump is correct), and `effective_registry_hint_mode` already defines precedence between `registry_hint_mode` and the legacy boolean fields.

---

## Scope Check

This plan implements two connected boundary moves in one branch because both are about editor-portable daemon ownership:

1. Registry latest/deprecation hints move from `extension/src/guidance/registryHints.ts` into a daemon registry module and daemon IPC. The daemon caches normalized npm package metadata by package name, not derived hints by package/version. The daemon must not repeat the old blocking design: `AnalyzePackageJsonRequest` may derive cached hints from cached package metadata, but live npm fetches happen only through `RefreshRegistryHintsRequest` and must stream partials progressively.
2. Workspace report scanning and summary aggregation move from `extension/src/report/*.ts` into a daemon report module and daemon IPC.
3. Obsolete TypeScript domain implementations are removed after the daemon paths are wired. TypeScript keeps only protocol types, transport forwarding, VS Code state orchestration, and UI rendering for these features.

The following remain in TypeScript because they are editor integration rather than reusable domain work:

- VS Code `FileSystemWatcher` registration in `extension/src/watcher.ts`.
- Text-document debounce and stale request rejection in `extension/src/listener.ts`.
- Daemon process lifecycle, binary hash validation, sockets, storage-path selection, and crash backoff in `extension/src/daemon/*`.
- UI rendering, decorations, hovers, CodeLens, webviews, command registration, and clipboard interaction in `extension/src/ui/*`.
- Git working-tree diff and VS Code `globalState` history insight enrichment in `extension/src/analysis/*`; the SRS already records this as extension-side editor context.
- Loose-file daemon start-root bootstrapping in `extension/src/workspaceContext.ts`; the extension still needs a root before it can spawn the daemon.

## Requirement Notes For Approval

These are the requirement corrections or sharper interpretations I recommend before implementation:

1. "Use multi-thread" should mean "use isolated bounded worker pools plus nonblocking IPC dispatch." A plain async task or generic blocking task is threaded, but it can still compete with normal daemon work, and the daemon can still be blocked if the IPC read loop waits for the job. This plan uses a dedicated `RegistryRefreshExecutor`, a bounded report executor, and an outbound response queue.
2. "Show the pkg that has been fetched" should include three states: cached hint shown by package.json analysis, live refresh success applied as a partial response, and live refresh failure logged plus returned as a per-package error while keeping stale cached data visible with a stale indicator.
3. Retry should not apply to every failure. The daemon should retry transient network failures plus HTTP 408, 425, 429, and 5xx; it should not retry 404 package-not-found responses or malformed package names.
4. Logging every retry at warning level would make noisy logs on slow networks. This plan logs retry attempts at debug level, rate limits and final failures at warning level, and returns per-package errors to the extension.
5. "Remove all old TS code" should remove old TS domain implementations and dependencies, not editor integration. The plan deletes the TS npm fetch/cache module, old report scanner/model modules, their tests, and `p-queue`; it keeps VS Code UI, commands, config, protocol types, and daemon transport forwarding in TypeScript.
6. Moving registry network work to the daemon is a deliberate SRS change. The original SRS prohibited daemon network access, so Task 1 narrows that rule to forbid network during size analysis while allowing only the bounded registry refresh endpoint.
7. Manual registry refresh means `ForceRefresh`: bypass freshness TTLs and cached retry windows, fetch npm metadata when no same-package fetch is already running, then update the centralized package metadata cache. If a same-package fetch is already active, the manual request joins that in-flight fetch instead of starting duplicate network work.
8. Workspace report generation should still move to the daemon, but it should use a bounded report executor instead of a generic blocking task. That gives the best balance: daemon-owned portable report logic, faster native filesystem traversal, stable UI behavior for VS Code, and a cleaner path for editors such as Zed.

## File Structure

### Rust Registry Module

- Create `daemon/src/registry/mod.rs`
  - Re-export the registry service API.
- Create `daemon/src/registry/constants.rs`
  - Store TTL, timeout, retry, concurrency, and queue constants.
- Create `daemon/src/registry/types.rs`
  - Store normalized package metadata, cache-entry, and lookup types separate from service logic.
- Create `daemon/src/registry/client.rs`
  - Define a small HTTP client trait plus the `ureq` implementation.
- Create `daemon/src/registry/cache.rs`
  - Own daemon-side persistent JSON package metadata cache under the extension-managed daemon cache base.
- Create `daemon/src/registry/service.rs`
  - Own package-level refresh modes, in-flight de-duplication, retry behavior, failure logging, stale fallback, and conversion to per-installed-version `RegistryHint`.
- Create `daemon/src/registry/executor.rs`
  - Own a small dedicated registry worker thread pool so npm refresh cannot consume the daemon's foreground analysis workers or IPC runtime.
- Modify `daemon/src/lib.rs`
  - Export `registry`.
- Modify `daemon/Cargo.toml`
  - Add `ureq`.

### Rust Protocol And Service

- Modify `daemon/src/ipc/protocol.rs`
  - Bump protocol to v7.
  - Add registry refresh request/response types.
  - Add workspace report request/response types.
  - Add optional `registry_hint_mode` to package.json analysis.
- Modify `daemon/src/ipc/server.rs`
  - Add handlers for registry refresh and workspace report requests.
- Modify `daemon/src/service.rs`
  - Store a `RegistryHintService`.
  - Attach cached registry hints during package.json analysis when requested.
  - Expose a per-target registry refresh helper used by the IPC server's bounded registry worker path.
  - Expose `build_workspace_report`.

### Rust Report Module

- Create `daemon/src/report/mod.rs`
  - Re-export report API.
- Create `daemon/src/report/executor.rs`
  - Own a bounded report worker pool for command-triggered workspace reports.
- Create `daemon/src/report/scanner.rs`
  - Recursively scan supported source files from a workspace root while skipping configured directories.
- Create `daemon/src/report/model.rs`
  - Build report rows, duplicate import groups, duplicate module groups, and treemap data from daemon analysis items.

### TypeScript Protocol And Transport

- Modify `extension/src/ipc/protocol.ts`
  - Bump protocol to v7.
  - Mirror new registry refresh and workspace report protocol types.
- Modify `extension/src/daemon/transport.ts`
  - Add transport methods for registry refresh and workspace report.
- Modify `extension/src/daemon/manager.ts`
  - Forward the new transport methods.
- Modify `extension/src/daemon/nativeTransport.ts`
  - Send new IPC requests through `IpcClient`.
- Modify `extension/src/ipc/client.ts`
  - Add request helpers and response routing for the new response shapes.

### TypeScript Package.json Guidance

- Modify `extension/src/guidance/packageJsonState.ts`
  - Add local stale-refresh state used by VS Code UI.
- Modify `extension/src/guidance/packageJsonAnalysis.ts`
  - Remove direct npm fetch/cache calls.
  - Ask daemon for cached registry hints in package.json analysis.
  - Ask daemon for stale or forced registry refreshes after states are known.
- Modify `extension/src/guidance/packageJsonPartial.ts`
  - Preserve current "newer hint wins" merge behavior for daemon registry partials.
- Modify `extension/src/ui/packageJsonLabels.ts`
  - Add stale registry suffix labels.
- Modify `extension/src/ui/packageJsonHintVisuals.ts`
  - Add warning color for stale registry suffixes.
- Modify `extension/src/ui/packageJsonHintSegments.ts`
  - Route stale registry suffixes through the existing inline hint caution tone.
- Modify `extension/src/ui/packageJsonTooltip.ts`
  - Explain when cached registry data is being shown because live refresh failed.
- Delete `extension/src/guidance/registryHints.ts`.
- Delete `extension/test/guidance/registryHints.test.ts`.
- Modify `package.json`
  - Remove `p-queue` from dependencies.

### TypeScript Workspace Report UI

- Modify `extension/src/ui/report.ts`
  - Request report rows and summary from daemon.
  - Keep HTML/webview rendering in TS.
- Delete `extension/src/report/workspaceScanner.ts`.
- Delete `extension/src/report/reportModel.ts`.
- Delete `extension/src/report/concurrency.ts` (its `mapWithConcurrency` helper is only used by `workspaceScanner.ts`; it becomes dead code once scanning moves to the daemon).
- Delete `extension/test/report/workspaceScanner.test.ts`.
- Delete `extension/test/report/reportModel.test.ts`.

### Documentation And Generated Artifacts

- Modify `docs/ImportLens-SRS.md`
  - Registry network work becomes daemon-owned and bounded.
  - Daemon gains read-only workspace-source scanning for report generation.
  - Protocol v7 describes registry refresh and workspace report endpoints.
  - `p-queue` is removed from runtime npm dependencies.
- Modify `extension/src/daemon/knownHashes.generated.ts`
  - Regenerated by `pnpm package:win32-x64`.

---

## Task 1: Update SRS And Protocol Boundary Contract

**Files:**
- Modify: `docs/ImportLens-SRS.md`

- [ ] **Step 1: Update daemon responsibility prose**

Replace the current SRS statements that say registry refresh is extension-host owned with daemon-owned bounded refresh behavior.

Use these exact behavior rules:

```markdown
Registry latest/deprecation metadata is daemon-owned. The extension host never calls the npm registry directly. The daemon maintains a centralized normalized npm package metadata cache keyed by package name. Package.json dependency analysis may request cached registry hints from the daemon without network I/O; the daemon derives each per-installed-version hint from the cached package metadata. A separate registry refresh request asks the daemon to fetch npm metadata only when the package metadata cache is missing or expired. Automatic refreshes respect freshness TTLs and cached retry windows. Manual refreshes use `force_refresh`, bypass TTL and retry-window checks, and fetch from npm unless the same package already has an active in-flight fetch to join. Refresh uses bounded concurrency, shared interval rate limiting, short timeouts, retry-after handling, hard retry limits, per-package failure isolation, per-package failure logging, and daemon-owned persistent cache storage under the extension-managed daemon cache base. The refresh request streams one partial response for each completed package so the extension can update visible package rows as soon as each registry result is available. If live refresh fails but cached metadata exists, the daemon returns both the cached hint and a per-package error; editors must keep the cached hint visible and mark it stale.
```

- [ ] **Step 2: Replace the daemon no-network rule**

Change `NFR-011` from "The daemon must make no outbound network connections" to this narrower rule:

```markdown
**NFR-011** (Critical) - The daemon must make no outbound network connections during import size computation, package resolution, module graph construction, tree-shaking, minification, compression, cache lookup, or cache invalidation. The only permitted outbound network path is the registry-hint refresh endpoint, which may call the public npm registry when `importLens.enableRegistryHints` is enabled and a client explicitly requests stale or forced registry refresh. Registry refresh must use centralized package metadata caching, short timeouts, bounded concurrency, shared interval rate limiting, package-level in-flight de-duplication, retry-after handling, hard retry limits, cached retry windows for automatic refresh, manual refresh cache bypass, and stale-cache fallback. Each package failure must be logged and returned as a per-package nullable registry hint result without failing the whole refresh request. A result with both `hint` and `error` means cached metadata is being returned after live refresh failed. Registry refresh must stream partial responses as individual packages finish and must not affect import size computation.
```

- [ ] **Step 3: Add workspace report daemon ownership**

Add this rule near the workspace report requirements:

```markdown
The daemon must own workspace report source scanning and report data aggregation. The extension host may request a workspace report for a workspace root and render the returned report model, but it must not enumerate/open every source file or rebuild duplicate-import/shared-module summaries itself. The request carries the editor's current report budgets so per-import and per-file budget warnings remain user-configurable while the aggregation stays daemon-owned. The daemon scan is read-only, limited to supported source extensions, and skips `node_modules`, `dist`, `build`, `out`, and `coverage` directories.
```

- [ ] **Step 4: Update protocol section to v7**

Update protocol text so `NFR-018` says protocol v7 adds daemon-owned registry refresh and workspace report endpoints on top of protocol v6.

Add these TypeScript-like schemas:

```ts
type RegistryHintMode = "off" | "cached" | "refresh_stale" | "force_refresh";

interface RegistryHintTarget {
  name: string;
  installedVersion?: string;
}

interface RegistryHintResult {
  target: RegistryHintTarget;
  hint?: RegistryHint | null;
  error?: string | null;
}

interface RefreshRegistryHintsRequest {
  type: "refresh_registry_hints";
  version: number;
  request_id: number;
  targets: RegistryHintTarget[];
  mode: "refresh_stale" | "force_refresh";
}

interface RefreshRegistryHintsResponse {
  version: number;
  request_id: number;
  results: RegistryHintResult[];
  indexes?: number[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

interface WorkspaceReportRequest {
  type: "workspace_report";
  version: number;
  request_id: number;
  workspace_root: string;
  budgets?: {
    perImportBrotliBytes?: number;
    perFileBrotliBytes?: number;
  };
}

interface WorkspaceReportResponse {
  version: number;
  request_id: number;
  rows: WorkspaceReportRow[];
  summary: WorkspaceReportSummary;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

Add this streaming contract to the protocol prose:

```markdown
`RefreshRegistryHintsResponse` may be emitted multiple times for the same `request_id`. Partial responses contain `indexes` for the completed target positions and one result per completed package. The final response omits `indexes` and contains the full ordered result set. A package fetch failure sets `RegistryHintResult.error` for that package and leaves `RefreshRegistryHintsResponse.error` null unless the whole request is invalid. When stale cache fallback is available, `RegistryHintResult.hint` contains the cached metadata and `RegistryHintResult.error` contains the live refresh failure reason.
```

- [ ] **Step 5: Run documentation grep**

Run:

```powershell
rg -n "registry refresh remains extension-host-owned|daemon must not perform live registry fetches|p-queue|NFR-011|Protocol v6" docs/ImportLens-SRS.md
```

Expected: no old extension-owned registry wording remains; `NFR-011` contains only the narrowed daemon-network rule; protocol section names v7.

- [ ] **Step 6: Commit SRS boundary update**

```powershell
git add docs/ImportLens-SRS.md
git commit -m "docs: move registry and report ownership to daemon"
```

---

## Task 2: Add Daemon Registry Types, Constants, Cache, And HTTP Client

**Files:**
- Create: `daemon/src/registry/mod.rs`
- Create: `daemon/src/registry/constants.rs`
- Create: `daemon/src/registry/types.rs`
- Create: `daemon/src/registry/client.rs`
- Create: `daemon/src/registry/cache.rs`
- Create: `daemon/src/registry/service.rs`
- Create: `daemon/src/registry/executor.rs`
- Modify: `daemon/src/lib.rs`
- Modify: `daemon/Cargo.toml`
- Test: `daemon/tests/registry.rs`

- [ ] **Step 1: Add failing registry metadata/cache tests**

Create `daemon/tests/registry.rs` with tests that do not touch the real network:

```rust
use import_lens_daemon::{
    ipc::protocol::RegistryHint,
    registry::{
        cache::RegistryMetadataCache,
        constants::FRESH_HINT_TTL_MS,
        service::{RegistryHintMode, RegistryHintService},
        types::{HttpRegistryResponse, RegistryHttpClient, RegistryPackageMetadata},
    },
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::Duration,
};

#[derive(Clone, Default)]
struct FakeRegistryHttpClient {
    calls: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<Vec<Result<HttpRegistryResponse, String>>>>,
}

impl FakeRegistryHttpClient {
    fn with_response(response: HttpRegistryResponse) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(vec![Ok(response)])),
        }
    }

    fn with_responses(responses: Vec<Result<HttpRegistryResponse, String>>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RegistryHttpClient for FakeRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(package_name.to_owned());
        self.responses
            .lock()
            .expect("responses lock")
            .remove(0)
    }
}

#[derive(Clone, Default)]
struct SlowRegistryHttpClient {
    calls: Arc<Mutex<Vec<String>>>,
}

impl SlowRegistryHttpClient {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RegistryHttpClient for SlowRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(package_name.to_owned());
        thread::sleep(Duration::from_millis(250));
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{
              "dist-tags":{"latest":"19.0.0"},
              "versions":{"18.2.0":{},"17.0.0":{"deprecated":"legacy release"}},
              "time":{"19.0.0":"2026-06-25T00:00:00.000Z"}
            }"#.to_owned(),
        })
    }
}

fn temp_cache_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "import-lens-registry-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("cache dir");
    path
}

#[test]
fn registry_service_builds_hint_from_metadata() {
    let cache_path = temp_cache_path("metadata");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"19.0.0"},
          "versions":{"18.2.0":{}},
          "time":{"19.0.0":"2026-06-25T00:00:00.000Z"}
        }"#.to_owned(),
    });
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 100);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(lookup.error, None);
    assert_eq!(
        lookup.hint,
        Some(RegistryHint {
            latest_version: Some("19.0.0".to_owned()),
            latest_published_at: Some("2026-06-25T00:00:00.000Z".to_owned()),
            is_latest: Some(false),
            deprecated: Some(false),
            fetched_at: Some(100),
        })
    );
}

#[test]
fn registry_service_uses_cached_metadata_without_network_in_cached_mode() {
    let cache_path = temp_cache_path("cached");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: vec!["17.0.0".to_owned()],
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::default();
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::Cached, 10_000_000);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert!(client.calls().is_empty());
    assert_eq!(lookup.error, None);
    assert_eq!(lookup.hint.and_then(|item| item.latest_version), Some("19.0.0".to_owned()));
}

#[test]
fn registry_service_derives_multiple_version_hints_from_one_cached_package_metadata() {
    let cache_path = temp_cache_path("cached-versions");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: vec!["17.0.0".to_owned()],
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::default();
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let current = service.hint_for("react", Some("18.2.0"), RegistryHintMode::Cached, 10_000_000);
    let deprecated = service.hint_for("react", Some("17.0.0"), RegistryHintMode::Cached, 10_000_000);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert!(client.calls().is_empty());
    assert_eq!(current.hint.and_then(|item| item.deprecated), Some(false));
    assert_eq!(deprecated.hint.and_then(|item| item.deprecated), Some(true));
}

#[test]
fn registry_service_force_refresh_bypasses_fresh_cache() {
    let cache_path = temp_cache_path("force-refresh");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"20.0.0"},
          "versions":{"18.2.0":{}},
          "time":{"20.0.0":"2026-07-01T00:00:00.000Z"}
        }"#.to_owned(),
    });
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("20.0.0".to_owned()),
    );
}

#[test]
fn registry_service_refreshes_expired_package_metadata() {
    let cache_path = temp_cache_path("expired");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 200,
        retry_after_ms: None,
        body: r#"{
          "dist-tags":{"latest":"20.0.0"},
          "versions":{"18.2.0":{}},
          "time":{"20.0.0":"2026-07-01T00:00:00.000Z"}
        }"#.to_owned(),
    });
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for(
        "react",
        Some("18.2.0"),
        RegistryHintMode::RefreshStale,
        51 + FRESH_HINT_TTL_MS,
    );

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("20.0.0".to_owned()),
    );
}

#[test]
fn registry_service_persists_retry_window_for_rate_limits() {
    let cache_path = temp_cache_path("retry-after");
    let client = FakeRegistryHttpClient::with_response(HttpRegistryResponse {
        status: 429,
        retry_after_ms: Some(1_000),
        body: r#"{"error":"rate limited"}"#.to_owned(),
    });
    let service = RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    );

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 100);
    let second = service.hint_for("react", Some("18.2.0"), RegistryHintMode::RefreshStale, 500);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert_eq!(lookup.hint, None);
    assert_eq!(lookup.error.as_deref(), Some("npm registry rate limit"));
    assert_eq!(second.hint, None);
    assert_eq!(second.error.as_deref(), Some("npm registry rate limit"));
}

#[test]
fn registry_service_retries_transient_failures_and_returns_stale_hint_with_error() {
    let cache_path = temp_cache_path("transient");
    let cache = RegistryMetadataCache::new(cache_path.clone());
    cache
        .write_metadata(
            "react",
            RegistryPackageMetadata {
                latest_version: Some("19.0.0".to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            50,
        )
        .expect("cache write");
    let client = FakeRegistryHttpClient::with_responses(vec![
        Err("temporary registry failure 1".to_owned()),
        Err("temporary registry failure 2".to_owned()),
        Err("temporary registry failure 3".to_owned()),
    ]);
    let service = RegistryHintService::new(cache, Box::new(client.clone()));

    let lookup = service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 10_000);

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react", "react", "react"]);
    assert_eq!(
        lookup.hint.and_then(|item| item.latest_version),
        Some("19.0.0".to_owned()),
    );
    assert_eq!(lookup.error.as_deref(), Some("temporary registry failure 3"));
}

#[test]
fn registry_service_dedupes_in_flight_duplicate_targets() {
    let cache_path = temp_cache_path("singleflight");
    let client = SlowRegistryHttpClient::default();
    let service = Arc::new(RegistryHintService::new(
        RegistryMetadataCache::new(cache_path.clone()),
        Box::new(client.clone()),
    ));
    let start = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let service = Arc::clone(&service);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                service.hint_for("react", Some("18.2.0"), RegistryHintMode::ForceRefresh, 100)
            })
        })
        .collect::<Vec<_>>();

    start.wait();
    let lookups = handles
        .into_iter()
        .map(|handle| handle.join().expect("registry lookup should not panic"))
        .collect::<Vec<_>>();

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(client.calls(), vec!["react"]);
    assert!(lookups.iter().all(|lookup| lookup.error.is_none()));
    assert!(lookups
        .iter()
        .all(|lookup| lookup.hint.as_ref().and_then(|hint| hint.latest_version.as_deref()) == Some("19.0.0")));
}

#[test]
fn registry_cache_persists_latest_snapshot_under_concurrent_writes() {
    let cache_path = temp_cache_path("concurrent-cache");
    let cache = Arc::new(RegistryMetadataCache::new(cache_path.clone()));
    let handles = (0..16)
        .map(|index| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                cache
                    .write_metadata(
                        &format!("pkg-{index}"),
                        RegistryPackageMetadata {
                            latest_version: Some("2.0.0".to_owned()),
                            latest_published_at: None,
                            deprecated_versions: Vec::new(),
                        },
                        index,
                    )
                    .expect("concurrent cache write");
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("cache writer should not panic");
    }

    let persisted = fs::read_to_string(cache_path.join("registry-metadata.json")).expect("cache file");
    let value = serde_json::from_str::<serde_json::Value>(&persisted).expect("cache json");

    fs::remove_dir_all(cache_path).expect("cache cleanup");
    assert_eq!(value.as_object().expect("cache object").len(), 16);
}
```

- [ ] **Step 2: Run the new test and verify it fails**

Run:

```powershell
cargo test -p import-lens-daemon --test registry
```

Expected: FAIL because `registry` module does not exist.

- [ ] **Step 3: Add registry module exports**

Create `daemon/src/registry/mod.rs`:

```rust
pub mod cache;
pub mod client;
pub mod constants;
pub mod executor;
pub mod service;
pub mod types;
```

Modify `daemon/src/lib.rs`:

```rust
pub mod cache;
pub mod document;
pub mod ipc;
pub mod lifecycle;
pub mod logging;
pub mod pipeline;
pub mod prefetch;
pub mod registry;
pub mod service;
```

- [ ] **Step 4: Add registry constants**

Create `daemon/src/registry/constants.rs`:

```rust
pub const FRESH_HINT_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const NOT_FOUND_TTL_MS: u64 = 6 * 60 * 60 * 1000;
pub const TRANSIENT_ERROR_RETRY_MS: u64 = 5 * 60 * 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 3_000;
pub const MAX_ATTEMPTS: usize = 3;
pub const REGISTRY_REFRESH_CONCURRENCY: usize = 4;
pub const REGISTRY_RATE_LIMIT_REQUESTS: usize = 20;
pub const REGISTRY_RATE_LIMIT_WINDOW_MS: u64 = 1_000;
pub const REGISTRY_RETRY_BASE_DELAY_MS: u64 = 100;
pub const REGISTRY_CACHE_FILE_NAME: &str = "registry-metadata.json";
```

- [ ] **Step 5: Add dedicated registry worker pool**

Create `daemon/src/registry/executor.rs`:

```rust
use rayon::{ThreadPool, ThreadPoolBuilder};

pub struct RegistryRefreshExecutor {
    pool: ThreadPool,
}

impl RegistryRefreshExecutor {
    pub fn new(thread_count: usize) -> Self {
        let pool = ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .thread_name(|index| format!("import-lens-registry-{index}"))
            .build()
            .expect("registry refresh thread pool should build");
        Self { pool }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.pool.spawn(job);
    }
}
```

This is a dedicated Rayon pool, not the daemon's global Rayon analysis pool. Registry refresh work must enter through this executor so npm latency cannot consume foreground analysis workers or the Tokio IPC runtime.

- [ ] **Step 6: Add registry types**

Create `daemon/src/registry/types.rs`:

```rust
use crate::ipc::protocol::RegistryHint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPackageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_published_at: Option<String>,
    #[serde(default)]
    pub deprecated_versions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPackageMetadataEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RegistryPackageMetadata>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub not_found: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRegistryResponse {
    pub status: u16,
    pub retry_after_ms: Option<u64>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHintLookup {
    pub hint: Option<RegistryHint>,
    pub error: Option<String>,
}

pub trait RegistryHttpClient: Send + Sync {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String>;
}
```

- [ ] **Step 7: Add ureq dependency**

Modify `daemon/Cargo.toml`:

```toml
ureq = { version = "^3.3", default-features = false, features = ["rustls"] }
```

- [ ] **Step 8: Add HTTP client wrapper**

Create `daemon/src/registry/client.rs`:

```rust
use super::{constants::DEFAULT_TIMEOUT_MS, types::HttpRegistryResponse};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UreqRegistryHttpClient {
    timeout_ms: u64,
}

impl Default for UreqRegistryHttpClient {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl UreqRegistryHttpClient {
    pub fn new(timeout_ms: u64) -> Self {
        Self { timeout_ms }
    }
}

impl super::types::RegistryHttpClient for UreqRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        let url = registry_url(package_name);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(self.timeout_ms)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .header("accept", "application/vnd.npm.install-v1+json, application/json")
            .call()
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let retry_after_ms = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(retry_after_delay_ms);
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?;

        Ok(HttpRegistryResponse {
            status,
            retry_after_ms,
            body,
        })
    }
}

fn registry_url(package_name: &str) -> String {
    if let Some(rest) = package_name.strip_prefix('@') {
        format!("https://registry.npmjs.org/@{}", rest.replace('/', "%2F"))
    } else {
        format!("https://registry.npmjs.org/{package_name}")
    }
}

fn retry_after_delay_ms(header: &str) -> Option<u64> {
    header
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds.max(0.0) * 1000.0).round() as u64)
}
```

- [ ] **Step 9: Add persistent registry cache**

Create `daemon/src/registry/cache.rs`:

```rust
use super::{
    constants::REGISTRY_CACHE_FILE_NAME,
    types::{RegistryPackageMetadata, RegistryPackageMetadataEntry},
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug)]
pub struct RegistryMetadataCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, RegistryPackageMetadataEntry>>,
    persist_lock: Mutex<()>,
}

impl RegistryMetadataCache {
    pub fn new(storage_path: PathBuf) -> Self {
        let path = storage_path.join(REGISTRY_CACHE_FILE_NAME);
        let entries = load_entries(&path);
        Self {
            path,
            entries: Mutex::new(entries),
            persist_lock: Mutex::new(()),
        }
    }

    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Mutex::new(HashMap::new()),
            persist_lock: Mutex::new(()),
        }
    }

    pub fn get(&self, package_name: &str) -> Option<RegistryPackageMetadataEntry> {
        self.entries
            .lock()
            .expect("registry cache lock")
            .get(&cache_key(package_name))
            .cloned()
    }

    pub fn write_entry(
        &self,
        package_name: &str,
        entry: RegistryPackageMetadataEntry,
    ) -> Result<(), String> {
        {
            let mut entries = self.entries.lock().expect("registry cache lock");
            entries.insert(cache_key(package_name), entry);
        }
        self.persist_latest_snapshot()
    }

    pub fn write_metadata(
        &self,
        package_name: &str,
        metadata: RegistryPackageMetadata,
        updated_at: u64,
    ) -> Result<(), String> {
        self.write_entry(
            package_name,
            RegistryPackageMetadataEntry {
                metadata: Some(metadata),
                updated_at,
                retry_after: None,
                error: None,
                not_found: false,
            },
        )
    }

    fn persist_latest_snapshot(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let _persist_guard = self.persist_lock.lock().expect("registry cache persist lock");
        let snapshot = self.entries.lock().expect("registry cache lock").clone();
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
        // Persist atomically: a direct `fs::write` to the live path can truncate the
        // cache if the process crashes mid-write. Write the full last-writer-wins
        // snapshot to a temp file, then rename it over the target.
        let temp_path = self.path.with_extension("json.tmp");
        fs::write(&temp_path, bytes).map_err(|error| error.to_string())?;
        fs::rename(&temp_path, &self.path).map_err(|error| error.to_string())
    }
}

pub fn cache_key(package_name: &str) -> String {
    package_name.to_owned()
}

fn load_entries(path: &Path) -> HashMap<String, RegistryPackageMetadataEntry> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}
```

- [ ] **Step 10: Add registry service**

Create `daemon/src/registry/service.rs`:

```rust
use super::{
    cache::{self, RegistryMetadataCache},
    constants::{
        FRESH_HINT_TTL_MS,
        MAX_ATTEMPTS,
        NOT_FOUND_TTL_MS,
        REGISTRY_RATE_LIMIT_REQUESTS,
        REGISTRY_RATE_LIMIT_WINDOW_MS,
        REGISTRY_RETRY_BASE_DELAY_MS,
        TRANSIENT_ERROR_RETRY_MS,
    },
    types::{
        HttpRegistryResponse,
        RegistryHintLookup,
        RegistryHttpClient,
        RegistryPackageMetadata,
        RegistryPackageMetadataEntry,
    },
};
use crate::{ipc::protocol::RegistryHint, logging};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryHintMode {
    Off,
    Cached,
    RefreshStale,
    ForceRefresh,
}

pub struct RegistryHintService {
    cache: RegistryMetadataCache,
    client: Box<dyn RegistryHttpClient>,
    in_flight: Mutex<HashMap<String, Arc<InflightRegistryPackageFetch>>>,
    rate_limiter: Mutex<RegistryRateLimiter>,
}

struct InflightRegistryPackageFetch {
    result: Mutex<Option<RegistryPackageMetadataEntry>>,
    ready: Condvar,
}

impl InflightRegistryPackageFetch {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

struct RegistryRateLimiter {
    window_started_at: Instant,
    request_count: usize,
}

impl RegistryRateLimiter {
    fn new() -> Self {
        Self {
            window_started_at: Instant::now(),
            request_count: 0,
        }
    }

    /// Reserves a rate-limit slot and returns how long the caller must sleep
    /// *after releasing the lock*. Sleeping while holding the mutex would
    /// serialize every registry worker during backoff and defeat the bounded
    /// concurrency this refresh path is built around.
    fn reserve_slot(&mut self) -> Option<Duration> {
        let window = Duration::from_millis(REGISTRY_RATE_LIMIT_WINDOW_MS);
        let elapsed = self.window_started_at.elapsed();
        if elapsed >= window {
            self.window_started_at = Instant::now();
            self.request_count = 1;
            None
        } else if self.request_count >= REGISTRY_RATE_LIMIT_REQUESTS {
            // Open the next window at the end of the current one and count this
            // caller as its first request, so the slot is reserved before the
            // lock is released and the caller then sleeps lock-free.
            let wait = window - elapsed;
            self.window_started_at = Instant::now() + wait;
            self.request_count = 1;
            Some(wait)
        } else {
            self.request_count += 1;
            None
        }
    }
}

impl RegistryHintService {
    pub fn new(cache: RegistryMetadataCache, client: Box<dyn RegistryHttpClient>) -> Self {
        Self {
            cache,
            client,
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            cache: RegistryMetadataCache::empty(),
            client: Box::new(NoopRegistryHttpClient),
            in_flight: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(RegistryRateLimiter::new()),
        }
    }

    pub fn hint_for(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        mode: RegistryHintMode,
        now_ms: u64,
    ) -> RegistryHintLookup {
        if mode == RegistryHintMode::Off {
            return RegistryHintLookup { hint: None, error: None };
        }

        let cached = self.cache.get(package_name);
        if mode == RegistryHintMode::Cached {
            return cached
                .as_ref()
                .map(|entry| lookup_from_entry(entry, installed_version))
                .unwrap_or(RegistryHintLookup { hint: None, error: None });
        }
        if mode != RegistryHintMode::ForceRefresh {
            if let Some(entry) = cached.as_ref() {
                if mode == RegistryHintMode::RefreshStale && is_usable_without_fetch(entry, now_ms) {
                    return lookup_from_entry(entry, installed_version);
                }
                if entry.retry_after.is_some_and(|retry_after| retry_after > now_ms) {
                    return lookup_from_entry(entry, installed_version);
                }
            }
        }

        let entry = self.fetch_package_singleflight(package_name, now_ms);
        lookup_from_entry(&entry, installed_version)
    }

    fn fetch_package_singleflight(
        &self,
        package_name: &str,
        now_ms: u64,
    ) -> RegistryPackageMetadataEntry {
        let key = cache::cache_key(package_name);
        let (flight, is_owner) = {
            let mut in_flight = self.in_flight.lock().expect("registry in-flight lock");
            if let Some(flight) = in_flight.get(&key) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(InflightRegistryPackageFetch::new());
                in_flight.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if is_owner {
            let result = self.fetch_package_with_retries(package_name, now_ms);
            {
                let mut guard = flight.result.lock().expect("registry in-flight result lock");
                *guard = Some(result.clone());
                flight.ready.notify_all();
            }
            let mut in_flight = self.in_flight.lock().expect("registry in-flight lock");
            if in_flight.get(&key).is_some_and(|current| Arc::ptr_eq(current, &flight)) {
                in_flight.remove(&key);
            }
            return result;
        }

        let mut guard = flight.result.lock().expect("registry in-flight result lock");
        while guard.is_none() {
            guard = flight.ready.wait(guard).expect("registry in-flight wait");
        }
        guard.clone().expect("registry in-flight result")
    }

    fn fetch_package_with_retries(
        &self,
        package_name: &str,
        now_ms: u64,
    ) -> RegistryPackageMetadataEntry {
        let mut last_error = None;
        for attempt in 1..=MAX_ATTEMPTS {
            self.wait_for_rate_limit_slot();
            match self.client.get_package_metadata(package_name) {
                Ok(response) if response.status == 200 => {
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
                    let entry = RegistryPackageMetadataEntry {
                        metadata: Some(metadata),
                        updated_at: now_ms,
                        retry_after: None,
                        error: None,
                        not_found: false,
                    };
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!("failed to persist npm metadata for {package_name}: {error}"),
                        );
                    }
                    return entry;
                }
                Ok(response) if response.status == 404 => {
                    let entry = RegistryPackageMetadataEntry {
                        metadata: None,
                        updated_at: now_ms,
                        retry_after: None,
                        error: None,
                        not_found: true,
                    };
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!("failed to persist npm not-found metadata for {package_name}: {error}"),
                        );
                    }
                    return entry;
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = now_ms + response.retry_after_ms.unwrap_or_else(|| transient_backoff_ms(attempt));
                    logging::log_warn(
                        "registry",
                        format!("npm registry rate limited {package_name}; retry after {retry_after}"),
                    );
                    let entry = failed_entry_from_cache(
                        self.cache.get(package_name).as_ref(),
                        "npm registry rate limit".to_owned(),
                        retry_after,
                    );
                    if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
                        logging::log_warn(
                            "registry",
                            format!("failed to persist npm rate-limit metadata for {package_name}: {error}"),
                        );
                    }
                    return entry;
                }
                Ok(response) => {
                    last_error = Some(format!("npm registry responded with {}", response.status));
                    if attempt == MAX_ATTEMPTS || !is_transient_status(response.status) {
                        break;
                    }
                    logging::log_debug(
                        "registry",
                        format!(
                            "retrying npm metadata fetch for {package_name} after HTTP {} attempt {attempt}",
                            response.status,
                        ),
                    );
                    sleep_before_retry(attempt);
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt == MAX_ATTEMPTS {
                        break;
                    }
                    logging::log_debug(
                        "registry",
                        format!("retrying npm metadata fetch for {package_name} after network failure attempt {attempt}"),
                    );
                    sleep_before_retry(attempt);
                }
            }
        }

        logging::log_warn(
            "registry",
            format!(
                "failed to refresh npm metadata for {package_name} after {MAX_ATTEMPTS} attempt(s): {}",
                last_error.as_deref().unwrap_or("unknown error"),
            ),
        );
        let entry = failed_entry_from_cache(
            self.cache.get(package_name).as_ref(),
            last_error.clone().unwrap_or_else(|| "unknown registry error".to_owned()),
            now_ms + TRANSIENT_ERROR_RETRY_MS,
        );
        if let Err(error) = self.cache.write_entry(package_name, entry.clone()) {
            logging::log_warn(
                "registry",
                format!("failed to persist npm error metadata for {package_name}: {error}"),
            );
        }
        entry
    }

    fn wait_for_rate_limit_slot(&self) {
        let wait = self
            .rate_limiter
            .lock()
            .expect("registry rate limiter lock")
            .reserve_slot();
        if let Some(delay) = wait {
            thread::sleep(delay);
        }
    }
}

struct NoopRegistryHttpClient;

impl RegistryHttpClient for NoopRegistryHttpClient {
    fn get_package_metadata(&self, _package_name: &str) -> Result<HttpRegistryResponse, String> {
        Err("registry client disabled".to_owned())
    }
}

fn is_usable_without_fetch(entry: &RegistryPackageMetadataEntry, now_ms: u64) -> bool {
    if entry.metadata.is_some() {
        return now_ms.saturating_sub(entry.updated_at) <= FRESH_HINT_TTL_MS;
    }
    entry.not_found && now_ms.saturating_sub(entry.updated_at) <= NOT_FOUND_TTL_MS
}

fn lookup_from_entry(
    entry: &RegistryPackageMetadataEntry,
    installed_version: Option<&str>,
) -> RegistryHintLookup {
    RegistryHintLookup {
        hint: entry
            .metadata
            .as_ref()
            .map(|metadata| registry_hint_from_metadata(metadata, installed_version, entry.updated_at)),
        error: entry.error.clone(),
    }
}

fn registry_hint_from_metadata(
    metadata: &RegistryPackageMetadata,
    installed_version: Option<&str>,
    fetched_at: u64,
) -> RegistryHint {
    RegistryHint {
        is_latest: installed_version
            .zip(metadata.latest_version.as_deref())
            .map(|(installed, latest)| installed == latest),
        latest_version: metadata.latest_version.clone(),
        latest_published_at: metadata.latest_published_at.clone(),
        deprecated: installed_version.map(|version| metadata.deprecated_versions.iter().any(|item| item == version)),
        fetched_at: Some(fetched_at),
    }
}

fn package_metadata_from_response(
    response: HttpRegistryResponse,
) -> Result<RegistryPackageMetadata, String> {
    let document = serde_json::from_str::<Value>(&response.body).map_err(|error| error.to_string())?;
    let latest_version = document
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let latest_published_at = latest_version
        .as_ref()
        .and_then(|version| document.get("time").and_then(|time| time.get(version)))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut deprecated_versions = document
        .get("versions")
        .and_then(Value::as_object)
        .map(|versions| {
            versions
                .iter()
                .filter_map(|(version, metadata)| {
                    metadata
                        .get("deprecated")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .map(|_| version.clone())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    deprecated_versions.sort();

    Ok(RegistryPackageMetadata {
        latest_version,
        latest_published_at,
        deprecated_versions,
    })
}

fn failed_entry_from_cache(
    cached: Option<&RegistryPackageMetadataEntry>,
    error: String,
    retry_after: u64,
) -> RegistryPackageMetadataEntry {
    RegistryPackageMetadataEntry {
        metadata: cached.and_then(|entry| entry.metadata.clone()),
        updated_at: cached.map(|entry| entry.updated_at).unwrap_or(0),
        retry_after: Some(retry_after),
        error: Some(error),
        not_found: false,
    }
}

fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 425 || status == 429 || status >= 500
}

fn transient_backoff_ms(attempt: usize) -> u64 {
    REGISTRY_RETRY_BASE_DELAY_MS * attempt as u64
}

fn sleep_before_retry(attempt: usize) {
    thread::sleep(Duration::from_millis(transient_backoff_ms(attempt)));
}
```

- [ ] **Step 11: Run registry tests**

Run:

```powershell
cargo test -p import-lens-daemon --test registry
```

Expected: PASS.

- [ ] **Step 12: Commit registry module**

```powershell
git add daemon/Cargo.toml daemon/src/lib.rs daemon/src/registry daemon/tests/registry.rs Cargo.lock
git commit -m "feat: add daemon registry metadata cache"
```

---

## Task 3: Add Registry Hint IPC And Wire Package.json Analysis To Daemon Cache

**Files:**
- Modify: `daemon/src/ipc/protocol.rs`
- Modify: `daemon/src/ipc/server.rs`
- Modify: `daemon/src/service.rs`
- Test: `daemon/tests/service.rs`
- Test: `daemon/tests/ipc_codec.rs`
- Test: `daemon/tests/server.rs`

- [ ] **Step 1: Add failing service tests for cached registry hints**

Append to `daemon/tests/service.rs`:

```rust
#[test]
fn package_json_analysis_includes_cached_registry_hints_when_requested() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new_with_cache_policy(None, false, 512, 30);
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("tiny-lib", "1.1.0", 100);

    let response = service.handle_analyze_package_json(AnalyzePackageJsonRequest {
        message_type: "analyze_package_json".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 40,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
        source: r#"{"dependencies":{"tiny-lib":"^1.0.0"}}"#.to_owned(),
        streaming: false,
        include_registry_hints: true,
        force_registry_refresh: false,
        refresh_section: None,
        registry_hint_mode: Some(import_lens_daemon::ipc::protocol::RegistryHintMode::Cached),
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None);
    assert_eq!(
        response.states[0]
            .registry_hint
            .as_ref()
            .and_then(|hint| hint.latest_version.as_deref()),
        Some("1.1.0")
    );
}
```

- [ ] **Step 2: Add failing IPC codec tests**

Extend `client_message_decodes_daemon_first_v6_requests` in `daemon/tests/ipc_codec.rs`, rename it to `client_message_decodes_daemon_first_v7_requests`, and set:

```rust
assert_eq!(PROTOCOL_VERSION, 7);
```

Add decode checks:

```rust
assert!(matches!(
    decode_client_message(serde_json::json!({
        "type": "refresh_registry_hints",
        "version": PROTOCOL_VERSION,
        "request_id": 6,
        "targets": [{"name": "react", "installedVersion": "18.2.0"}],
        "mode": "refresh_stale"
    })),
    ClientMessage::RefreshRegistryHints(_),
));
assert!(matches!(
    decode_client_message(serde_json::json!({
        "type": "workspace_report",
        "version": PROTOCOL_VERSION,
        "request_id": 7,
        "workspace_root": "C:/workspace"
    })),
    ClientMessage::WorkspaceReport(_),
));
```

In `daemon/tests/server.rs`, add imports for `RefreshRegistryHintsRequest`, `RefreshRegistryHintsResponse`, `RegistryHintMode`, `RegistryHintTarget`, `RegistryHintService`, `RegistryMetadataCache`, `RegistryHttpClient`, and `HttpRegistryResponse`. Then add a response reader for registry refresh responses:

```rust
struct RegistryRefreshResponseReader {
    decoder: FrameDecoder,
    pending: VecDeque<RefreshRegistryHintsResponse>,
}

impl RegistryRefreshResponseReader {
    fn new() -> Self {
        Self {
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    async fn read_response(&mut self, stream: &mut DuplexStream) -> RefreshRegistryHintsResponse {
        if let Some(response) = self.pending.pop_front() {
            return response;
        }

        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("server response should be readable");
            assert!(read > 0, "server closed before writing response");
            for payload in self.decoder.push(&buffer[..read]).expect("server frame should decode") {
                self.pending.push_back(
                    decode_payload::<RefreshRegistryHintsResponse>(&payload)
                        .expect("registry refresh response should decode"),
                );
            }
            if let Some(response) = self.pending.pop_front() {
                return response;
            }
        }
    }
}
```

Add a server streaming test using a fake registry client:

```rust
#[tokio::test]
async fn server_streams_registry_hint_partials_before_final_response() {
    let workspace = temp_workspace();
    let (mut client_stream, server_stream) = duplex(64 * 1024);
    let registry_hints = RegistryHintService::new(
        RegistryMetadataCache::empty(),
        Box::new(DelayedRegistryClient),
    );
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new_with_registry_hints_for_tests(registry_hints)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut reader = RegistryRefreshResponseReader::new();

    client_stream.write_all(&encode_frame(&hello(&workspace)).expect("hello should encode")).await.expect("hello should be written");
    client_stream.write_all(&encode_frame(&RefreshRegistryHintsRequest {
        message_type: "refresh_registry_hints".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 8,
        targets: vec![
            RegistryHintTarget { name: "fast-lib".to_owned(), installed_version: Some("1.0.0".to_owned()) },
            RegistryHintTarget { name: "slow-lib".to_owned(), installed_version: Some("1.0.0".to_owned()) },
            RegistryHintTarget { name: "fail-lib".to_owned(), installed_version: Some("1.0.0".to_owned()) },
        ],
        mode: RegistryHintMode::RefreshStale,
    }).expect("registry refresh request should encode")).await.expect("request should be written");

    let first_partial = tokio::time::timeout(Duration::from_millis(200), reader.read_response(&mut client_stream))
        .await
        .expect("first registry partial should arrive before the slow package finishes");
    assert_eq!(first_partial.request_id, 8);
    assert!(first_partial.indexes.is_some());
    assert_eq!(first_partial.results.len(), 1);

    let early_final = tokio::time::timeout(Duration::from_millis(20), reader.read_response(&mut client_stream)).await;
    assert!(early_final.is_err(), "final response should not be buffered with the first partial");

    let final_response = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = reader.read_response(&mut client_stream).await;
            if response.indexes.is_none() {
                return response;
            }
        }
    })
    .await
    .expect("final registry refresh response should arrive");
    assert_eq!(final_response.results.len(), 3);
    assert!(final_response.results.iter().any(|result| result.target.name == "fail-lib" && result.error.is_some()));
    assert!(final_response.results.iter().any(|result| result.target.name == "fast-lib" && result.hint.is_some()));

    client_stream.write_all(&encode_frame(&ShutdownMessage { message_type: "shutdown".to_owned() }).expect("shutdown should encode")).await.expect("shutdown should be written");
    server.await.expect("server task should join").expect("server should exit cleanly");
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}
```

Add the fake client in the same test file:

```rust
struct DelayedRegistryClient;

impl RegistryHttpClient for DelayedRegistryClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        if package_name == "slow-lib" {
            std::thread::sleep(Duration::from_millis(300));
        }
        if package_name == "fail-lib" {
            return Err("simulated registry failure".to_owned());
        }
        Ok(HttpRegistryResponse {
            status: 200,
            retry_after_ms: None,
            body: r#"{"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{}},"time":{"2.0.0":"2026-01-01T00:00:00.000Z"}}"#.to_owned(),
        })
    }
}
```

- [ ] **Step 3: Run daemon tests and verify failure**

Run:

```powershell
cargo test -p import-lens-daemon --test ipc_codec --test service --test server
```

Expected: FAIL because protocol v7, new message variants, registry refresh streaming, and registry test injection do not exist.

- [ ] **Step 4: Bump protocol and add registry protocol types**

In `daemon/src/ipc/protocol.rs`, change:

```rust
pub const PROTOCOL_VERSION: u32 = 7;
```

Add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHintMode {
    Off,
    Cached,
    RefreshStale,
    ForceRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintTarget {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintResult {
    pub target: RegistryHintTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<RegistryHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsRequest {
    #[serde(rename = "type")]
    #[serde(default = "refresh_registry_hints_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub targets: Vec<RegistryHintTarget>,
    pub mode: RegistryHintMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsResponse {
    pub version: u32,
    pub request_id: u64,
    pub results: Vec<RegistryHintResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}
```

Add `registry_hint_mode` to `AnalyzePackageJsonRequest`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub registry_hint_mode: Option<RegistryHintMode>,
```

Add `RefreshRegistryHints(RefreshRegistryHintsRequest)` to `ClientMessage`, `TypedClientMessage`, and conversions.

Add:

```rust
fn refresh_registry_hints_message_type() -> String {
    "refresh_registry_hints".to_owned()
}
```

- [ ] **Step 5: Convert protocol registry mode to service registry mode**

In `daemon/src/service.rs`, extend the `crate::ipc::protocol` imports with `RegistryHintMode as ProtocolRegistryHintMode`, `RegistryHintResult`, and `RegistryHintTarget`, then add helper:

```rust
fn effective_registry_hint_mode(request: &AnalyzePackageJsonRequest) -> crate::registry::service::RegistryHintMode {
    match request.registry_hint_mode {
        Some(ProtocolRegistryHintMode::Off) => crate::registry::service::RegistryHintMode::Off,
        Some(ProtocolRegistryHintMode::Cached) => crate::registry::service::RegistryHintMode::Cached,
        Some(ProtocolRegistryHintMode::RefreshStale) => crate::registry::service::RegistryHintMode::RefreshStale,
        Some(ProtocolRegistryHintMode::ForceRefresh) => crate::registry::service::RegistryHintMode::ForceRefresh,
        None if request.force_registry_refresh => crate::registry::service::RegistryHintMode::ForceRefresh,
        None if request.include_registry_hints => crate::registry::service::RegistryHintMode::Cached,
        None => crate::registry::service::RegistryHintMode::Off,
    }
}
```

- [ ] **Step 6: Store registry service in ImportLensService**

Remove `#[derive(Debug)]` from `ImportLensService`; the registry service and worker executors contain trait objects/thread pools that should not drive debug trait bounds.

Modify `ImportLensService`:

```rust
pub struct ImportLensService {
    cache_registry: ProjectCacheRegistry,
    registry_hints: crate::registry::service::RegistryHintService,
    registry_executor: crate::registry::executor::RegistryRefreshExecutor,
}
```

In `new_with_cache_policy`, initialize with storage-backed registry cache when `storage_path` exists:

```rust
let cache_registry = ProjectCacheRegistry::new(
    storage_path.clone(),
    enable_disk_cache,
    cache_max_size_mb,
    cache_max_age_days,
);
let registry_hints = storage_path
    .clone()
    .map(|path| crate::registry::service::RegistryHintService::new(
        crate::registry::cache::RegistryMetadataCache::new(path),
        Box::new(crate::registry::client::UreqRegistryHttpClient::default()),
    ))
    .unwrap_or_else(crate::registry::service::RegistryHintService::disabled);
let registry_executor = crate::registry::executor::RegistryRefreshExecutor::new(
    crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
);
Self {
    cache_registry,
    registry_hints,
    registry_executor,
}
```

- [ ] **Step 7: Attach registry hints to package.json states**

Inside `analyze_package_json`, compute:

```rust
let registry_hint_mode = effective_registry_hint_mode(&request);
let now_ms = current_time_millis();
```

When creating each `PackageJsonDependencyAnalysisItem`, set:

```rust
registry_hint: self.registry_hints
    .hint_for(&entry.name, Some(&version), registry_hint_mode, now_ms)
    .hint,
```

Add helper:

```rust
fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
```

- [ ] **Step 8: Add per-target registry refresh service method**

In `ImportLensService`, add:

```rust
pub fn refresh_registry_hint_target(
    &self,
    target: RegistryHintTarget,
    mode: ProtocolRegistryHintMode,
    now_ms: u64,
) -> RegistryHintResult {
    let service_mode = match mode {
        ProtocolRegistryHintMode::RefreshStale => {
            crate::registry::service::RegistryHintMode::RefreshStale
        }
        ProtocolRegistryHintMode::ForceRefresh => {
            crate::registry::service::RegistryHintMode::ForceRefresh
        }
        ProtocolRegistryHintMode::Off | ProtocolRegistryHintMode::Cached => crate::registry::service::RegistryHintMode::Cached,
    };

    let lookup = self.registry_hints.hint_for(
        &target.name,
        target.installed_version.as_deref(),
        service_mode,
        now_ms,
    );

    RegistryHintResult {
        target,
        hint: lookup.hint,
        error: lookup.error,
    }
}

pub fn spawn_registry_refresh(&self, job: impl FnOnce() + Send + 'static) {
    self.registry_executor.spawn(job);
}
```

Do not use the global Rayon analysis pool for live registry refresh. Registry network work must run through `RegistryRefreshExecutor`, which owns a separate bounded worker pool, so npm latency cannot starve foreground import analysis work.

- [ ] **Step 9: Add test-only registry helper**

Under `#[cfg(test)]` in `daemon/src/service.rs`, add:

```rust
pub struct RegistryHintTestHandle<'a> {
    service: &'a ImportLensService,
}

impl ImportLensService {
    #[cfg(test)]
    pub fn registry_hints_for_tests(&self) -> RegistryHintTestHandle<'_> {
        RegistryHintTestHandle { service: self }
    }

    #[cfg(test)]
    pub fn new_with_registry_hints_for_tests(
        registry_hints: crate::registry::service::RegistryHintService,
    ) -> Self {
        Self {
            cache_registry: ProjectCacheRegistry::new(None, false, 512, 30),
            registry_hints,
            registry_executor: crate::registry::executor::RegistryRefreshExecutor::new(
                crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
            ),
        }
    }
}

#[cfg(test)]
impl RegistryHintTestHandle<'_> {
    pub fn write_metadata_for_tests(
        &self,
        package_name: &str,
        latest_version: &str,
        fetched_at: u64,
    ) {
        let _ = self.service.registry_hints.write_metadata_for_tests(
            package_name,
            latest_version,
            fetched_at,
        );
    }
}
```

Add matching `#[cfg(test)]` method to `RegistryHintService`:

```rust
#[cfg(test)]
impl RegistryHintService {
    pub fn write_metadata_for_tests(
        &self,
        package_name: &str,
        latest_version: &str,
        fetched_at: u64,
    ) -> Result<(), String> {
        self.cache.write_metadata(
            package_name,
            RegistryPackageMetadata {
                latest_version: Some(latest_version.to_owned()),
                latest_published_at: None,
                deprecated_versions: Vec::new(),
            },
            fetched_at,
        )
    }
}
```

- [ ] **Step 10: Wire IPC server with bounded progressive refresh**

In `daemon/src/ipc/server.rs`, add an outbound response queue so background tasks can send responses without blocking the request read loop:

```rust
enum ServerOutboundMessage {
    RefreshRegistryHints(RefreshRegistryHintsResponse),
}

async fn send_outbound_message<S>(
    framed: &mut Framed<S, LengthDelimitedCodec>,
    message: ServerOutboundMessage,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match message {
        ServerOutboundMessage::RefreshRegistryHints(response) => framed.send(payload_bytes(&response)?).await?,
    }
    Ok(())
}
```

`message_frame_codec()` returns `tokio_util::codec::LengthDelimitedCodec` (see `daemon/src/ipc/codec.rs`), so this helper's `Framed<S, LengthDelimitedCodec>` type matches the writer built at `server.rs:100`. Import `tokio_util::codec::LengthDelimitedCodec`. In `handle_connection`, initialize the queue before the loop:

```rust
let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerOutboundMessage>();
```

Add an outbound branch to the existing `tokio::select!`:

```rust
let payload = tokio::select! {
    outbound = outbound_rx.recv() => {
        if let Some(message) = outbound {
            send_outbound_message(&mut framed, message).await?;
        }
        continue;
    }
    payload = framed.next() => payload.transpose()?,
    _ = tokio::time::sleep(LIFECYCLE_CHECK_INTERVAL) => {
        if recycle_if_needed(
            &lifecycle,
            service.cache_len(),
            lifecycle_storage_path.as_deref(),
            &prefetcher,
            &service,
        ) {
            return Ok(());
        }
        continue;
    }
};
```

Keep existing foreground analysis response handling unchanged in this task. New long-running registry refresh and workspace report work must use the outbound queue so the read loop keeps accepting normal analysis requests.

Add imports for `RefreshRegistryHintsResponse`, `RegistryHintResult`, and `crate::logging`. Add local helpers in `daemon/src/ipc/server.rs` for the new server-owned protocol checks:

```rust
fn is_supported_protocol_version(version: u32) -> bool {
    // The registry-refresh and workspace-report variants were introduced in
    // protocol v7; older negotiated clients never emit them, so accepting the
    // full supported range here is safe and matches `is_supported_version`.
    (1..=PROTOCOL_VERSION).contains(&version)
}

fn protocol_diagnostics_for_stage(stage: &str, message: &str) -> Vec<ImportDiagnostic> {
    vec![ImportDiagnostic {
        stage: stage.to_owned(),
        message: message.to_owned(),
        details: Vec::new(),
    }]
}

// `server.rs` has no existing wall-clock helper; the registry handler needs one
// to stamp `now_ms` for cache freshness/retry decisions.
fn current_time_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
```

Add `ClientMessage::RefreshRegistryHints` handling with streaming support:

```rust
ClientMessage::RefreshRegistryHints(request) if hello_received => {
    if !is_supported_protocol_version(request.version) {
        let message = format!("unsupported protocol version {}", request.version);
        send_message!(RefreshRegistryHintsResponse {
            version: request.version.min(PROTOCOL_VERSION),
            request_id: request.request_id,
            results: Vec::new(),
            indexes: None,
            error: Some(message.clone()),
            diagnostics: protocol_diagnostics_for_stage("protocol", &message),
        });
        continue;
    }

    let version = request.version;
    let request_id = request.request_id;
    let mode = request.mode;
    let targets = request.targets;
    let now_ms = current_time_millis();
    let (partial_tx, mut partial_rx) = mpsc::unbounded_channel();
    let outbound = outbound_tx.clone();

    for (index, target) in targets.iter().cloned().enumerate() {
        let svc = std::sync::Arc::clone(&service);
        let tx = partial_tx.clone();
        service.spawn_registry_refresh(move || {
            let target_for_error = target.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                svc.refresh_registry_hint_target(target, mode, now_ms)
            }))
            .unwrap_or_else(|_| {
                logging::log_warn(
                    "registry",
                    format!("registry worker panicked for {}", target_for_error.name),
                );
                RegistryHintResult {
                    target: target_for_error,
                    hint: None,
                    error: Some("registry worker panicked".to_owned()),
                }
            });
            let _ = tx.send((index, result));
        });
    }
    drop(partial_tx);

    tokio::spawn(async move {
        let mut ordered_results = vec![None; targets.len()];
        while let Some((index, result)) = partial_rx.recv().await {
            ordered_results[index] = Some(result.clone());
            let _ = outbound.send(ServerOutboundMessage::RefreshRegistryHints(RefreshRegistryHintsResponse {
                version,
                request_id,
                results: vec![result],
                indexes: Some(vec![index]),
                error: None,
                diagnostics: Vec::new(),
            }));
        }

        let results = ordered_results
            .into_iter()
            .zip(targets)
            .map(|(result, target)| result.unwrap_or(RegistryHintResult {
                target,
                hint: None,
                error: Some("registry refresh worker did not return a result".to_owned()),
            }))
            .collect();

        let _ = outbound.send(ServerOutboundMessage::RefreshRegistryHints(RefreshRegistryHintsResponse {
            version,
            request_id,
            results,
            indexes: None,
            error: None,
            diagnostics: Vec::new(),
        }));
    });
    continue;
}
ClientMessage::RefreshRegistryHints(request) => {
    let message = "hello message not received".to_owned();
    send_message!(RefreshRegistryHintsResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        results: Vec::new(),
        indexes: None,
        error: Some(message.clone()),
        diagnostics: protocol_diagnostics_for_stage("protocol", &message),
    });
}
```

This request must be fired separately from `AnalyzePackageJsonRequest`; package.json analysis returns cached hints first, then the extension applies these partial refresh responses as each package completes.

- [ ] **Step 11: Run daemon protocol/service tests**

Run:

```powershell
cargo test -p import-lens-daemon --test ipc_codec --test service --test server --test registry
```

Expected: PASS.

- [ ] **Step 12: Commit registry IPC wiring**

```powershell
git add daemon/src/ipc/protocol.rs daemon/src/ipc/server.rs daemon/src/service.rs daemon/tests/ipc_codec.rs daemon/tests/service.rs daemon/tests/server.rs
git commit -m "feat: serve registry hints from daemon"
```

---

## Task 4: Move TypeScript Package.json Registry Flow To Daemon

**Files:**
- Modify: `extension/src/ipc/protocol.ts`
- Modify: `extension/src/ipc/client.ts`
- Modify: `extension/src/daemon/transport.ts`
- Modify: `extension/src/daemon/manager.ts`
- Modify: `extension/src/daemon/nativeTransport.ts`
- Modify: `extension/src/guidance/packageJsonState.ts`
- Modify: `extension/src/guidance/packageJsonAnalysis.ts`
- Modify: `extension/src/guidance/packageJsonPartial.ts`
- Modify: `extension/src/ui/packageJsonLabels.ts`
- Modify: `extension/src/ui/packageJsonHintVisuals.ts`
- Modify: `extension/src/ui/packageJsonHintSegments.ts`
- Modify: `extension/src/ui/packageJsonTooltip.ts`
- Delete: `extension/src/guidance/registryHints.ts`
- Delete: `extension/test/guidance/registryHints.test.ts`
- Modify: `extension/test/guidance/packageJsonPartial.test.ts`
- Modify: `extension/test/ui/packageJsonLabels.test.ts`
- Modify: `extension/test/ui/packageJsonTooltip.test.ts`
- Modify: `extension/test/ipc/client.test.ts`
- Modify: `extension/test/daemon/transport.test.ts`
- Modify: `package.json`

- [ ] **Step 1: Write failing TypeScript protocol/client tests**

In `extension/test/ipc/client.test.ts`, import `RefreshRegistryHintsRequest` and `RefreshRegistryHintsResponse`, then add a request fixture:

```ts
const registryRefreshRequest = (requestId: number): RefreshRegistryHintsRequest => ({
  type: "refresh_registry_hints",
  version: protocolVersion,
  request_id: requestId,
  targets: [{ name: "react", installedVersion: "18.2.0" }],
  mode: "refresh_stale",
});
```

Add response routing test using the existing socket test helpers:

```ts
test("IpcClient routes registry hint refresh responses by request id", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const final: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 45,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => socket.write(encodeFrame(final)), 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestRefreshRegistryHints(registryRefreshRequest(45));

    assert.equal(response.results[0]?.hint?.latestVersion, "19.0.0");
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});
```

Add partial routing test:

```ts
test("IpcClient delivers registry hint refresh partials before final response", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const partial: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 46,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    indexes: [0],
    error: null,
    diagnostics: [],
  };
  const final: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 46,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(partial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: RefreshRegistryHintsResponse[] = [];
    const response = await client.requestRefreshRegistryHints(
      registryRefreshRequest(46),
      30000,
      (item) => partials.push(item),
    );

    assert.deepEqual(partials[0]?.indexes, [0]);
    assert.equal(response.results[0]?.hint?.latestVersion, "19.0.0");
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});
```

In `extension/test/ui/packageJsonLabels.test.ts`, add:

```ts
test("packageJsonDependencyVersionStatusLabel marks stale cached registry hints", () => {
  const label = packageJsonDependencyVersionStatusLabel({
    name: "react",
    section: "dependencies",
    status: "ready",
    installedVersion: "18.2.0",
    registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
    registryHintRefreshStatus: "stale",
    registryHintRefreshError: "temporary registry failure",
  });

  assert.equal(label, "stale · update 19.0.0");
});
```

In `extension/test/ui/packageJsonTooltip.test.ts`, add:

```ts
test("packageJsonDependencyTooltipMarkdown explains stale cached registry data", () => {
  const markdown = packageJsonDependencyTooltipMarkdown(
    {
      name: "react",
      section: "dependencies",
      status: "ready",
      installedVersion: "18.2.0",
      registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      registryHintRefreshStatus: "stale",
      registryHintRefreshError: "temporary registry failure",
    },
    config({ enableRegistryHints: true }),
    { formatFetchedAt: () => "cached-time" },
  );

  assert.match(markdown, /\$\(warning\) Showing cached registry data/);
  assert.match(markdown, /Refresh error: temporary registry failure/);
});
```

In `extension/test/guidance/packageJsonPartial.test.ts`, add:

```ts
test("mergePackageJsonAnalysisPartial preserves stale registry refresh status", () => {
  const current = [{
    ...stateFor("react", "ready"),
    registryHint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
    registryHintRefreshStatus: "stale",
    registryHintRefreshError: "temporary registry failure",
  }];
  const partial: AnalyzePackageJsonResponse = {
    version: 7,
    request_id: 9,
    sections: [],
    states: [{
      ...stateFor("react", "ready"),
      result: resultFor("react"),
    }],
    error: null,
    diagnostics: [],
  };

  const merged = mergePackageJsonAnalysisPartial(current, partial);

  assert.equal(merged[0]?.registryHintRefreshStatus, "stale");
  assert.equal(merged[0]?.registryHintRefreshError, "temporary registry failure");
});
```

- [ ] **Step 2: Run TypeScript tests and verify failure**

Run:

```powershell
pnpm test:ts
```

Expected: FAIL because protocol types and client method do not exist.

- [ ] **Step 3: Update TypeScript protocol**

In `extension/src/ipc/protocol.ts`, change:

```ts
export const protocolVersion = 7;
```

Add:

```ts
export type RegistryHintMode = "off" | "cached" | "refresh_stale" | "force_refresh";

export interface RegistryHintTarget {
  name: string;
  installedVersion?: string;
}

export interface RegistryHintResult {
  target: RegistryHintTarget;
  hint?: RegistryHint | null;
  error?: string | null;
}

export interface RefreshRegistryHintsRequest {
  type: "refresh_registry_hints";
  version: number;
  request_id: number;
  targets: RegistryHintTarget[];
  mode: "refresh_stale" | "force_refresh";
}

export interface RefreshRegistryHintsResponse {
  version: number;
  request_id: number;
  results: RegistryHintResult[];
  indexes?: number[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

Add optional field to `AnalyzePackageJsonRequest`:

```ts
registry_hint_mode?: RegistryHintMode;
```

Add `RefreshRegistryHintsRequest` to `ClientMessage`.

- [ ] **Step 4: Add IpcClient request method**

In `extension/src/ipc/client.ts`, import `RefreshRegistryHintsRequest` and `RefreshRegistryHintsResponse`.

Add pending state:

```ts
interface PendingRegistryHintRefreshRequest {
  resolve: (response: RefreshRegistryHintsResponse) => void;
  reject: (error: Error) => void;
  onPartial?: (response: RefreshRegistryHintsResponse) => void;
  resetTimeout: () => void;
}
```

Add maps:

```ts
readonly #registryHintRefreshPending = new Map<number, PendingRegistryHintRefreshRequest>();
```

Add request method:

```ts
requestRefreshRegistryHints(
  request: RefreshRegistryHintsRequest,
  timeoutMs = 30000,
  onPartial?: (response: RefreshRegistryHintsResponse) => void,
): Promise<RefreshRegistryHintsResponse> {
  return new Promise((resolve, reject) => {
    let timer: NodeJS.Timeout | undefined;

    const resetTimeout = (): void => {
      if (timer) {
        clearTimeout(timer);
      }
      timer = setTimeout(() => {
        if (this.#registryHintRefreshPending.has(request.request_id)) {
          this.#registryHintRefreshPending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);
    };

    resetTimeout();
    this.#registryHintRefreshPending.set(request.request_id, {
      resolve: (response) => {
        if (timer) {
          clearTimeout(timer);
        }
        resolve(response);
      },
      reject: (error) => {
        if (timer) {
          clearTimeout(timer);
        }
        reject(error);
      },
      onPartial,
      resetTimeout,
    });
    this.send(request);
  });
}
```

Add response routing in `#handleData` before the generic analyze-document branch:

```ts
if (isRefreshRegistryHintsResponse(message)) {
  const pending = this.#registryHintRefreshPending.get(message.request_id);

  if (!pending) {
    continue;
  }

  if (isRegistryHintRefreshPartial(message)) {
    pending.resetTimeout();
    pending.onPartial?.(message);
    continue;
  }

  this.#registryHintRefreshPending.delete(message.request_id);
  pending.resolve(message);
  continue;
}
```

Add close cleanup:

```ts
for (const pending of this.#registryHintRefreshPending.values()) {
  pending.reject(error);
}
this.#registryHintRefreshPending.clear();
```

Add type guards:

```ts
const isRefreshRegistryHintsResponse = (value: unknown): value is RefreshRegistryHintsResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<RefreshRegistryHintsResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.results) &&
    candidate.results.every((result) =>
      !!result &&
      typeof result === "object" &&
      !!(result as { target?: unknown }).target &&
      typeof ((result as { target: { name?: unknown } }).target.name) === "string") &&
    (candidate.indexes === undefined ||
      (Array.isArray(candidate.indexes) && candidate.indexes.every((index) => typeof index === "number"))) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isRegistryHintRefreshPartial = (response: RefreshRegistryHintsResponse): boolean =>
  Array.isArray(response.indexes) && response.indexes.length > 0;

```

Keep `requestRefreshRegistryHints` close to `requestAnalyzePackageJson`; both stream partial responses and reset the timeout on each partial.

- [ ] **Step 5: Add daemon transport forwarding**

Update `AnalysisTransport`, `TransportCoordinator`, `DaemonManager`, and `NativeDaemonTransport` with:

```ts
refreshRegistryHints(
  request: RefreshRegistryHintsRequest,
  onPartial?: (response: RefreshRegistryHintsResponse) => void,
): Promise<RefreshRegistryHintsResponse | null>;
```

In `NativeDaemonTransport`, implement:

```ts
async refreshRegistryHints(
  request: RefreshRegistryHintsRequest,
  onPartial?: (response: RefreshRegistryHintsResponse) => void,
): Promise<RefreshRegistryHintsResponse | null> {
  if (!this.#client || this.#state !== "ready") {
    this.#logger.warn(`Registry hint refresh ${request.request_id} skipped because daemon is ${this.#state}.`);
    return null;
  }

  this.#logger.debug(`Requesting registry hint refresh ${request.request_id} for ${request.targets.length} package(s).`);
  return this.#client.requestRefreshRegistryHints(request, 30000, onPartial);
}
```

In `extension/test/daemon/transport.test.ts`, import `RefreshRegistryHintsRequest`, `RefreshRegistryHintsResponse`, and `protocolVersion`. Update `FakeTransport` and `SlowReadyTransport` to implement `refreshRegistryHints`. Add a coordinator forwarding test:

```ts
test("TransportCoordinator forwards registry refresh partial callbacks", async () => {
  const transport = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([transport]);
  const partials: RefreshRegistryHintsResponse[] = [];

  await coordinator.start("/workspace");
  const response = await coordinator.refreshRegistryHints({
    type: "refresh_registry_hints",
    version: protocolVersion,
    request_id: 88,
    targets: [{ name: "react", installedVersion: "18.2.0" }],
    mode: "refresh_stale",
  }, (partial) => partials.push(partial));

  assert.deepEqual(transport.calls, ["start:/workspace", "registryHints:88"]);
  assert.equal(partials.length, 1);
  assert.equal(response?.results[0]?.hint?.latestVersion, "19.0.0");
});
```

Implement the fake transport method for that test:

```ts
async refreshRegistryHints(
  request: RefreshRegistryHintsRequest,
  onPartial?: (response: RefreshRegistryHintsResponse) => void,
): Promise<RefreshRegistryHintsResponse> {
  this.calls.push(`registryHints:${request.request_id}`);
  const partial: RefreshRegistryHintsResponse = {
    version: request.version,
    request_id: request.request_id,
    indexes: [0],
    results: [{
      target: request.targets[0]!,
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    error: null,
    diagnostics: [],
  };
  onPartial?.(partial);
  return {
    ...partial,
    indexes: undefined,
  };
}
```

- [ ] **Step 6: Replace packageJsonAnalysis registry imports**

Remove from `extension/src/guidance/packageJsonAnalysis.ts`:

```ts
import { fetchRegistryHint, getCachedRegistryHint } from "./registryHints.js";
```

Add these protocol type imports to the existing import from `../ipc/protocol.js`:

```ts
type RefreshRegistryHintsRequest,
type RefreshRegistryHintsResponse,
type RegistryHintTarget,
```

Update initial package.json request:

```ts
include_registry_hints: config.enableRegistryHints,
registry_hint_mode: config.enableRegistryHints ? "cached" : "off",
```

Remove `applyCachedRegistryHints`. The daemon response already owns cached hints.

In `extension/src/guidance/packageJsonState.ts`, extend the local UI state:

```ts
export type RegistryHintRefreshStatus = "fresh" | "stale";

export interface PackageJsonDependencyHintState {
  name: string;
  section: PackageJsonDependencySectionName;
  status: PackageJsonDependencyHintStatus;
  installedVersion?: string;
  result?: ImportResult;
  registryHint?: RegistryHint | null;
  registryHintRefreshStatus?: RegistryHintRefreshStatus;
  registryHintRefreshError?: string | null;
}
```

This state is local to editor rendering. The portable protocol signal remains `RefreshRegistryHintsResponse.results[].error`; when a result contains both `hint` and `error`, editors should show the hint as cached/stale data.

In `extension/src/guidance/packageJsonPartial.ts`, preserve stale refresh state when merging daemon package.json responses:

```ts
import type { RegistryHintRefreshStatus } from "./packageJsonState.js";

type PackageJsonRefreshStateFields = {
  registryHintRefreshStatus?: RegistryHintRefreshStatus;
  registryHintRefreshError?: string | null;
};

type PackageJsonMergeState = PackageJsonDependencyAnalysisItem & PackageJsonRefreshStateFields;

export const mergePackageJsonAnalysisPartial = (
  currentStates: readonly PackageJsonMergeState[],
  partial: AnalyzePackageJsonResponse,
): PackageJsonMergeState[] => {
  if (!partial.indexes) {
    return mergePackageJsonFinalStates(currentStates, partial.states);
  }

  const nextStates = [...currentStates];

  partial.indexes.forEach((stateIndex, partialIndex) => {
    const incoming = partial.states[partialIndex];

    if (!incoming) {
      return;
    }

    const current = nextStates[stateIndex];

    if (current && !isSameDependencyState(current, incoming)) {
      return;
    }

    nextStates[stateIndex] = mergePackageJsonState(current, incoming);
  });

  return nextStates;
};

export const mergePackageJsonFinalStates = (
  currentStates: readonly PackageJsonMergeState[],
  finalStates: readonly PackageJsonDependencyAnalysisItem[],
): PackageJsonMergeState[] =>
  finalStates.map((incoming, index) => mergePackageJsonState(currentStates[index], incoming));

const mergePackageJsonState = (
  current: PackageJsonMergeState | undefined,
  incoming: PackageJsonDependencyAnalysisItem,
): PackageJsonMergeState => {
  if (!current) {
    return incoming;
  }

  const registryHint = newerRegistryHint(current.registryHint, incoming.registryHint);

  return {
    ...incoming,
    registryHint,
    registryHintRefreshStatus: current.registryHintRefreshStatus,
    registryHintRefreshError: current.registryHintRefreshError,
  };
};
```

- [ ] **Step 7: Implement daemon registry refresh queueing in packageJsonAnalysis**

Replace `queueRegistryRefreshes` internals with daemon request targets:

```ts
private queueRegistryRefreshes(
  uri: vscode.Uri,
  states: readonly PackageJsonDependencyAnalysisState[],
  indexes?: readonly number[],
): void {
  if (!getImportLensConfig().enableRegistryHints) {
    return;
  }

  const selectedStates = indexes
    ? indexes.map((index) => states[index]).filter((state): state is PackageJsonDependencyAnalysisState => !!state)
    : states;
  const targets = registryTargetsForStates(selectedStates);

  if (targets.length === 0) {
    return;
  }

  void this.requestRegistryHintRefresh(uri, targets, "refresh_stale");
}
```

Add helpers:

```ts
const registryTargetsForStates = (
  states: readonly PackageJsonDependencyAnalysisState[],
): RegistryHintTarget[] => {
  const seen = new Set<string>();
  const targets: RegistryHintTarget[] = [];

  for (const state of states) {
    const key = `${state.name}\n${state.installedVersion ?? ""}`;

    if (seen.has(key)) {
      continue;
    }

    seen.add(key);
    targets.push({
      name: state.name,
      installedVersion: state.installedVersion,
    });
  }

  return targets;
};

const registryTargetMap = (
  targets: readonly RegistryHintTarget[],
): Map<string, RegistryHintTarget> =>
  new Map(targets.map((target) => [registryTargetKey(target), target]));

const registryTargetKey = (
  target: Pick<RegistryHintTarget, "name" | "installedVersion">,
): string => `${target.name}\n${target.installedVersion ?? ""}`;
```

Add request helper:

```ts
private async requestRegistryHintRefresh(
  uri: vscode.Uri,
  targets: readonly RegistryHintTarget[],
  mode: RefreshRegistryHintsRequest["mode"],
): Promise<void> {
  const pendingTargets = registryTargetMap(targets);
  const markCompleted = (response: RefreshRegistryHintsResponse): void => {
    for (const result of response.results) {
      pendingTargets.delete(registryTargetKey(result.target));
    }
  };

  try {
    const response = await this.#daemon.refreshRegistryHints({
      type: "refresh_registry_hints",
      version: protocolVersion,
      request_id: nextIpcRequestId(),
      targets: [...targets],
      mode,
    }, (partial) => {
      markCompleted(partial);
      this.handleRegistryHintPartial(uri, partial);
    });

    if (!response) {
      this.handleRegistryRefreshRequestFailure(
        uri,
        [...pendingTargets.values()],
        new Error("Daemon unavailable"),
      );
      return;
    }

    markCompleted(response);
    this.handleRegistryHintPartial(uri, response);

    if (response.error && pendingTargets.size > 0) {
      this.handleRegistryRefreshRequestFailure(
        uri,
        [...pendingTargets.values()],
        new Error(response.error),
      );
    }
  } catch (error) {
    this.handleRegistryRefreshRequestFailure(uri, [...pendingTargets.values()], error);
  }
}
```

Add partial merge:

```ts
private handleRegistryHintPartial(
  uri: vscode.Uri,
  response: RefreshRegistryHintsResponse,
): void {
  if (response.error) {
    this.#logger.debug(`Registry hint refresh response failed: ${response.error}`);
  }

  for (const result of response.results) {
    if (result.error) {
      this.#logger.debug(`Registry hint unavailable for ${result.target.name}: ${result.error}`);
    }
    this.updateRegistryHint(
      uri,
      result.target.name,
      result.target.installedVersion,
      result.hint ?? undefined,
      result.error ?? null,
    );
  }
}
```

Add request-level failure handling. This only marks unresolved targets stale; targets completed by earlier partial responses remain fresh.

```ts
private handleRegistryRefreshRequestFailure(
  uri: vscode.Uri,
  targets: readonly RegistryHintTarget[],
  error: unknown,
): void {
  const message = error instanceof Error ? error.message : String(error);
  this.#logger.warn(`Registry hint refresh request failed: ${message}`);

  for (const target of targets) {
    this.updateRegistryHint(
      uri,
      target.name,
      target.installedVersion,
      undefined,
      message,
    );
  }
}
```

Update `updateRegistryHint` to preserve cached data and mark it stale when a live refresh fails:

```ts
private updateRegistryHint(
  uri: vscode.Uri,
  packageName: string,
  installedVersion: string | undefined,
  hint: RegistryHint | null | undefined,
  refreshError: string | null = null,
): void {
  const key = uri.toString();
  const states = this.#states.get(key);

  if (!states) {
    return;
  }

  let changed = false;
  const nextStates = states.map((state) => {
    if (state.name !== packageName || state.installedVersion !== installedVersion) {
      return state;
    }

    const registryHint = newerRegistryHint(state.registryHint, hint);
    const registryHintRefreshStatus = refreshError && registryHint
      ? "stale"
      : registryHint
        ? "fresh"
        : undefined;
    const registryHintRefreshError = refreshError;

    if (
      registryHint === state.registryHint
      && registryHintRefreshStatus === state.registryHintRefreshStatus
      && registryHintRefreshError === state.registryHintRefreshError
    ) {
      return state;
    }

    changed = true;
    return {
      ...state,
      registryHint,
      registryHintRefreshStatus,
      registryHintRefreshError,
    };
  });

  if (changed) {
    this.setStates(uri, nextStates);
  }
}
```

Update `extension/src/ui/packageJsonHintVisuals.ts`:

```ts
export type PackageJsonPrimaryTone = "neutral" | "unavailable";

export type PackageJsonSuffixTone = "latest" | "update" | "install" | "stale";

export const primaryToneThemeColor = (tone: PackageJsonPrimaryTone): string => {
  if (tone === "unavailable") {
    return "list.errorForeground";
  }

  return "descriptionForeground";
};

export const suffixToneThemeColor = (tone: PackageJsonSuffixTone): string => {
  if (tone === "latest") {
    return "gitDecoration.addedResourceForeground";
  }

  if (tone === "stale") {
    return "problemsWarningIcon.foreground";
  }

  return "gitDecoration.modifiedResourceForeground";
};
```

Update `packageJsonDependencyVersionStatusSuffix` in `extension/src/ui/packageJsonLabels.ts`:

```ts
export const packageJsonDependencyVersionStatusSuffix = (
  state: PackageJsonDependencyHintState,
): Pick<PackageJsonHintParts, "suffix" | "suffixTone"> => {
  const { registryHint } = state;

  if (!registryHint?.latestVersion) {
    return state.registryHintRefreshStatus === "stale"
      ? { suffix: "registry stale", suffixTone: "stale" }
      : { suffix: null, suffixTone: null };
  }

  let suffix: string | null = null;
  let suffixTone: PackageJsonSuffixTone | null = null;

  if (state.status === "missing") {
    suffix = `install ${registryHint.latestVersion}`;
    suffixTone = "install";
  } else if (registryHint.isLatest === true) {
    suffix = "latest";
    suffixTone = "latest";
  } else if (registryHint.isLatest === false) {
    suffix = `update ${registryHint.latestVersion}`;
    suffixTone = "update";
  }

  if (state.registryHintRefreshStatus === "stale") {
    return {
      suffix: suffix ? `stale · ${suffix}` : "registry stale",
      suffixTone: "stale",
    };
  }

  return { suffix, suffixTone };
};
```

Update `registryDetailsMarkdown` in `extension/src/ui/packageJsonTooltip.ts`:

```ts
if (state.registryHintRefreshStatus === "stale") {
  details.push("$(warning) Showing cached registry data because the latest refresh failed");
}

if (state.registryHintRefreshError) {
  details.push(`Refresh error: ${state.registryHintRefreshError}`);
}
```

Update `suffixInlineTone` in `extension/src/ui/packageJsonHintSegments.ts`:

```ts
const suffixInlineTone = (tone: PackageJsonSuffixTone): InlineHintSegment["tone"] => {
  if (tone === "latest") {
    return "info";
  }

  if (tone === "stale") {
    return "caution";
  }

  return "action";
};
```

- [ ] **Step 8: Update manual refresh commands**

In `refreshRegistryHintsForUri`, replace `fetchRegistryHint` calls with daemon refresh:

```ts
await this.requestRegistryHintRefresh(
  uri,
  registryTargetsForStates(targets),
  "force_refresh",
);
```

- [ ] **Step 9: Delete old TS registry fetch/cache implementation and p-queue**

Delete:

```powershell
git rm extension/src/guidance/registryHints.ts extension/test/guidance/registryHints.test.ts
```

Modify `package.json` dependencies:

```json
"dependencies": {
  "@msgpack/msgpack": "3.1.3"
}
```

Run:

```powershell
pnpm install --lockfile-only
```

Expected: lockfile removes `p-queue` runtime dependency.

- [ ] **Step 10: Verify old TS registry implementation is gone**

Run:

```powershell
rg -n "fetchRegistryHint|getCachedRegistryHint|registryHints\\.ts|PQueue|p-queue|registry\\.npmjs\\.org|importLens\\.registryHints|Retry-After|retry-after" extension/src extension/test package.json pnpm-lock.yaml
```

Expected: no matches. The remaining TypeScript registry names must be UI/state/protocol names such as `enableRegistryHints`, `registryHint`, `RefreshRegistryHintsRequest`, and `refreshRegistryHints`.

- [ ] **Step 11: Run focused TS tests**

Run:

```powershell
pnpm check
pnpm test:ts
```

Expected: PASS.

- [ ] **Step 12: Commit TS registry migration**

```powershell
git add package.json pnpm-lock.yaml extension/src extension/test
git commit -m "feat: move registry hints to daemon protocol"
```

---

## Task 5: Add Daemon Workspace Report Protocol And Model

**Files:**
- Create: `daemon/src/report/mod.rs`
- Create: `daemon/src/report/executor.rs`
- Create: `daemon/src/report/scanner.rs`
- Create: `daemon/src/report/model.rs`
- Modify: `daemon/src/lib.rs`
- Modify: `daemon/src/ipc/protocol.rs`
- Modify: `daemon/src/ipc/server.rs`
- Modify: `daemon/src/service.rs`
- Test: `daemon/tests/report.rs`
- Test: `daemon/tests/ipc_codec.rs`

- [ ] **Step 1: Add failing report tests**

Create `daemon/tests/report.rs`:

```rust
use import_lens_daemon::{
    ipc::protocol::{PROTOCOL_VERSION, WorkspaceReportBudgets, WorkspaceReportRequest},
    service::ImportLensService,
};
use std::{fs, path::PathBuf};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-report")
}

fn write_report_package(workspace: &std::path::Path) {
    let package_root = workspace.join("node_modules").join("tiny-lib");
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    fs::write(package_root.join("index.js"), "export const value = 1;").expect("entry");
}

#[test]
fn workspace_report_scans_supported_sources_and_skips_node_modules() {
    let workspace = temp_workspace();
    write_report_package(&workspace);
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        workspace.join("src").join("index.ts"),
        "import { value } from 'tiny-lib';\nconsole.log(value);",
    )
    .expect("source file");
    fs::write(
        workspace.join("node_modules").join("ignored.ts"),
        "import { value } from 'tiny-lib';",
    )
    .expect("ignored source");
    let service = ImportLensService::new(None, false);

    let response = service.build_workspace_report(WorkspaceReportRequest {
        message_type: "workspace_report".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 90,
        workspace_root: workspace.to_string_lossy().to_string(),
        budgets: WorkspaceReportBudgets {
            per_import_brotli_bytes: Some(1),
            per_file_brotli_bytes: Some(1),
        },
    });

    fs::remove_dir_all(workspace).expect("workspace cleanup");
    assert_eq!(response.error, None);
    assert_eq!(response.rows.len(), 1, "{response:?}");
    assert_eq!(response.rows[0].package_name, "tiny-lib");
    assert_eq!(response.summary.import_count, 1);
    assert!(response.summary.total_brotli_bytes > 0);
    assert!(response.summary.budget_violation_count > 0);
}
```

- [ ] **Step 2: Run report test and verify failure**

Run:

```powershell
cargo test -p import-lens-daemon --test report
```

Expected: FAIL because report protocol/module does not exist.

- [ ] **Step 3: Add report module exports**

Create `daemon/src/report/mod.rs`:

```rust
pub mod executor;
pub mod model;
pub mod scanner;
```

Create `daemon/src/report/executor.rs`:

```rust
use rayon::{ThreadPool, ThreadPoolBuilder};

const MAX_REPORT_WORKER_THREADS: usize = 4;

pub struct WorkspaceReportExecutor {
    pool: ThreadPool,
}

impl WorkspaceReportExecutor {
    pub fn new() -> Self {
        let pool = ThreadPoolBuilder::new()
            .num_threads(default_report_worker_threads())
            .thread_name(|index| format!("import-lens-report-{index}"))
            .build()
            .expect("workspace report thread pool should build");
        Self { pool }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.pool.spawn(job);
    }

    pub fn install<R: Send>(&self, job: impl FnOnce() -> R + Send) -> R {
        self.pool.install(job)
    }
}

fn default_report_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|count| (count.get() / 2).clamp(1, MAX_REPORT_WORKER_THREADS))
        .unwrap_or(2)
}
```

Modify `daemon/src/lib.rs`:

```rust
pub mod report;
```

- [ ] **Step 4: Add report protocol types**

In `daemon/src/ipc/protocol.rs`, add:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_import_brotli_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_file_brotli_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportRequest {
    #[serde(rename = "type")]
    #[serde(default = "workspace_report_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    #[serde(default)]
    pub budgets: WorkspaceReportBudgets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportRow {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub line: u32,
    pub runtime: String,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub shared_bytes: u64,
    pub confidence: String,
    pub confidence_reasons: String,
    pub top_modules: String,
    pub warning: String,
    pub module_contributions: Vec<ModuleContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportTreemapItem {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub brotli_bytes: u64,
    pub percentage: u64,
    pub confidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateImportGroup {
    pub specifier: String,
    pub count: u64,
    pub total_brotli_bytes: u64,
    pub source_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateModuleGroup {
    pub module_path: String,
    pub basename: String,
    pub count: u64,
    pub total_bytes: u64,
    pub specifiers: Vec<String>,
    pub vendored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportSummary {
    pub import_count: u64,
    pub total_brotli_bytes: u64,
    pub low_confidence_count: u64,
    pub medium_confidence_count: u64,
    pub conservative_count: u64,
    pub budget_violation_count: u64,
    pub duplicate_imports: Vec<DuplicateImportGroup>,
    pub shared_modules: Vec<DuplicateModuleGroup>,
    pub treemap: Vec<WorkspaceReportTreemapItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportResponse {
    pub version: u32,
    pub request_id: u64,
    pub rows: Vec<WorkspaceReportRow>,
    pub summary: WorkspaceReportSummary,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}
```

Add `WorkspaceReport(WorkspaceReportRequest)` to `ClientMessage` and `TypedClientMessage`.

Add:

```rust
fn workspace_report_message_type() -> String {
    "workspace_report".to_owned()
}
```

- [ ] **Step 5: Add scanner**

Create `daemon/src/report/scanner.rs`:

```rust
use std::{
    fs,
    path::{Path, PathBuf},
};

const SUPPORTED_EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "mts", "cts", "svelte", "astro", "vue"];
const SKIPPED_DIRECTORIES: &[&str] = &["node_modules", "dist", "build", "out", "coverage"];

pub fn scan_workspace_sources(workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    scan_directory(workspace_root, &mut files);
    files.sort();
    files
}

fn scan_directory(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }
            scan_directory(&path, files);
            continue;
        }

        if is_supported_source(&path) {
            files.push(path);
        }
    }
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name))
}

fn is_supported_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| SUPPORTED_EXTENSIONS.iter().any(|candidate| extension.eq_ignore_ascii_case(candidate)))
}
```

- [ ] **Step 6: Add report model**

Create `daemon/src/report/model.rs` with daemon equivalents of the current TS report aggregation. Keep UI strings compatible:

```rust
use crate::ipc::protocol::{
    ConfidenceLevel, DetectedImport, DuplicateImportGroup, DuplicateModuleGroup, ImportResult,
    WorkspaceReportBudgets, WorkspaceReportRow, WorkspaceReportSummary, WorkspaceReportTreemapItem,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub struct WorkspaceReportItem {
    pub detected: DetectedImport,
    pub source_file: String,
    pub workspace_root: String,
    pub result: Option<ImportResult>,
    pub warning: Option<String>,
}

pub fn build_report_rows(items: &[WorkspaceReportItem], budgets: &WorkspaceReportBudgets) -> Vec<WorkspaceReportRow> {
    let mut rows = items
        .iter()
        .map(|item| row_for_item(item, budgets))
        .collect::<Vec<_>>();
    rows = apply_file_budget_warnings(rows, budgets);
    rows.sort_by(|left, right| {
        right.brotli_bytes
            .cmp(&left.brotli_bytes)
            .then_with(|| format!("{}:{}:{}", left.source_file, left.line, left.specifier)
                .cmp(&format!("{}:{}:{}", right.source_file, right.line, right.specifier)))
    });
    rows
}

pub fn build_report_summary(rows: &[WorkspaceReportRow]) -> WorkspaceReportSummary {
    let total_brotli_bytes = rows.iter().map(|row| row.brotli_bytes).sum::<u64>();
    WorkspaceReportSummary {
        import_count: rows.len() as u64,
        total_brotli_bytes,
        low_confidence_count: rows.iter().filter(|row| row.confidence == "low").count() as u64,
        medium_confidence_count: rows.iter().filter(|row| row.confidence == "medium").count() as u64,
        conservative_count: rows.iter().filter(|row| row.warning.contains("Conservative estimate")).count() as u64,
        budget_violation_count: rows
            .iter()
            .filter(|row| row.warning.to_ascii_lowercase().contains("budget exceeded"))
            .count() as u64,
        duplicate_imports: build_duplicate_import_groups(rows),
        shared_modules: build_duplicate_module_groups(rows),
        treemap: build_treemap(rows, total_brotli_bytes),
    }
}

fn row_for_item(item: &WorkspaceReportItem, budgets: &WorkspaceReportBudgets) -> WorkspaceReportRow {
    let result = item.result.as_ref();
    WorkspaceReportRow {
        package_name: item.detected.package_name.clone(),
        specifier: item.detected.specifier.clone(),
        source_file: relative_source_file(&item.workspace_root, &item.source_file),
        line: item.detected.line + 1,
        runtime: item.detected.runtime.as_str().to_owned(),
        minified_bytes: result.map(|item| item.minified_bytes).unwrap_or_default(),
        gzip_bytes: result.map(|item| item.gzip_bytes).unwrap_or_default(),
        brotli_bytes: result.map(|item| item.brotli_bytes).unwrap_or_default(),
        zstd_bytes: result.map(|item| item.zstd_bytes).unwrap_or_default(),
        shared_bytes: result.and_then(|item| item.shared_bytes).unwrap_or_default(),
        confidence: confidence_for_result(result),
        confidence_reasons: result.map(|item| item.confidence_reasons.join(" · ")).unwrap_or_default(),
        top_modules: module_breakdown_summary(result),
        warning: warning_for_item(item, budgets),
        module_contributions: result
            .and_then(|item| item.module_breakdown.clone())
            .unwrap_or_default(),
    }
}

fn confidence_for_result(result: Option<&ImportResult>) -> String {
    match result.map(|item| item.confidence) {
        Some(ConfidenceLevel::High) => "high",
        Some(ConfidenceLevel::Medium) => "medium",
        Some(ConfidenceLevel::Low) => "low",
        None => "unknown",
    }
    .to_owned()
}

fn module_breakdown_summary(result: Option<&ImportResult>) -> String {
    result
        .and_then(|item| item.module_breakdown.as_ref())
        .map(|modules| {
            modules
                .iter()
                .take(3)
                .map(|module| format!("{} ({} B)", basename(&module.path), module.bytes))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn warning_for_item(item: &WorkspaceReportItem, budgets: &WorkspaceReportBudgets) -> String {
    let mut warnings = Vec::new();
    if let Some(warning) = item.warning.as_ref() {
        warnings.push(warning.clone());
    }
    if let Some(error) = item.result.as_ref().and_then(|result| result.error.as_ref()) {
        warnings.push(error.clone());
    }
    if item.result.as_ref().and_then(|result| result.shared_bytes).unwrap_or_default() > 0 {
        warnings.push(format!(
            "Shares {} B with other imports in this file",
            item.result.as_ref().and_then(|result| result.shared_bytes).unwrap_or_default()
        ));
    }
    if let (Some(result), Some(limit)) = (item.result.as_ref(), budgets.per_import_brotli_bytes) {
        if result.error.is_none() && result.brotli_bytes > limit {
            warnings.push(format!("Budget exceeded: {} B br > {} B br", result.brotli_bytes, limit));
        }
    }
    if item.result.as_ref().is_some_and(|result| result.is_cjs || result.side_effects || !result.truly_treeshakeable) {
        warnings.push("Conservative estimate".to_owned());
    }
    if let Some(result) = item.result.as_ref() {
        match result.confidence {
            ConfidenceLevel::Low => warnings.push(format!("Low confidence{}", confidence_reason_suffix(result))),
            ConfidenceLevel::Medium => warnings.push(format!("Medium confidence{}", confidence_reason_suffix(result))),
            ConfidenceLevel::High => {}
        }
    }
    warnings.join(" · ")
}

fn apply_file_budget_warnings(
    mut rows: Vec<WorkspaceReportRow>,
    budgets: &WorkspaceReportBudgets,
) -> Vec<WorkspaceReportRow> {
    let Some(limit) = budgets.per_file_brotli_bytes else {
        return rows;
    };
    let mut totals = BTreeMap::<String, u64>::new();
    for row in &rows {
        if row.brotli_bytes > 0 {
            *totals.entry(row.source_file.clone()).or_default() += row.brotli_bytes;
        }
    }
    let mut warned_files = BTreeSet::<String>::new();
    for row in &mut rows {
        let total = totals.get(&row.source_file).copied().unwrap_or_default();
        if total > limit && warned_files.insert(row.source_file.clone()) {
            row.warning = append_warning(
                &row.warning,
                &format!("File budget exceeded: {total} B br > {limit} B br"),
            );
        }
    }
    rows
}

fn append_warning(existing: &str, next: &str) -> String {
    if existing.is_empty() {
        next.to_owned()
    } else {
        format!("{existing} · {next}")
    }
}

fn confidence_reason_suffix(result: &ImportResult) -> String {
    if result.confidence_reasons.is_empty() {
        String::new()
    } else {
        format!(": {}", result.confidence_reasons.join(" · "))
    }
}

fn build_duplicate_import_groups(rows: &[WorkspaceReportRow]) -> Vec<DuplicateImportGroup> {
    let mut groups = BTreeMap::<String, DuplicateImportGroup>::new();
    for row in rows {
        let group = groups.entry(row.specifier.clone()).or_insert_with(|| DuplicateImportGroup {
            specifier: row.specifier.clone(),
            count: 0,
            total_brotli_bytes: 0,
            source_files: Vec::new(),
        });
        group.count += 1;
        group.total_brotli_bytes += row.brotli_bytes;
        group.source_files.push(row.source_file.clone());
    }
    let mut groups = groups
        .into_values()
        .filter(|group| group.count > 1)
        .map(|mut group| {
            group.source_files = group.source_files.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
            group
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| right.count.cmp(&left.count)
        .then_with(|| right.total_brotli_bytes.cmp(&left.total_brotli_bytes))
        .then_with(|| left.specifier.cmp(&right.specifier)));
    groups
}

fn build_duplicate_module_groups(rows: &[WorkspaceReportRow]) -> Vec<DuplicateModuleGroup> {
    let mut groups = BTreeMap::<String, DuplicateModuleGroup>::new();
    for row in rows {
        for module in &row.module_contributions {
            let group = groups.entry(module.path.clone()).or_insert_with(|| DuplicateModuleGroup {
                module_path: module.path.clone(),
                basename: basename(&module.path),
                count: 0,
                total_bytes: 0,
                specifiers: Vec::new(),
                vendored: is_vendored_module_path(&module.path),
            });
            group.count += 1;
            group.total_bytes += module.bytes;
            group.specifiers.push(row.specifier.clone());
        }
    }
    let mut groups = groups
        .into_values()
        .filter(|group| group.count > 1)
        .map(|mut group| {
            group.specifiers = group.specifiers.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
            group
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| right.count.cmp(&left.count)
        .then_with(|| right.total_bytes.cmp(&left.total_bytes))
        .then_with(|| left.module_path.cmp(&right.module_path)));
    groups
}

fn build_treemap(rows: &[WorkspaceReportRow], total_brotli_bytes: u64) -> Vec<WorkspaceReportTreemapItem> {
    rows.iter()
        .filter(|row| row.brotli_bytes > 0)
        .take(10)
        .map(|row| WorkspaceReportTreemapItem {
            package_name: row.package_name.clone(),
            specifier: row.specifier.clone(),
            source_file: row.source_file.clone(),
            brotli_bytes: row.brotli_bytes,
            percentage: if total_brotli_bytes > 0 {
                ((row.brotli_bytes * 100) + (total_brotli_bytes / 2)) / total_brotli_bytes
            } else {
                0
            },
            confidence: row.confidence.clone(),
        })
        .collect()
}

fn relative_source_file(workspace_root: &str, source_file: &str) -> String {
    Path::new(source_file)
        .strip_prefix(workspace_root)
        .ok()
        .and_then(|path| path.to_str())
        .unwrap_or(source_file)
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_owned()
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_owned()
}

fn is_vendored_module_path(module_path: &str) -> bool {
    let normalized = module_path.replace('\\', "/");
    normalized.contains("/vendor/")
        || normalized.contains("/vendors/")
        || normalized.contains("/vendored/")
        || normalized.contains("/node_modules/") && normalized.matches("/node_modules/").count() > 1
}
```

- [ ] **Step 7: Add service workspace report method**

In `daemon/src/service.rs`, extend the `crate::ipc::protocol` imports with `WorkspaceReportRequest`, `WorkspaceReportResponse`, and `WorkspaceReportSummary`. Keep the existing `AnalyzeDocumentRequest`, `PROTOCOL_VERSION`, `fs`, `PathBuf`, and `rayon::prelude::*` imports available for the report builder. Add `report_executor` to `ImportLensService` and initialize it in `new_with_cache_policy`:

```rust
pub struct ImportLensService {
    cache_registry: ProjectCacheRegistry,
    registry_hints: crate::registry::service::RegistryHintService,
    registry_executor: crate::registry::executor::RegistryRefreshExecutor,
    report_executor: crate::report::executor::WorkspaceReportExecutor,
}
```

```rust
let report_executor = crate::report::executor::WorkspaceReportExecutor::new();
Self {
    cache_registry,
    registry_hints,
    registry_executor,
    report_executor,
}
```

Update the `new_with_registry_hints_for_tests` constructor added in Task 3:

```rust
#[cfg(test)]
pub fn new_with_registry_hints_for_tests(
    registry_hints: crate::registry::service::RegistryHintService,
) -> Self {
    Self {
        cache_registry: ProjectCacheRegistry::new(None, false, 512, 30),
        registry_hints,
        registry_executor: crate::registry::executor::RegistryRefreshExecutor::new(
            crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
        ),
        report_executor: crate::report::executor::WorkspaceReportExecutor::new(),
    }
}
```

Add:

```rust
pub fn build_workspace_report(&self, request: WorkspaceReportRequest) -> WorkspaceReportResponse {
    self.report_executor.install(|| self.build_workspace_report_on_worker(request))
}

fn build_workspace_report_on_worker(&self, request: WorkspaceReportRequest) -> WorkspaceReportResponse {
    if !is_supported_protocol_version(request.version) {
        return WorkspaceReportResponse {
            version: request.version.min(PROTOCOL_VERSION),
            request_id: request.request_id,
            rows: Vec::new(),
            summary: empty_workspace_report_summary(),
            error: Some(format!("unsupported protocol version {}", request.version)),
            diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
        };
    }

    self.build_workspace_report_inner(request)
}

fn build_workspace_report_inner(&self, request: WorkspaceReportRequest) -> WorkspaceReportResponse {
    let workspace_root = PathBuf::from(&request.workspace_root);
    let files = crate::report::scanner::scan_workspace_sources(&workspace_root);
    let items = files
        .par_iter()
        .flat_map(|source_path| {
            let source = match fs::read_to_string(source_path) {
                Ok(source) => source,
                Err(_) => return Vec::new(),
            };
            let document_request = AnalyzeDocumentRequest {
                message_type: "analyze_document".to_owned(),
                version: request.version,
                request_id: request.request_id,
                workspace_root: request.workspace_root.clone(),
                active_document_path: source_path.to_string_lossy().to_string(),
                source,
            };
            self.handle_analyze_document(document_request)
                .imports
                .into_iter()
                .map(|item| crate::report::model::WorkspaceReportItem {
                    source_file: source_path.to_string_lossy().to_string(),
                    workspace_root: request.workspace_root.clone(),
                    warning: if item.result.is_some() { None } else { item.message.clone() },
                    detected: item.detected,
                    result: item.result,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let rows = crate::report::model::build_report_rows(&items, &request.budgets);
    let summary = crate::report::model::build_report_summary(&rows);

    WorkspaceReportResponse {
        version: request.version,
        request_id: request.request_id,
        rows,
        summary,
        error: None,
        diagnostics: Vec::new(),
    }
}

pub fn spawn_workspace_report(
    self: &std::sync::Arc<Self>,
    request: WorkspaceReportRequest,
    tx: tokio::sync::oneshot::Sender<WorkspaceReportResponse>,
) {
    let service = std::sync::Arc::clone(self);
    self.report_executor.spawn(move || {
        let _ = tx.send(service.build_workspace_report_on_worker(request));
    });
}
```

Add:

```rust
fn empty_workspace_report_summary() -> WorkspaceReportSummary {
    WorkspaceReportSummary {
        import_count: 0,
        total_brotli_bytes: 0,
        low_confidence_count: 0,
        medium_confidence_count: 0,
        conservative_count: 0,
        budget_violation_count: 0,
        duplicate_imports: Vec::new(),
        shared_modules: Vec::new(),
        treemap: Vec::new(),
    }
}
```

- [ ] **Step 8: Wire IPC server workspace report handler**

In `daemon/src/ipc/server.rs`, extend the `crate::ipc::protocol` imports with `WorkspaceReportRequest`, `WorkspaceReportResponse`, and `WorkspaceReportSummary`. Extend the outbound response queue introduced for background registry refreshes:

```rust
enum ServerOutboundMessage {
    RefreshRegistryHints(RefreshRegistryHintsResponse),
    WorkspaceReport(WorkspaceReportResponse),
}
```

Add the matching arm in `send_outbound_message`:

```rust
ServerOutboundMessage::WorkspaceReport(response) => framed.send(payload_bytes(&response)?).await?,
```

Add:

```rust
ClientMessage::WorkspaceReport(request) if hello_received => {
    prefetcher.cancel();
    lifecycle.record_batch();
    let request_for_error = request.clone();
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    service.spawn_workspace_report(request, response_tx);
    let outbound = outbound_tx.clone();
    tokio::spawn(async move {
        let response = response_rx.await.unwrap_or_else(|_| {
            workspace_report_protocol_error(
                &request_for_error,
                "workspace report worker stopped before sending a response",
            )
        });
        let _ = outbound.send(ServerOutboundMessage::WorkspaceReport(response));
    });
    continue;
}
ClientMessage::WorkspaceReport(request) => {
    send_message!(workspace_report_protocol_error(&request, "hello message not received"));
}
```

Add:

```rust
fn workspace_report_protocol_error(
    request: &WorkspaceReportRequest,
    message: &str,
) -> WorkspaceReportResponse {
    WorkspaceReportResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        rows: Vec::new(),
        summary: WorkspaceReportSummary {
            import_count: 0,
            total_brotli_bytes: 0,
            low_confidence_count: 0,
            medium_confidence_count: 0,
            conservative_count: 0,
            budget_violation_count: 0,
            duplicate_imports: Vec::new(),
            shared_modules: Vec::new(),
            treemap: Vec::new(),
        },
        error: Some(message.to_owned()),
        diagnostics: protocol_diagnostics_for_stage("workspace_report", message),
    }
}
```

This branch must not await the report response in the read loop; the daemon must keep accepting foreground analysis requests while the report worker runs.

- [ ] **Step 9: Run daemon report tests**

Run:

```powershell
cargo test -p import-lens-daemon --test report --test ipc_codec
```

Expected: PASS.

- [ ] **Step 10: Commit report daemon model**

```powershell
git add daemon/src/report daemon/src/lib.rs daemon/src/ipc/protocol.rs daemon/src/ipc/server.rs daemon/src/service.rs daemon/tests/report.rs daemon/tests/ipc_codec.rs
git commit -m "feat: build workspace reports in daemon"
```

---

## Task 6: Replace TypeScript Workspace Report Scanner With Daemon Request

**Files:**
- Modify: `extension/src/ipc/protocol.ts`
- Modify: `extension/src/ipc/client.ts`
- Modify: `extension/src/daemon/transport.ts`
- Modify: `extension/src/daemon/manager.ts`
- Modify: `extension/src/daemon/nativeTransport.ts`
- Modify: `extension/src/ui/report.ts`
- Delete: `extension/src/report/workspaceScanner.ts`
- Delete: `extension/src/report/reportModel.ts`
- Delete: `extension/test/report/workspaceScanner.test.ts`
- Delete: `extension/test/report/reportModel.test.ts`
- Test: `extension/test/ui/report.test.ts`

- [ ] **Step 1: Add failing TS report transport test**

In `extension/test/ipc/client.test.ts`, import `WorkspaceReportRequest` and `WorkspaceReportResponse`, then add:

```ts
const workspaceReportRequest = (requestId: number): WorkspaceReportRequest => ({
  type: "workspace_report",
  version: protocolVersion,
  request_id: requestId,
  workspace_root: "C:/workspace",
  budgets: {
    perImportBrotliBytes: 1,
    perFileBrotliBytes: 1,
  },
});

test("IpcClient routes workspace report responses by request id", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const final: WorkspaceReportResponse = {
    version: protocolVersion,
    request_id: 46,
    rows: [],
    summary: {
      importCount: 0,
      totalBrotliBytes: 0,
      lowConfidenceCount: 0,
      mediumConfidenceCount: 0,
      conservativeCount: 0,
      budgetViolationCount: 0,
      duplicateImports: [],
      sharedModules: [],
      treemap: [],
    },
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => socket.write(encodeFrame(final)), 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestWorkspaceReport(workspaceReportRequest(46));

    assert.equal(response.summary.importCount, 0);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});
```

- [ ] **Step 2: Run TS tests and verify failure**

Run:

```powershell
pnpm test:ts
```

Expected: FAIL because workspace report protocol/client does not exist.

- [ ] **Step 3: Add TypeScript report protocol**

In `extension/src/ipc/protocol.ts`, add interfaces mirroring Rust:

```ts
export interface WorkspaceReportRequest {
  type: "workspace_report";
  version: number;
  request_id: number;
  workspace_root: string;
  budgets?: WorkspaceReportBudgets;
}

export interface WorkspaceReportBudgets {
  perImportBrotliBytes?: number;
  perFileBrotliBytes?: number;
}

export interface WorkspaceReportRow {
  packageName: string;
  specifier: string;
  sourceFile: string;
  line: number;
  runtime: string;
  minifiedBytes: number;
  gzipBytes: number;
  brotliBytes: number;
  zstdBytes: number;
  sharedBytes: number;
  confidence: ConfidenceLevel | "unknown";
  confidenceReasons: string;
  topModules: string;
  warning: string;
  moduleContributions: ModuleContribution[];
}

export interface WorkspaceReportTreemapItem {
  packageName: string;
  specifier: string;
  sourceFile: string;
  brotliBytes: number;
  percentage: number;
  confidence: ConfidenceLevel | "unknown";
}

export interface DuplicateImportGroup {
  specifier: string;
  count: number;
  totalBrotliBytes: number;
  sourceFiles: string[];
}

export interface DuplicateModuleGroup {
  modulePath: string;
  basename: string;
  count: number;
  totalBytes: number;
  specifiers: string[];
  vendored: boolean;
}

export interface WorkspaceReportSummary {
  importCount: number;
  totalBrotliBytes: number;
  lowConfidenceCount: number;
  mediumConfidenceCount: number;
  conservativeCount: number;
  budgetViolationCount: number;
  duplicateImports: DuplicateImportGroup[];
  sharedModules: DuplicateModuleGroup[];
  treemap: WorkspaceReportTreemapItem[];
}

export interface WorkspaceReportResponse {
  version: number;
  request_id: number;
  rows: WorkspaceReportRow[];
  summary: WorkspaceReportSummary;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}
```

Add `WorkspaceReportRequest` to `ClientMessage`.

- [ ] **Step 4: Add client and transport methods**

Add `requestWorkspaceReport` to `IpcClient`, `AnalysisTransport`, `TransportCoordinator`, `DaemonManager`, and `NativeDaemonTransport`.

In `extension/src/ipc/client.ts`, import `WorkspaceReportRequest` and `WorkspaceReportResponse`. Add:

```ts
readonly #workspaceReportPending = new Map<number, PendingRequest<WorkspaceReportResponse>>();

requestWorkspaceReport(
  request: WorkspaceReportRequest,
  timeoutMs = 60000,
): Promise<WorkspaceReportResponse> {
  return this.#requestWithPending(this.#workspaceReportPending, request, timeoutMs);
}
```

Add response routing in `#handleData` before the generic analyze-document branch:

```ts
if (isWorkspaceReportResponse(message)) {
  this.#resolvePending(this.#workspaceReportPending, message);
  continue;
}
```

Add close cleanup:

```ts
for (const pending of this.#workspaceReportPending.values()) {
  pending.reject(error);
}
this.#workspaceReportPending.clear();
```

Add type guard:

```ts
const isWorkspaceReportResponse = (value: unknown): value is WorkspaceReportResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<WorkspaceReportResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.rows) &&
    !!candidate.summary &&
    typeof candidate.summary === "object" &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};
```

In `NativeDaemonTransport`:

```ts
async requestWorkspaceReport(request: WorkspaceReportRequest): Promise<WorkspaceReportResponse | null> {
  if (!this.#client || this.#state !== "ready") {
    this.#logger.warn(`Workspace report ${request.request_id} skipped because daemon is ${this.#state}.`);
    return null;
  }

  this.#logger.debug(`Requesting workspace report ${request.request_id} for ${request.workspace_root}.`);
  return this.#client.requestWorkspaceReport(request, 60000);
}
```

- [ ] **Step 5: Update report UI to use daemon response**

In `extension/src/ui/report.ts`, remove:

```ts
import { buildReportRows, buildReportSummary } from "../report/reportModel.js";
import { buildWorkspaceReportItems, type WorkspaceScannerApi } from "../report/workspaceScanner.js";
```

Add:

```ts
import { protocolVersion, type WorkspaceReportSummary } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
```

Replace report data build with:

```ts
const config = getImportLensConfig();
const response = await vscode.window.withProgress(
  {
    location: vscode.ProgressLocation.Notification,
    title: "ImportLens: Building workspace report",
  },
  () => daemon.requestWorkspaceReport({
    type: "workspace_report",
    version: protocolVersion,
    request_id: nextIpcRequestId(),
    workspace_root: workspaceRoot,
    budgets: config.budgets,
  }),
);

if (!response || response.error) {
  await vscode.window.showWarningMessage(`ImportLens report unavailable${response?.error ? `: ${response.error}` : "."}`);
  return;
}

logger.info(`Workspace report built with ${response.rows.length} import item(s).`);
const reportRows = response.rows;
const summary = response.summary;
```

Update the `svgTreemap` signature because `buildReportSummary` is no longer imported:

```ts
const svgTreemap = (
  items: WorkspaceReportSummary["treemap"],
): string => {
```

Delete `workspaceScannerApi`.

- [ ] **Step 6: Delete old TS scanner/model files and tests**

Run:

```powershell
git rm extension/src/report/workspaceScanner.ts extension/src/report/reportModel.ts extension/src/report/concurrency.ts extension/test/report/workspaceScanner.test.ts extension/test/report/reportModel.test.ts
```

- [ ] **Step 7: Verify old TS report scanner/model implementation is gone**

Run:

```powershell
rg -n "buildWorkspaceReportItems|buildReportRows|buildReportSummary|WorkspaceScannerApi|report/workspaceScanner|report/reportModel|report/concurrency|mapWithConcurrency" extension/src extension/test
```

Expected: no matches. The remaining TypeScript report code must be UI/webview rendering, command wiring, daemon transport forwarding, and protocol types.

- [ ] **Step 8: Run focused TS tests**

Run:

```powershell
pnpm check
pnpm test:ts
```

Expected: PASS.

- [ ] **Step 9: Commit TS report migration**

```powershell
git add extension/src extension/test
git commit -m "feat: request workspace reports from daemon"
```

---

## Task 7: Full Verification, Packaging, And Hash Refresh

**Files:**
- Generated by packaging: `extension/src/daemon/knownHashes.generated.ts`

- [ ] **Step 1: Rust formatting**

Run:

```powershell
cargo fmt --check
```

Expected: PASS.

If it fails, run:

```powershell
cargo fmt
```

Then run `cargo fmt --check` again and expect PASS.

- [ ] **Step 2: TypeScript checks**

Run:

```powershell
pnpm check
```

Expected: PASS.

- [ ] **Step 3: Full test suite**

Run:

```powershell
pnpm test
```

Expected: PASS.

- [ ] **Step 4: Windows package and daemon hash refresh**

Run:

```powershell
pnpm package:win32-x64
```

Expected: PASS. This rebuilds the daemon, copies the Windows binary, refreshes `extension/src/daemon/knownHashes.generated.ts`, builds the extension bundle, and creates the Windows VSIX.

- [ ] **Step 5: Inspect git status**

Run:

```powershell
git status --short
```

Expected: only intentional source, docs, lockfile, generated hash, and packaging artifacts shown. Build artifacts under ignored paths should not be staged.

- [ ] **Step 6: Commit generated hash if changed**

If `extension/src/daemon/knownHashes.generated.ts` changed:

```powershell
git add extension/src/daemon/knownHashes.generated.ts
git commit -m "build: refresh daemon hash after boundary migration"
```

- [ ] **Step 7: Final review diff**

Run:

```powershell
git log --oneline -5
git status --short
```

Expected: recent commits match the tasks above; no unexpected unstaged source changes remain.

- [ ] **Step 8: Final architecture grep**

Run:

```powershell
rg -n "fetchRegistryHint|getCachedRegistryHint|registryHints\\.ts|PQueue|p-queue|registry\\.npmjs\\.org|importLens\\.registryHints|buildWorkspaceReportItems|buildReportRows|buildReportSummary|WorkspaceScannerApi|report/workspaceScanner|report/reportModel|report/concurrency|mapWithConcurrency|tokio::task::spawn_blocking\\(move \\|\\| svc\\.build_workspace_report|tokio::task::spawn_blocking\\(move \\|\\|.*refresh_registry" extension/src extension/test daemon/src package.json pnpm-lock.yaml
```

Expected: no matches. Registry refresh must go through daemon registry service plus `RegistryRefreshExecutor`; workspace report generation must go through daemon report service plus `WorkspaceReportExecutor`; TypeScript must contain only protocol, transport, UI, command, and config code for these features.

---

## Self-Review

- Spec coverage: The plan updates SRS ownership first, moves registry latest/deprecation checks and cache to the daemon, removes TS npm registry fetch code, moves workspace report scan/aggregation to the daemon, adds stale-cache indicators for failed live registry refresh, and keeps editor-only responsibilities in TS.
- Placeholder scan: No task depends on unspecified behavior; code shapes, request fields, and commands are explicit.
- Type consistency: Protocol names match between TS and Rust: `refresh_registry_hints`, `workspace_report`, `RegistryHintMode`, `RegistryHintTarget`, `RefreshRegistryHintsResponse`, and `WorkspaceReportResponse`; UI-only stale status stays in TypeScript state and portable stale fallback is represented as `RegistryHintResult.hint` plus `RegistryHintResult.error`.
- Performance coverage: Live npm refresh uses `RegistryRefreshExecutor`, workspace reports use `WorkspaceReportExecutor`, and both long-running paths respond through the daemon outbound response queue instead of blocking the IPC read loop.
- Verification coverage: Focused tests are added before implementation, then `pnpm check`, `pnpm test`, `cargo fmt --check`, `pnpm package:win32-x64`, and the final architecture grep close the branch.

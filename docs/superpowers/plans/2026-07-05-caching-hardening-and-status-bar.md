# Caching Hardening + Status Bar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the L1 file-size aggregate cache, surface the current file's bundle size live in the status bar, and close the verified caching-system gaps (eviction/orphans, invalidation cost, redundant resolution/prewarm, two small correctness nits).

**Architecture:** Each numbered Part below is one self-contained, independently shippable logical change. Parts are ordered so earlier ones unblock later ones (L1 cache → status bar consumes it). The deep caching-internals parts (5–7) are specified at design + interface + test-intent level; their per-line code is produced during execution after the implementer reads the named file, against the stated interfaces and tests. Parts 1–4 are specified with full code.

**Tech Stack:** Rust daemon (`papaya::HashMap`, `redb`, `rayon`, `rmp_serde`), TypeScript VSCode extension.

## Global Constraints

- **Commit discipline (the important one):** Each **Part** is one logical change and must land as **exactly one clean commit**. While implementing a Part you may make several working commits; when that Part's implementation, review, and fixes are all done, **squash its commits into a single commit** before starting the next Part. Do not interleave Parts in one commit, and do not leave a Part as a string of "wip"/"fix review" commits in history.
  - Mechanically: work each Part on its own, and at the Part gate run `git reset --soft <part-base>` then a single `git commit`, or `git rebase --autosquash`. Verify `git log --oneline` shows one commit per completed Part.
- **Part-boundary gate:** After squashing a Part, run the full relevant test + lint suite (below) and do not start the next Part until it is green.
- **Final review:** After all Parts, dispatch a sub-agent review (superpowers:requesting-code-review) over the whole branch; verify each reported finding against the code before acting (no fake fixes).
- **TDD:** failing test → watch it fail → minimal implementation → watch it pass.
- **Per-Part checks:**
  - Rust (Parts 1, 3, 4, 5, 6, 7): from `daemon/`, `cargo fmt`, `cargo clippy --workspace --all-targets` (repo policy — warnings allowed; do not add `-D warnings`, and do not introduce NEW warnings in changed code), `cargo test -p import-lens-daemon`.
  - TypeScript (Parts 2, 4, 7 UI): from `extension/`, run the project's lint + unit test scripts (`npm run lint`, `npm test` or the repo's configured equivalents).
- **No clippy/lint suppression** to pass — fix the underlying issue.
- Dependency version policy unchanged; no new crates/packages are introduced by this plan.

## Part order & commit map

| Part | Deliverable | Commit type | Files touched (primary) |
|---|---|---|---|
| 1 | L1 file-size aggregate cache | `feat(daemon)` | `pipeline/file_size_cache.rs`, `service.rs` |
| 2 | Status bar: `IL` + live current-file size + states | `feat(extension)` | `ui/statusbar.ts`, `listener.ts` |
| 3 | Prewarm: skip uncacheable `Default` jobs | `perf(daemon)` | `prefetch.rs` |
| 4 | Shutdown flush + bulk-invalidation scoping | `fix(daemon)` | `ipc/server.rs`, `service.rs` |
| 5 | Single-pass package invalidation | `perf(daemon)` | `cache/disk.rs`, `cache/memory.rs`, `cache/key.rs`, `cache/project.rs` |
| 6 | package.json single manifest read | `perf(daemon)` | `service.rs`, `pipeline/resolver.rs` |
| 7 | Eviction caps + orphan-purge command + UI | `feat` | `cache/*.rs`, `registry/cache.rs`, `ipc/protocol.*`, `ui/cacheManager*.ts` |
| — | Deferred (SWR, drop-fingerprint-from-key) | not scheduled | design notes only |

---

# Part 1 — L1 file-size aggregate cache

**Goal:** Cache the aggregate `compute_file_size` result per document (memory-only, one slot per file, keyed on the import-set signature), so a repeat file-size request with an unchanged import set skips the whole bundle→minify→compress.

**Architecture:** New `FileSizeCache` keyed by document `PathBuf`, one slot per file (overwrite-in-place → no orphan growth from edits), LRU-capped like `GRAPH_CACHE`, memory-only. Freshness = hash of the sorted resolved per-import cache keys + `cache_generation()`. Wraps the two `compute_file_size` call sites.

**Interfaces produced (used by Part 2's efficiency note only if folded later):** `FileSizeCache`, `shared_file_size_cache()`, `file_size_signature(context, requests) -> u64`.

### Task 1.1: Make the aggregate result cacheable

- [ ] **Step 1:** In [file_size.rs:22](daemon/src/pipeline/file_size.rs#L22) change `#[derive(Debug, Default)]` to `#[derive(Debug, Default, Clone)]` on `FileSizeComputation`.
- [ ] **Step 2:** In [memory.rs](daemon/src/cache/memory.rs) below `current_cache_generation` add:

```rust
/// Public reader for the global cache generation. The file-size L1 cache folds
/// this into its freshness signature so a node_modules invalidation forces every
/// file entry to recompute.
pub fn cache_generation() -> u64 {
    current_cache_generation()
}
```

- [ ] **Step 3:** `cargo build` — expect clean.
- [ ] **Step 4:** Commit (working commit; squashed at Part gate).

### Task 1.2: `FileSizeCache` struct

- [ ] **Step 1:** Add `pub mod file_size_cache;` after line 7 of [pipeline/mod.rs](daemon/src/pipeline/mod.rs).
- [ ] **Step 2:** Create `daemon/src/pipeline/file_size_cache.rs` with the struct + failing tests (bodies `todo!()`):

```rust
use crate::pipeline::file_size::FileSizeComputation;
use papaya::HashMap;
use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

// One aggregate-size entry per document path. Editing a file overwrites its
// single slot in place, so repeated edits never accumulate orphaned entries.
// Distinct files are bounded by LRU eviction, mirroring GRAPH_CACHE.
const MAX_CACHED_FILE_SIZES: usize = 64;

#[derive(Debug)]
struct CachedFileSize {
    signature: u64,
    computation: FileSizeComputation,
    last_used_millis: AtomicU64,
}

#[derive(Debug, Default)]
pub struct FileSizeCache {
    entries: HashMap<PathBuf, CachedFileSize>,
}

impl FileSizeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path, signature: u64) -> Option<FileSizeComputation> {
        let pinned = self.entries.pin();
        let entry = pinned.get(path)?;
        if entry.signature != signature {
            return None;
        }
        entry
            .last_used_millis
            .store(crate::time::unix_millis_now(), Ordering::Relaxed);
        Some(entry.computation.clone())
    }

    pub fn insert(&self, path: PathBuf, signature: u64, computation: FileSizeComputation) {
        let pinned = self.entries.pin();
        pinned.insert(
            path,
            CachedFileSize {
                signature,
                computation,
                last_used_millis: AtomicU64::new(crate::time::unix_millis_now()),
            },
        );
        if pinned.len() > MAX_CACHED_FILE_SIZES {
            if let Some(oldest) = pinned
                .iter()
                .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
            {
                pinned.remove(&oldest);
            }
        }
    }

    /// Signature-independent presence check (integration tests use it so a
    /// concurrent generation bump cannot make the assertion flaky).
    pub fn contains_path(&self, path: &Path) -> bool {
        self.entries.pin().get(path).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.pin().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static SHARED_FILE_SIZE_CACHE: OnceLock<FileSizeCache> = OnceLock::new();

pub fn shared_file_size_cache() -> &'static FileSizeCache {
    SHARED_FILE_SIZE_CACHE.get_or_init(FileSizeCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computation(minified: u64) -> FileSizeComputation {
        FileSizeComputation { minified_bytes: minified, ..FileSizeComputation::default() }
    }

    #[test]
    fn get_on_empty_is_none() {
        assert!(FileSizeCache::new().get(Path::new("/a/index.ts"), 1).is_none());
    }

    #[test]
    fn insert_then_get_with_matching_signature_returns_value() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 42, computation(1234));
        assert_eq!(cache.get(Path::new("/a/index.ts"), 42).expect("hit").minified_bytes, 1234);
        assert!(cache.contains_path(Path::new("/a/index.ts")));
        assert!(!cache.contains_path(Path::new("/a/other.ts")));
    }

    #[test]
    fn get_with_stale_signature_is_none() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 42, computation(1234));
        assert!(cache.get(Path::new("/a/index.ts"), 99).is_none());
    }

    #[test]
    fn reinserting_same_path_overwrites_in_place_without_orphans() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 1, computation(10));
        cache.insert(PathBuf::from("/a/index.ts"), 2, computation(20));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(Path::new("/a/index.ts"), 1).is_none());
        assert_eq!(cache.get(Path::new("/a/index.ts"), 2).expect("hit").minified_bytes, 20);
    }

    #[test]
    fn eviction_bounds_distinct_files() {
        let cache = FileSizeCache::new();
        for index in 0..(MAX_CACHED_FILE_SIZES + 10) {
            cache.insert(PathBuf::from(format!("/a/file{index}.ts")), 1, computation(index as u64));
        }
        assert!(cache.len() <= MAX_CACHED_FILE_SIZES);
    }
}
```

Note: `get`/`insert`/`contains_path` are shown implemented above; if you prefer strict red-green, stub them `todo!()` first, run to see the failures, then paste the bodies. Either way end with all five tests green.

- [ ] **Step 3:** `cargo test -p import-lens-daemon file_size_cache` → all pass.
- [ ] **Step 4:** Commit.

### Task 1.3: Import-set signature

- [ ] **Step 1:** Add these tests to the `tests` module (unresolvable context → all requests hit the fallback token, no fixture needed):

```rust
    use crate::cache::memory::bump_cache_generation;
    use crate::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
    use crate::pipeline::analyze::AnalysisContext;

    fn unresolvable_context() -> AnalysisContext {
        AnalysisContext {
            workspace_root: PathBuf::from("/does/not/exist"),
            active_document_path: PathBuf::from("/does/not/exist/src/index.ts"),
        }
    }
    fn named_request(package: &str, named: &[&str]) -> ImportRequest {
        ImportRequest {
            specifier: package.to_owned(),
            package_name: package.to_owned(),
            version: "1.0.0".to_owned(),
            named: named.iter().map(|n| (*n).to_owned()).collect(),
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }
    }

    #[test]
    fn signature_is_order_independent() {
        let ctx = unresolvable_context();
        let a = named_request("alpha", &["x"]);
        let b = named_request("beta", &["y"]);
        assert_eq!(
            file_size_signature(&ctx, &[a.clone(), b.clone()]),
            file_size_signature(&ctx, &[b, a])
        );
    }
    #[test]
    fn signature_changes_when_named_exports_change() {
        let ctx = unresolvable_context();
        assert_ne!(
            file_size_signature(&ctx, &[named_request("alpha", &["x"])]),
            file_size_signature(&ctx, &[named_request("alpha", &["x", "y"])])
        );
    }
    #[test]
    fn signature_changes_when_generation_bumps() {
        let ctx = unresolvable_context();
        let reqs = [named_request("alpha", &["x"])];
        let before = file_size_signature(&ctx, &reqs);
        bump_cache_generation();
        assert_ne!(before, file_size_signature(&ctx, &reqs));
    }
```

- [ ] **Step 2:** Run → fail to compile (`file_size_signature` missing).
- [ ] **Step 3:** Extend the top-of-file `use` block to add `crate::cache::{key::cache_key_for_resolved_import, memory::cache_generation}`, `crate::ipc::protocol::ImportRequest`, `crate::pipeline::{analyze::AnalysisContext, resolver::resolve_package_entry}`, and `std::{collections::hash_map::DefaultHasher, hash::{Hash, Hasher}}`. Add above the tests:

```rust
/// Freshness key for an L1 file-size entry: sorted resolved per-import cache keys
/// (which fold in each package's manifest + entry fingerprint) plus the cache
/// generation. Unresolvable requests contribute a stable request-shape token.
pub fn file_size_signature(context: &AnalysisContext, requests: &[ImportRequest]) -> u64 {
    let mut tokens = requests
        .iter()
        .map(|request| match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => cache_key_for_resolved_import(request, &resolved),
            Err(_) => format!(
                "unresolved:{}:{:?}:{}",
                request.specifier, request.import_kind, request.named.join(",")
            ),
        })
        .collect::<Vec<_>>();
    tokens.sort();

    let mut hasher = DefaultHasher::new();
    cache_generation().hash(&mut hasher);
    for token in &tokens {
        token.hash(&mut hasher);
    }
    hasher.finish()
}
```

- [ ] **Step 4:** `cargo test -p import-lens-daemon file_size_cache` → all eight pass.
- [ ] **Step 5:** Commit.

### Task 1.4: Wire both handlers

- [ ] **Step 1:** Add the integration test to [daemon/tests/service.rs](daemon/tests/service.rs) (add `use daemon::pipeline::file_size_cache::shared_file_size_cache;` at top):

```rust
fn file_size_request(workspace: &Path, request_id: u64) -> FileSizeRequest {
    FileSizeRequest {
        message_type: "file_size".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("src").join("index.ts").to_string_lossy().to_string(),
        imports: vec![ImportRequest {
            specifier: "tiny-lib".to_owned(),
            package_name: "tiny-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
    }
}

#[test]
fn handle_file_size_populates_and_reuses_aggregate_cache() {
    let workspace = temp_workspace("file-size-cache");
    let service = ImportLensService::new(None, false);
    let first = service.handle_file_size(file_size_request(&workspace, 1));
    let second = service.handle_file_size(file_size_request(&workspace, 2));
    assert_eq!(first.minified_bytes, second.minified_bytes);
    assert_eq!(first.gzip_bytes, second.gzip_bytes);
    assert!(shared_file_size_cache().contains_path(&workspace.join("src").join("index.ts")));
}
```

- [ ] **Step 2:** Run → fail on the `contains_path` assertion.
- [ ] **Step 3:** Add the helper inside `impl ImportLensService` (near `analyze_with_cache`, ~[service.rs:1232](daemon/src/service.rs#L1232)):

```rust
fn file_size_with_cache(
    &self,
    context: &AnalysisContext,
    active_document_path: &str,
    requests: &[ImportRequest],
) -> crate::pipeline::file_size::FileSizeComputation {
    let cache = crate::pipeline::file_size_cache::shared_file_size_cache();
    let path = PathBuf::from(active_document_path);
    let signature = crate::pipeline::file_size_cache::file_size_signature(context, requests);
    if let Some(hit) = cache.get(&path, signature) {
        crate::logging::log_debug("file_size_cache", format!("hit: {}", path.display()));
        return hit;
    }
    crate::logging::log_debug("file_size_cache", format!("miss: {}", path.display()));
    let computed = compute_file_size(context, requests);
    cache.insert(path, signature, computed.clone());
    computed
}
```

- [ ] **Step 4:** Replace [service.rs:435](daemon/src/service.rs#L435) `let file_size = compute_file_size(&context, &request.imports);` with `let file_size = self.file_size_with_cache(&context, &request.active_document_path, &request.imports);`
- [ ] **Step 5:** Replace [service.rs:599](daemon/src/service.rs#L599) `let file_size = compute_file_size(&context, &requests);` with `let file_size = self.file_size_with_cache(&context, &request.active_document_path, &requests);`
- [ ] **Step 6:** `cargo test -p import-lens-daemon` → all green.
- [ ] **Step 7:** Commit.

### Part 1 gate

- [ ] `cargo fmt` · `cargo clippy --workspace --all-targets` (repo policy — warnings allowed; do not add `-D warnings`, and do not introduce NEW warnings in changed code) · `cargo test -p import-lens-daemon` all green.
- [ ] **Squash Part 1's commits into one:** `feat(daemon): add memory-only L1 file-size aggregate cache`.

---

# Part 2 — Status bar: `IL` + live current-file size + states

**Goal:** The status bar shows `IL` (not `ImportLens`), and by default displays the **current file's bundle size**; when no supported file is open it shows a `Ready`/idle state; when the daemon is unavailable or a request errors it shows that instead.

**Architecture:** Replace the fixed `setStatus(status)` label map with a small state model rendered by a **pure** `statusBarText(state)` function (unit-testable without the `vscode` API). The `DocumentAnalysisController` already runs on open/change/active-editor-change; after a successful document analysis it issues one `file_size_document` request (caches are warm from Part 1 + L2, so this is cheap) and renders the size; it also handles the no-active-editor case.

**State precedence:** `unavailable` (daemon/error) > `computing` (in flight) > `size` (file has runtime imports) > `ready` (idle: no file, or file with no runtime imports).

> Efficiency note (not implemented here, deliberate): the status size uses a second `file_size_document` round-trip rather than folding the aggregate into `analyze_document`. Folding it into `AnalyzeDocumentResponse` would save one round-trip but needs a protocol version bump; deferred to keep this Part's blast radius to the extension. L1 + L2 make the second request inexpensive.

### Task 2.1: Pure status-bar text model

**Files:** `extension/src/ui/statusbar.ts`; Test: the repo's TS test dir mirroring `extension/test/ui/` (create `extension/test/ui/statusbar.test.ts`).

- [ ] **Step 1:** Write `extension/test/ui/statusbar.test.ts` (match the repo's test framework — the existing `extension/test/ui/*.test.ts` files show the import + assertion style; mirror them):

```ts
import { statusBarText } from "../../src/ui/statusbar.js";

describe("statusBarText", () => {
  it("prefixes with IL and shows the size for a sized state", () => {
    expect(statusBarText({ kind: "size", label: "12.3 kB gzip" })).toBe("IL: 12.3 kB gzip");
  });
  it("shows Ready when idle", () => {
    expect(statusBarText({ kind: "ready" })).toBe("IL: Ready");
  });
  it("shows Computing while in flight", () => {
    expect(statusBarText({ kind: "computing" })).toBe("IL: Computing…");
  });
  it("shows Unavailable on daemon/error", () => {
    expect(statusBarText({ kind: "unavailable" })).toBe("IL: Unavailable");
  });
});
```

(If the repo uses `node:test`+`assert` rather than Jest-style `describe/it`, adapt to that — check a neighboring `extension/test/ui/*.test.ts` and copy its harness exactly.)

- [ ] **Step 2:** Run the test → fails (`statusBarText` missing).
- [ ] **Step 3:** Rewrite `extension/src/ui/statusbar.ts`:

```ts
import * as vscode from "vscode";

export type StatusBarState =
  | { kind: "ready" }
  | { kind: "computing" }
  | { kind: "unavailable" }
  | { kind: "size"; label: string };

export const statusBarText = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size":
      return `IL: ${state.label}`;
    case "computing":
      return "IL: Computing…";
    case "unavailable":
      return "IL: Unavailable";
    case "ready":
      return "IL: Ready";
  }
};

const tooltipFor = (state: StatusBarState): string => {
  switch (state.kind) {
    case "size":
      return `ImportLens — current file bundle size (${state.label})`;
    case "computing":
      return "ImportLens — computing current file size";
    case "unavailable":
      return "ImportLens — daemon unavailable";
    case "ready":
      return "ImportLens — ready";
  }
};

export class StatusBarController implements vscode.Disposable {
  readonly #item: vscode.StatusBarItem;

  constructor() {
    this.#item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.#item.name = "ImportLens";
    this.#item.command = "importLens.showLogs";
    this.setState({ kind: "unavailable" });
    this.#item.show();
  }

  setState(state: StatusBarState): void {
    this.#item.text = statusBarText(state);
    this.#item.tooltip = tooltipFor(state);
  }

  dispose(): void {
    this.#item.dispose();
  }
}
```

- [ ] **Step 4:** Run the test → passes.
- [ ] **Step 5:** Commit.

### Task 2.2: Migrate existing callers to `setState`

**Files:** `extension/src/listener.ts`, `extension/src/extension.ts`. **NOT** `configRefresh.ts` — it only declares/forwards the `DaemonStateTransitionActions.setStatus(state: DaemonState)` interface ([configRefresh.ts:56](extension/src/configRefresh.ts#L56), [:67](extension/src/configRefresh.ts#L67)) and calls no `StatusBarController` method, so it needs no change.

Complete, grep-verified list of concrete `StatusBarController.setStatus` call sites (all must be migrated before Step 3 deletes the old method, or the build breaks):
- `listener.ts` ×7 — lines 88, 92, 111, 122, 128, 162, 169.
- [extension.ts:215](extension/src/extension.ts#L215) — `statusBar.setStatus(state === "ready" ? "ready" : "unavailable")` in `restartDaemonAndRefresh`.
- [extension.ts:358](extension/src/extension.ts#L358) — `setStatus: (state) => statusBar.setStatus(state)` (the `DaemonStateTransitionActions` wiring; `state` is a `DaemonState`).
- [extension.ts:376](extension/src/extension.ts#L376) — `statusBar.setStatus(state === "ready" ? "ready" : "unavailable")` at startup.

- [ ] **Step 1:** In [listener.ts](extension/src/listener.ts), replace `setStatus("unavailable")` → `setState({ kind: "unavailable" })`, `setStatus("computing")` → `setState({ kind: "computing" })`, `setStatus("ready")` → `setState({ kind: "ready" })` at lines 88, 92, 111, 122, 128, 162, 169. (Line 162's `ready` is replaced entirely in Task 2.3; leaving it as `setState({ kind: "ready" })` here as an interim is fine.)
- [ ] **Step 2:** In [extension.ts](extension/src/extension.ts):
  - Line 215 → `statusBar.setState({ kind: state === "ready" ? "ready" : "unavailable" });`
  - Line 376 → `statusBar.setState({ kind: state === "ready" ? "ready" : "unavailable" });`
  - Line 358 → `setStatus: (state) => statusBar.setState({ kind: state === "ready" ? "ready" : "unavailable" }),` — map the `DaemonState` to a `StatusBarState`. Do **not** rename the `DaemonStateTransitionActions.setStatus` interface method in `configRefresh.ts`; only this implementation body changes.
- [ ] **Step 3:** Remove the now-unused `ImportLensStatus` type, `labels` map, and old `setStatus` method from `statusbar.ts`. First `grep` the whole `extension/` for `.setStatus(` and confirm zero remaining `StatusBarController.setStatus` references before deleting.
- [ ] **Step 4:** Typecheck/build the extension → clean (this catches any missed site).
- [ ] **Step 5:** Commit.

### Task 2.3: Render the current file's size

**Files:** `extension/src/listener.ts`.

- [ ] **Step 1:** Add imports at the top of `listener.ts`:

```ts
import { bytesForCompression, formatBytes, labelForCompression } from "./ui/format.js";
```

- [ ] **Step 2:** Add a private method to `DocumentAnalysisController` that fetches and renders the size, guarded by freshness against the originating analysis request id:

```ts
private async updateFileSize(
  document: vscode.TextDocument,
  workspaceRoot: string,
  analysisRequestId: number,
): Promise<void> {
  const documentKey = document.uri.toString();
  const config = getImportLensConfig();
  let response: import("./ipc/protocol.js").FileSizeDocumentResponse | null = null;
  try {
    response = await this.#daemon.requestFileSizeDocument({
      type: "file_size_document",
      version: protocolVersion,
      request_id: nextIpcRequestId(),
      workspace_root: workspaceRoot,
      active_document_path: document.fileName,
      source: document.getText(),
    });
  } catch (error) {
    this.#logger.warn(
      `File-size status request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  // A newer analysis for this document supersedes this size result.
  if (!this.#freshness.isCurrent(documentKey, analysisRequestId)) {
    return;
  }
  if (!response || response.error) {
    this.#statusBar.setState({ kind: "unavailable" });
    return;
  }
  if (response.imports.length === 0) {
    this.#statusBar.setState({ kind: "ready" });
    return;
  }
  const label = `${formatBytes(bytesForCompression(response, config.compression))} ${labelForCompression(config.compression)}`;
  this.#statusBar.setState({ kind: "size", label });
}
```

- [ ] **Step 3:** In `analyze()`, replace the success `setStatus("ready")` at [listener.ts:162](extension/src/listener.ts#L162) with a call that renders size instead. Change the block after `recordImportCostHistory` from `this.#statusBar.setState({ kind: "ready" });` to:

```ts
      await this.updateFileSize(document, workspaceRoot, requestId);
      this.#logger.debug(`Completed document analysis request ${requestId}.`);
```

Leave the no-imports branch at line 126-130 as `setState({ kind: "ready" })` (idle when a file has no runtime imports). Leave computing/unavailable branches as migrated in Task 2.2.

- [ ] **Step 4:** Handle the no-active-editor case. In the constructor's `onDidChangeActiveTextEditor` handler ([listener.ts:52-56](extension/src/listener.ts#L52-L56)) add an else branch:

```ts
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor && supportedLanguageIds.has(editor.document.languageId) && editor.document.uri.scheme === "file") {
          this.schedule(editor.document);
        } else {
          this.#statusBar.setState({ kind: "ready" });
        }
      }),
```

- [ ] **Step 5:** Build the extension + run lint/tests → clean.
- [ ] **Step 6:** Manual verification (per superpowers:verify, if driving the extension is feasible): open a TS file with imports → status shows `IL: <size>`; switch to a non-code tab → `IL: Ready`; stop the daemon → `IL: Unavailable`.
- [ ] **Step 7:** Commit.

### Part 2 gate

- [ ] Extension lint + unit tests green; `statusBarText` tests pass; no stray references to the removed `setStatus`/`ImportLensStatus`.
- [ ] **Squash Part 2's commits into one:** `feat(extension): show current file size in status bar as IL`.

---

# Part 3 — Prewarm: skip uncacheable `Default` jobs

**Goal:** Stop prewarm from recomputing (bundle→minify→compress) on every trigger for packages with no `default` export, whose `Default` result is never cacheable (`should_cache_result` is false when a request-specific "exports" diagnostic is present).

**Root cause (verified):** [prefetch.rs:180-187](daemon/src/prefetch.rs#L180-L187) enqueues both a `Default` and a `Namespace` prewarm job for every dependency. Default-less packages emit an exports-stage diagnostic → not cached → the `Default` job re-runs each prewarm.

**Approach:** In `package_json_prewarm_jobs`, only enqueue the `Default` job when the resolved package actually exposes a `default` export; always enqueue `Namespace` (it is cacheable and is what the package.json view uses). Determining "has default export" cheaply: the resolver/graph already knows exports; reuse the existing export-enumeration or `module_provides_export(graph, entry_id, "default", …)` path rather than adding a new probe. If that requires a graph build the prewarm would do anyway, prefer gating on it; if it is too heavy for prewarm, instead gate by "package has no `default` in its exports map / is ESM-only" using the manifest already read.

### Task 3.1: Gate the Default prewarm job

**Files:** `daemon/src/prefetch.rs`; Test: `daemon/tests/prefetch.rs` (exists).

- [ ] **Step 1:** Read [prefetch.rs](daemon/src/prefetch.rs) fully around `package_json_prewarm_jobs` and how `resolved` (`installed_package`) exposes package info; and check `daemon/tests/prefetch.rs` for the existing fixture/harness (a package known to lack a default export).
- [ ] **Step 2:** Write a failing test in `daemon/tests/prefetch.rs`: given a fixture package that has named exports but **no** default export, the prewarm job list contains a `Namespace` job for it and **no** `Default` job; given a package with a default export, both are present. Assert on the produced `PrewarmJob.request.import_kind` set per package.
- [ ] **Step 3:** Run → fails (both jobs currently enqueued unconditionally).
- [ ] **Step 4:** Implement the gate in `package_json_prewarm_jobs`: compute `has_default` for the resolved package and push the `Default` job only when true; always push `Namespace`. Use the cheapest correct signal available in that module (confirm which of: resolver exports, a graph `module_provides_export(entry, "default")`, or manifest shape — pick the one that does not add a redundant full analysis).
- [ ] **Step 5:** Run the new test + `cargo test -p import-lens-daemon prefetch` → green.
- [ ] **Step 6:** Commit.

### Part 3 gate

- [ ] `cargo fmt` · `clippy` · `cargo test -p import-lens-daemon` green.
- [ ] **Squash into one:** `perf(daemon): skip uncacheable default prewarm jobs`.

---

# Part 4 — Shutdown flush + bulk-invalidation scoping

Two small, independent correctness improvements verified during review. Grouped because both are tiny and touch adjacent lifecycle/invalidation code; still **one squashed commit**.

**4A — Flush pending disk inserts on shutdown.** The `Shutdown` handler flushes recency touches but not pending inserts; the common path relies on `DiskCache::Drop` running on clean exit (which it does within the extension's 5s grace), but a background worker holding a cache `Arc` past the grace window can skip it. A one-line explicit flush removes the ambiguity (matches the recycle path).

**4B — Scope bulk invalidation to the changed sub-project.** A >20-package change currently calls `invalidate_all`, which clears **every shard in the current workspace** — over-reaching in monorepos/multi-root windows. Prefer the already-batched `invalidate_packages` path (handles arbitrary counts in one disk pass) or scope to the affected shard.

### Task 4A.1: Explicit flush on Shutdown

**Files:** `daemon/src/ipc/server.rs`.

- [ ] **Step 1:** In the `ClientMessage::Shutdown` arm at [server.rs:731-734](daemon/src/ipc/server.rs#L731-L734), add `service.flush_cache()` before `service.flush_cache_recency_touches()` (log a warn on `Err`, mirroring the recycle path at [server.rs:937-941](daemon/src/ipc/server.rs#L937-L941)):

```rust
            ClientMessage::Shutdown(_) => {
                prefetcher.cancel();
                if let Err(error) = service.flush_cache() {
                    logging::log_warn("lifecycle", format!("failed to flush cache on shutdown: {error}"));
                }
                service.flush_cache_recency_touches();
                return Ok(());
            }
```

- [ ] **Step 2:** If a daemon test exercises shutdown flushing (check `daemon/tests/server.rs`/`lifecycle.rs`), extend it to assert pending inserts are persisted after a shutdown; otherwise add a focused test that inserts <64 entries, sends `Shutdown`, reopens the disk cache at the same path, and asserts the entry is present. If a headless shutdown test is impractical, document that the behavior mirrors the covered recycle path.
- [ ] **Step 3:** `cargo test -p import-lens-daemon` → green.
- [ ] **Step 4:** Commit (working commit).

### Task 4B.1: Narrow bulk invalidation

**Files:** `daemon/src/service.rs`.

- [ ] **Step 1:** Read `invalidate_package_json_paths` ([service.rs:1159-1188](daemon/src/service.rs#L1159-L1188)) and `invalidate_packages` fully. Confirm `invalidate_packages` already runs graph/resolver/generation invalidation once for the batch (it does not today — those live in `invalidate_package_json_paths` after the loop; verify) so scoping does not drop a needed global step.
- [ ] **Step 2:** Write a test in `daemon/tests/` (or extend an invalidation test): a >20-path bulk change that maps to known package names invalidates those packages' entries but does **not** remove an unrelated project shard's entries under the same workspace `base_path`. (Seed two shards, invalidate a bulk set belonging to shard A, assert shard B's entry survives.)
- [ ] **Step 3:** Run → fails (current code calls `invalidate_all`, wiping both).
- [ ] **Step 4:** Change the overflow branch: when all `package_json_paths` resolve to package names, route through `invalidate_packages(&package_names)` + the existing per-batch graph/resolver/generation invalidation regardless of count, instead of `invalidate_all()`. Keep the `invalidate_all()` fallback only for the genuinely-unresolvable case (a path whose package name can't be derived), where a full clear is the safe choice. Preserve current behavior for that fallback.
- [ ] **Step 5:** Run the new test + full suite → green.
- [ ] **Step 6:** Commit (working commit).

### Part 4 gate

- [ ] `cargo fmt` · `clippy` · `cargo test -p import-lens-daemon` green.
- [ ] **Squash 4A + 4B into one:** `fix(daemon): flush inserts on shutdown and scope bulk invalidation`.

---

# Part 5 — Single-pass package invalidation

**Goal:** Make `invalidate_package`/`invalidate_packages` cheap: decode each cache key once and match against the whole package set in a single scan, and hold the write transaction only for removals (not for the full decode scan).

**Verified problem:** [disk.rs:320-337](daemon/src/cache/disk.rs#L320-L337) iterates the entire `CACHE_TABLE` under a `begin_write` txn, calling `cache_key_matches_package` (which hex-decodes + `rmp_serde`-deserializes every key at [key.rs:79-87](daemon/src/cache/key.rs#L79-L87)); [memory.rs:168-181](daemon/src/cache/memory.rs#L168-L181) repeats the decode per package; [project.rs:250-282](daemon/src/cache/project.rs#L250-L282) loops shards × packages. Net cost = shards × packages × keys, each a full decode, disk side blocking the single writer.

**Design (interfaces + test-intent; implement against the actual files):**

1. **Batch API.** Add `DiskCache::invalidate_packages(&self, package_names: &HashSet<String>)` and `ImportCache::invalidate_packages(&self, package_names: &HashSet<String>)` that scan the table/map **once**, decode each key **once**, and remove it if its decoded `package_name` is in the set. Keep the single-package methods as thin wrappers over the set form (one-element set) for existing callers/tests.
2. **Read-then-write on disk.** Collect matching keys under a **read** transaction (or a lock-free snapshot of keys), then open a short **write** txn only to remove the collected keys. Do not hold the writer during the decode scan.
3. **Single decode.** Change the matcher to decode once and test set membership, instead of `cache_key_matches_package` per (key, package).
4. **project.rs rewiring.** In `ProjectCacheRegistry::invalidate_packages`, pass the whole `HashSet<String>` down to each shard's new batch method once, replacing the `for package in names` inner loops (loaded shards at 259-263, on-disk shards at 269-281).

### Task 5.1: Batch matcher + memory batch invalidation

- [ ] **Step 1:** Read [key.rs](daemon/src/cache/key.rs), [memory.rs](daemon/src/cache/memory.rs) invalidation, and confirm the decode entry point. Add a helper `cache_key_package_name(key: &str) -> Option<String>` in `key.rs` (decode identity once → `package_name`, with the legacy-prefix fallback) so both layers share one decoder.
- [ ] **Step 2:** Failing unit test in `memory.rs` tests: seed entries for packages A, B, C; `invalidate_packages({A, C})` removes A and C's entries in a single call, leaves B; assert the decode helper is used (behavioral: only matching keys removed).
- [ ] **Step 3:** Implement `ImportCache::invalidate_packages(&HashSet<String>)` (single map scan, decode once per key via the helper, remove on membership), and make `invalidate_package` delegate to it. Mirror the disk delegation.
- [ ] **Step 4:** Run tests → green. Commit.

### Task 5.2: Disk batch invalidation, read-then-write

- [ ] **Step 1:** Read [disk.rs:320-337](daemon/src/cache/disk.rs#L320-L337) and the surrounding txn helpers. Failing test in `daemon/tests/cache_disk.rs`: seed keys for A/B/C, `invalidate_packages({A,B})` removes A and B, keeps C, and (assert via a counter or timing-free proxy if available) performs one scan. At minimum assert correctness of removals for the batch form.
- [ ] **Step 2:** Implement `DiskCache::invalidate_packages`: collect matching keys under a read txn using the single-decode helper, then remove under a short write txn. Delegate `invalidate_package` to it.
- [ ] **Step 3:** Run `cargo test -p import-lens-daemon cache_disk` + `memory` → green. Commit.

### Task 5.3: Rewire project registry

- [ ] **Step 1:** In [project.rs:250-282](daemon/src/cache/project.rs#L250-L282), replace the per-package inner loops with a single `shard.cache.invalidate_packages(&set)` (loaded) and `cache.invalidate_packages(&set)` (on-disk). Update the existing `daemon/tests/project_cache.rs`/`registry_cache.rs` if they assert the old per-package call shape.
- [ ] **Step 2:** Full suite → green. Commit.

### Part 5 gate

- [ ] `cargo fmt` · `clippy` · `cargo test -p import-lens-daemon` green.
- [ ] **Squash into one:** `perf(daemon): invalidate packages in a single decode pass`.

---

# Part 6 — package.json single manifest read

**Goal:** Stop resolving each dependency's package root + reading + parsing its `package.json` twice per package.json analysis.

**Verified problem:** Pass 1 `resolve_installed_package_version` ([service.rs:1408-1426](daemon/src/service.rs#L1408-L1426)) does ancestor-walk + read + parse to get `version`; pass 2 `analyze_with_cache` → `resolve_package_entry` → `find_package_manifest` ([resolver.rs:270-302](daemon/src/pipeline/resolver.rs#L270-L302)) re-walks + re-reads + re-parses the same `package.json`. Pass-2 resolution is structurally required (the cache key needs the fingerprints), so the eliminable redundancy is **pass 1's** separate manifest read.

**Design:** Fold the version extraction into the single manifest read that pass 2 already performs, so the walk+read+parse happens once per dependency. Options (pick the least invasive that keeps `ImportRequest.version` correct):
- (a) Have pass 1 reuse a shared/memoized manifest read that pass 2 also consumes (e.g. a short-lived per-analysis manifest map keyed by package root), or
- (b) Restructure `analyze_package_json` so a single resolution per dependency yields both the version (for the `ImportRequest`/cache key) and the resolved entry, threaded into `analyze_and_cache` without re-resolving.

Because `version` is an input to the cache key (built before pass 2), the clean shape is: **resolve once → produce `(version, ResolvedPackage)` → build request + key → `analyze_and_cache` using the already-resolved package** (there is already `analyze_resolved_import`/`prewarm_resolved_import` that accept a `ResolvedPackage`, so the plumbing exists).

### Task 6.1: Resolve each dependency once

**Files:** `daemon/src/service.rs` (`analyze_package_json`, `resolve_installed_package_version`, `analyze_with_cache`/`analyze_and_cache`), possibly `daemon/src/pipeline/resolver.rs`.

- [ ] **Step 1:** Read `analyze_package_json` ([service.rs:635-777](daemon/src/service.rs#L635-L777)), `analyze_resolved_import`/`analyze_and_cache`/`prewarm_resolved_import`, and `resolve_installed_package_version`. Confirm `ResolvedPackage` carries (or can cheaply carry) the installed version, or that the manifest read in pass 1 can hand its parsed `version` forward.
- [ ] **Step 2:** Add a failing test asserting behavior is unchanged (same states/results for a fixture package.json) — this is a refactor, so the guard is a characterization test plus a way to detect the doubled read is gone. If a read counter is impractical, rely on the characterization test + code review that the second manifest read is removed.
- [ ] **Step 3:** Refactor pass 1 to resolve the package **once** (entry + version) and thread the `ResolvedPackage` into an `analyze_resolved_*` call in pass 2, so `resolve_package_entry` is not called a second time for the same dependency. Preserve: version string in `ImportRequest`, cache-key identity, streaming partial emission order, registry-hint lookup, and the `Missing` (unresolved) branch.
- [ ] **Step 4:** `cargo test -p import-lens-daemon` (esp. package.json + streaming tests) → green.
- [ ] **Step 5:** Commit.

### Part 6 gate

- [ ] `cargo fmt` · `clippy` · full suite green; package.json streaming behavior unchanged.
- [ ] **Squash into one:** `perf(daemon): resolve each package.json dependency once`.

---

# Part 7 — Eviction caps + orphan-purge command + registry prune

**Goal:** Bound cache growth within a session and give the user a deliberate "purge orphan cache" action, closing the verified unbounded-growth + release-orphaning + registry-monotonic-growth findings. No project-wide file scan on startup — orphan detection is stat-only, on explicit user action.

**Verified problems:**
- Memory map uncapped ([memory.rs](daemon/src/cache/memory.rs)); disk entries have no age/count/size eviction ([disk.rs](daemon/src/cache/disk.rs)); shard cleanup is whole-shard and only on startup/explicit ([project.rs:162-215](daemon/src/cache/project.rs#L162-L215)).
- `ANALYZER_VERSION` embeds `CARGO_PKG_VERSION` ([key.rs:14](daemon/src/cache/key.rs#L14)) → every release changes all keys → old rows orphaned, reclaimed only on re-read (never for cold entries).
- Registry cache never evicts and the write-time union ([registry/cache.rs:126-133](daemon/src/registry/cache.rs#L126-L133)) resurrects removed entries.

**Design — three deliverables under one Part (squashed at the end):**

**7A — Memory + disk eviction caps.** Add a bounded LRU/size cap to the in-memory `ImportCache` (evict least-recently-used on insert past a cap, mirroring `GRAPH_CACHE`'s pattern), and an entry-level age/count cap on the disk table applied during the batched insert flush (drop oldest by the `recents` timestamp when over the cap). Keep it conservative — the goal is to bound, not to churn.

**7B — Orphan-purge command.** A new `ProjectCacheRegistry::purge_orphans()` that, on explicit request:
- Removes shards whose stored `project_root` ([project.rs:39-45](daemon/src/cache/project.rs#L39-L45), read via `scan_disk_shards`) no longer exists (`Path::exists`), via the existing `remove_shard_by_id`.
- Within each surviving shard, drops entries whose decoded identity's `package_root`/`entry_path` ([key.rs:29-30](daemon/src/cache/key.rs#L29-L30)) no longer exists on disk — **path existence, not `fingerprints_are_current`** (a *changed* file is not orphan; only a *missing* one is).
- Drops entries whose `analyzer_version` != current (the release-orphan class), reusing the disk decode gate.
- Also clears matching `GRAPH_CACHE`/L1 entries whose document/entry paths are gone.
- Wired behind a new IPC message + a Cache Manager UI action (deliberate user action, warned).

**7C — Registry prune.** Add a load-time age filter in `load_entries` ([registry/cache.rs:164](daemon/src/registry/cache.rs#L164)) (drop entries older than a retention window, e.g. 30d, distinct from the 6h refetch TTL) and make the purge command drop registry entries too, writing an **authoritative** snapshot that bypasses the union so purged entries are not resurrected.

### Task 7.1: Memory LRU cap

**Files:** `daemon/src/cache/memory.rs`; tests in-file.

- [ ] **Step 1:** Read `memory.rs` `insert_with_fingerprints`. Decide the cap (e.g. a `MAX_MEMORY_ENTRIES` const) and an LRU signal (add a `last_used_millis: AtomicU64` to `CachedImport`, stamped on `get`, evicted on insert-over-cap — mirror `GRAPH_CACHE` [graph.rs:269-277](daemon/src/pipeline/graph.rs#L269-L277)).
- [ ] **Step 2:** Failing test: insert `MAX + N` distinct keys → `memory_len()` ≤ `MAX`; a recently-`get`-touched key survives eviction over a cold one.
- [ ] **Step 3:** Implement the cap + LRU eviction. Ensure the eviction is best-effort/benign under concurrency (transient overshoot acceptable, like `GRAPH_CACHE`).
- [ ] **Step 4:** Tests green. Commit.

### Task 7.2: Disk entry age/count cap

**Files:** `daemon/src/cache/disk.rs`; tests in `daemon/tests/cache_disk.rs`.

- [ ] **Step 1:** Read the insert-flush path and the `recents` table. Failing test: after inserting > cap entries, a flush drops the oldest by recents timestamp so entry count stays ≤ cap; recently-touched entries survive.
- [ ] **Step 2:** Implement an entry-count (and/or age) cap enforced during `write_pending_inserts`/flush, using the existing `recents` ordering. Keep `recents` trimmed alongside (addresses the unbounded-recents note).
- [ ] **Step 3:** Tests green. Commit.

### Task 7.3: `purge_orphans` in the registry (project + disk + graph + L1)

**Files:** `daemon/src/cache/project.rs`, `daemon/src/cache/disk.rs`, `daemon/src/cache/key.rs`, `daemon/src/pipeline/graph.rs`, `daemon/src/pipeline/file_size_cache.rs`.

- [ ] **Step 1:** Add `DiskCache::purge_orphan_entries(current_analyzer_version: &str) -> usize` (iterate keys once; drop entries whose decoded `package_root`/`entry_path` is missing OR whose `analyzer_version` mismatches; return removed count) with a `cache_disk.rs` test seeding a live + a missing-path + a stale-version entry and asserting only the live one remains.
- [ ] **Step 2:** Add `ProjectCacheRegistry::purge_orphans() -> ProjectCachePurge` that: removes shards whose `project_root` is gone (via `remove_shard_by_id`), calls `purge_orphan_entries` on surviving loaded + on-disk shards, and returns a summary (shards removed, entries dropped). Test with a temp shard whose root exists vs one whose root was deleted.
- [ ] **Step 3:** Add `FileSizeCache::purge_missing_paths()` (drop entries whose document path no longer exists) and a `GRAPH_CACHE` equivalent (drop entries whose entry path is gone); small unit tests.
- [ ] **Step 4:** Tests green. Commit.

### Task 7.4: Registry prune + authoritative purge

**Files:** `daemon/src/registry/cache.rs`, `daemon/src/registry/constants.rs`.

- [ ] **Step 1:** Add a `REGISTRY_RETENTION_MS` (e.g. 30d) const. Failing test: `load_entries` drops entries with `updated_at` older than retention; fresh ones survive.
- [ ] **Step 2:** Implement the load-time age filter. Add a `purge()` that writes an authoritative snapshot **without** the on-disk union (so deletions stick), with a test that a purged entry stays gone across a reload.
- [ ] **Step 3:** Tests green. Commit.

### Task 7.5: IPC + Cache Manager UI wiring

**Files:** `daemon/src/ipc/protocol.rs`, `daemon/src/ipc/server.rs`, `daemon/src/service.rs`, `extension/src/ipc/protocol.ts`, `extension/src/ui/cacheManager.ts`, `extension/src/ui/cacheManagerItems.ts`, `extension/src/ui/cacheManagerRequests.ts`.

- [ ] **Step 1:** Read the existing Cache Manager remove/cleanup flow (`remove_cache` at [service.rs:1045-1077](daemon/src/service.rs#L1045-L1077); `cacheManagerActionItems` at [cacheManagerItems.ts:39-70](extension/src/ui/cacheManagerItems.ts#L39-L70); the request/handler wiring in `cacheManager.ts`/`cacheManagerRequests.ts`).
- [ ] **Step 2:** Add a `CachePurgeOrphans` message (or a `CacheRemoveScope::Orphans` variant) in the daemon + extension protocol, with a matching daemon handler that calls `registry.purge_orphans()` + registry-cache `purge()` + `clear_module_graph_cache()`/L1 purge, returning the summary. Add daemon-side codec/handler tests mirroring existing cache-message tests.
- [ ] **Step 2b (clear-all also flushes L1):** The existing "Clear all caches" / `remove_cache` path ([service.rs:1045-1077](daemon/src/service.rs#L1045-L1077)) does **not** call `bump_cache_generation()`, so the memory-only L1 `FileSizeCache` (Part 1) keeps serving cached totals until a later generation bump. Add a `FileSizeCache::clear()` (in Task 7.3) and call it from the clear-all/remove handlers so a user "Clear caches" forces a fresh status-bar size on the next request. Benign for correctness (sizes are deterministic from package contents), but matches the expectation that clearing caches refreshes everything.
- [ ] **Step 3:** Add a "Purge orphan cache (all projects)" action item + handler + request builder in the Cache Manager UI, with a confirmation warning (deliberate, irreversible-ish action). Mirror the existing `clearAllCaches` wiring. Add/extend `cacheManagerRequests.test.ts` / `cacheManagerItems.test.ts`.
- [ ] **Step 4:** Rust + TS suites green. Commit.

### Part 7 gate

- [ ] `cargo fmt` · `clippy` · `cargo test -p import-lens-daemon` green; extension lint + tests green.
- [ ] **Squash Tasks 7.1–7.5 into one:** `feat: bound cache growth and add orphan-purge action`.

---

# Deferred (not scheduled — need their own design pass)

These were discussed and are real, but each is a larger correctness-sensitive change better done deliberately after the above lands. Kept here so nothing is lost.

- **Stale-while-revalidate for bundle sizes.** Return a stale `ImportResult` (flagged) + background recompute instead of delete-on-stale, hooking `memory.rs:100-105` and revalidating in the service layer with an in-flight dedupe set. **Must** distinguish "file changed" (serve stale + refresh) from "file gone" (keep delete). Lower value than registry SWR (bundle recompute is local CPU, not network) — the registry path already implements SWR and is the reference. Consider only for expensive graph-backed recomputes.
- **Drop entry/manifest fingerprint from the cache key.** [key.rs:34-35,67-69](daemon/src/cache/key.rs#L34-L35) embeds fingerprints in the key, so mtime-only changes (`npm ci`, `git checkout`) mint a new key and orphan the old; it is redundant with the value-side `dependency_fingerprints` revalidation. Removing it (cache identity v3→v4) makes same-key overwrite instead of orphan. Bigger blast radius; also note the Part 1 L1 signature derives from this key, so if pursued, the L1 signature must fold fingerprints in independently.

---

## Self-review checklist (run before executing)

- Every Part ends in one squashed commit; the commit map table matches the Part headers.
- Parts 1–4 carry full code; Parts 5–7 carry interfaces + tests + hook points and instruct the implementer to read the named file before writing per-line code (deliberate, to avoid guessing internals).
- No Part depends on a Deferred item.
- Part 1's `file_size_signature`/`FileSizeCache` names are used consistently by Task 1.4's `file_size_with_cache`.
- Status-bar `statusBarText`/`StatusBarState`/`setState` names are consistent across Tasks 2.1–2.3 and the `listener.ts` migration.

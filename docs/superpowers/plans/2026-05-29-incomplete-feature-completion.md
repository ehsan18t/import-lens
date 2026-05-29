# Incomplete Feature Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the real incomplete ImportLens features that block a high-grade v1.0 release: accurate OXC-based size computation, durable cache schema, daemon lifecycle, package prewarm, workspace reporting, and the later WASM fallback tier.

**Architecture:** Keep the TypeScript extension host as the UI, document parser, and daemon coordinator. Move all heavy size computation into focused Rust daemon modules: resolver, graph, tree-shaker, minifier, cache, lifecycle, and prewarm. Implement native correctness first, then build the WASM tier on the same protocol and service boundaries.

**Tech Stack:** TypeScript 6, VS Code Extension API, Node test runner, Rust 2024, Tokio, Rayon, redb, papaya, rmp-serde, flate2, brotli, zstd, OXC crates (`oxc_parser`, `oxc_resolver`, `oxc_semantic`, `oxc_transformer`, `oxc_minifier`, `oxc_mangler`, `oxc_codegen`).

---

## Confirmed Incomplete Implementations

These are real gaps verified against the current code and SRS:

- `daemon/src/pipeline/analyze.rs` is still the Windows-alpha static entry analyzer. The SRS explicitly says it does not satisfy FR-018 or FR-019.
- `daemon/Cargo.toml` does not include the OXC Rust crates required by the full graph, semantic, transform, minify, mangle, and codegen pipeline.
- `daemon/src/cache/disk.rs` uses a single `imports` table, has no metadata table, no schema version, no startup preload into papaya, and no corrupt database recovery as required by FR-026 and FR-026a.
- `extension/src/daemon/manager.ts` has native daemon startup only. The status bar knows about `wasm`, but there is no WASM transport or worker fallback.
- `extension/src/ui/report.ts` renders only `AnalysisStore.all()`, which is the currently known in-memory state, not all imports in the workspace as required by FR-036.
- There is no prewarm protocol or package.json open/save prewarm workflow for FR-028.
- Daemon recycle handling is absent. Extension crash backoff also does not match FR-015/NFR-004b: it uses 250ms-derived delays, a 5 second stability reset, and 5 failures instead of 1s/2s/4s/8s/30s and 3 failures within 60 seconds.

## Execution Order

1. Native correctness: OXC resolver, graph, tree-shake, minify, mangle, codegen.
2. Cache durability: redb schema, metadata, startup preload, corruption recovery.
3. Lifecycle reliability: correct backoff, graceful shutdown escalation, daemon recycle loop guard.
4. Background prewarm: package.json open/save pre-calculation with cancellation.
5. Workspace report: scan workspace imports and compute missing results on demand.
6. WASM fallback: protocol transport abstraction, worker runtime, packaging.
7. Acceptance coverage: real package fixtures, performance gates, release checklist.

This should be executed as separate commits per task. Do not mix phases in a single commit.

---

## File Structure

### Rust Daemon

- Create `daemon/src/pipeline/resolver.rs`
  - Owns package discovery from `active_document_path`, package name validation, package manifest loading, and OXC-backed resolution.
- Create `daemon/src/pipeline/graph.rs`
  - Owns parsed module records, import/export edges, and graph traversal state.
- Create `daemon/src/pipeline/reachability.rs`
  - Owns symbol reachability from requested exports, side-effect inclusion, and conservative fallback decisions.
- Create `daemon/src/pipeline/bundle.rs`
  - Owns virtual entry generation, module concatenation, and module-scope renaming.
- Create `daemon/src/pipeline/minify.rs`
  - Owns OXC transform, minifier, mangler, codegen, and minified source emission.
- Modify `daemon/src/pipeline/analyze.rs`
  - Convert it into orchestration around the new modules. Keep the current static estimator only as an explicitly named fallback for non-tree-shakeable CJS or unsupported input, with diagnostics.
- Modify `daemon/src/pipeline/mod.rs`
  - Export the new modules.
- Modify `daemon/Cargo.toml`
  - Add OXC crates matching the SRS version pins.
- Create `daemon/tests/fixtures/packages/*`
  - Commit pinned package snapshots for lodash-es, date-fns, zod, react, and uuid.
- Add/modify `daemon/tests/analyze.rs`
  - Add named/default/namespace/dynamic import accuracy tests.
- Create `daemon/tests/graph.rs`
  - Add focused graph and reachability tests.
- Create `daemon/tests/cache_disk.rs`
  - Add schema, preload, corruption recovery, and invalidation tests.
- Create `daemon/src/lifecycle.rs`
  - Own daemon uptime, idle tracking, cache-size recycle trigger, and recycle counter writes.
- Create `daemon/src/prefetch.rs`
  - Own prewarm queue, secondary Rayon pool, cancellation token, and package.json dependency extraction.
- Modify `daemon/src/ipc/protocol.rs`
  - Add prewarm message type and any lifecycle acknowledgement needed by tests.
- Modify `daemon/src/ipc/server.rs`
  - Wire prewarm, cancellation on real batch requests, graceful shutdown, and lifecycle checks.
- Modify `daemon/src/service.rs`
  - Expose cache length and prewarm entry points without coupling to IPC.

### TypeScript Extension

- Create `extension/src/daemon/recycleGuard.ts`
  - Owns reading/writing `<globalStoragePath>/importlens-recycles.json`, rolling-window logic, and clean-session reset.
- Modify `extension/src/daemon/manager.ts`
  - Fix FR-015 backoff, add recycle guard, graceful shutdown escalation, and later transport selection.
- Create `extension/src/daemon/transport.ts`
  - Define `AnalysisTransport` shared by native IPC and WASM worker transports.
- Create `extension/src/daemon/nativeTransport.ts`
  - Wrap existing `IpcClient` behavior behind `AnalysisTransport`.
- Create `extension/src/daemon/wasmTransport.ts`
  - Later phase: start WASM worker and map postMessage to the protocol.
- Create `extension/src/prewarm/packageJson.ts`
  - Detect opened/saved package.json files and send prewarm requests.
- Create `extension/src/report/workspaceScanner.ts`
  - Finds supported workspace files, excludes `node_modules`, extracts imports, resolves package versions.
- Create `extension/src/report/reportModel.ts`
  - Builds sorted report rows from scan results and daemon responses.
- Modify `extension/src/ui/report.ts`
  - Render the workspace report model, not only current in-memory store state.
- Modify `extension/src/extension.ts`
  - Register package.json prewarm listeners and pass daemon/report dependencies.
- Add/modify tests under `extension/test/daemon`, `extension/test/prewarm`, and `extension/test/report`.

---

## Task 1: Add Real Package Fixtures and Baseline Failing Tests

**Files:**
- Create: `daemon/tests/fixtures/packages/README.md`
- Create: `daemon/tests/fixtures/packages/lodash-es@<pinned>/`
- Create: `daemon/tests/fixtures/packages/date-fns@<pinned>/`
- Create: `daemon/tests/fixtures/packages/zod@<pinned>/`
- Create: `daemon/tests/fixtures/packages/react@<pinned>/`
- Create: `daemon/tests/fixtures/packages/uuid@<pinned>/`
- Modify: `daemon/tests/analyze.rs`

- [ ] **Step 1: Pin fixture versions**

Use these exact versions unless the current lockfile already contains a different pinned fixture decision:

```text
lodash-es@4.17.21
date-fns@4.1.0
zod@4.1.13
react@19.2.3
uuid@13.0.0
```

Generate snapshots once using pnpm outside source code, then copy only the package directories into `daemon/tests/fixtures/packages/<name>@<version>/node_modules/<name>/`.

- [ ] **Step 2: Add fixture documentation**

Write `daemon/tests/fixtures/packages/README.md`:

```markdown
# Package Fixtures

These package snapshots are committed so daemon integration tests never read from
the live npm registry. Each fixture contains a minimal `package.json` workspace
with a local `node_modules` tree for one pinned package version.

Do not update these snapshots as drive-by dependency churn. Updating a fixture
requires regenerating expected size assertions in daemon integration tests.
```

- [ ] **Step 3: Add failing integration tests**

Add tests to `daemon/tests/analyze.rs` that assert named imports are smaller than namespace imports for tree-shakeable ESM packages:

```rust
#[test]
fn analyze_lodash_named_import_is_smaller_than_namespace_import() {
    let fixture = fixture_workspace("lodash-es@4.17.21");
    let context = AnalysisContext {
        workspace_root: fixture.clone(),
        active_document_path: fixture.join("src/app.ts"),
    };

    let named = analyze_import(&context, &import_request("lodash-es", "lodash-es", ImportKind::Named, &["debounce"]));
    let namespace = analyze_import(&context, &import_request("lodash-es", "lodash-es", ImportKind::Namespace, &[]));

    assert_eq!(named.error, None);
    assert_eq!(namespace.error, None);
    assert!(named.brotli_bytes > 0);
    assert!(namespace.brotli_bytes > 0);
    assert!(
        named.brotli_bytes < namespace.brotli_bytes,
        "named import should be smaller than namespace import: named={named:?}, namespace={namespace:?}",
    );
}
```

Also add similar tests for:
- `date-fns` named `format`
- `uuid` named `v4`
- `react` default import with conservative CJS/side-effect diagnostics
- `zod` namespace import as full module entry

- [ ] **Step 4: Run tests to verify failure**

Run:

```powershell
pnpm test:rust
```

Expected: the new named-vs-namespace assertions fail against the current static-entry analyzer because it measures whole entry files instead of reachable exports.

- [ ] **Step 5: Commit**

```powershell
git add daemon/tests/fixtures/packages daemon/tests/analyze.rs
git commit -m "test: add real package size fixtures"
```

---

## Task 2: Add OXC Dependencies and Split Pipeline Modules

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/src/pipeline/mod.rs`
- Create: `daemon/src/pipeline/resolver.rs`
- Create: `daemon/src/pipeline/graph.rs`
- Create: `daemon/src/pipeline/reachability.rs`
- Create: `daemon/src/pipeline/bundle.rs`
- Create: `daemon/src/pipeline/minify.rs`

- [ ] **Step 1: Add OXC dependencies**

Update `daemon/Cargo.toml`:

```toml
oxc_allocator = "~0.133"
oxc_ast = "~0.133"
oxc_codegen = "~0.133"
oxc_mangler = "~0.133"
oxc_minifier = "~0.133"
oxc_parser = "~0.133"
oxc_resolver = "~11.19"
oxc_semantic = "~0.133"
oxc_span = "~0.133"
oxc_transformer = "~0.133"
```

- [ ] **Step 2: Export module boundaries**

Update `daemon/src/pipeline/mod.rs`:

```rust
pub mod analyze;
pub mod bundle;
pub mod compress;
pub mod graph;
pub mod minify;
pub mod reachability;
pub mod resolver;
```

- [ ] **Step 3: Create data structures first**

Create `daemon/src/pipeline/graph.rs` with serializable/debuggable internal structs:

```rust
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub path: PathBuf,
    pub source: String,
    pub imports: Vec<ImportEdge>,
    pub exports: Vec<ExportRecord>,
    pub has_top_level_side_effects: bool,
}

#[derive(Debug, Clone)]
pub struct ImportEdge {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportRecord {
    pub exported_name: String,
    pub local_name: String,
}

#[derive(Debug, Default, Clone)]
pub struct ModuleGraph {
    pub modules: Vec<ModuleRecord>,
}
```

- [ ] **Step 4: Run compile check**

Run:

```powershell
cargo check -p import-lens-daemon
```

Expected: PASS after empty module files compile.

- [ ] **Step 5: Commit**

```powershell
git add daemon/Cargo.toml Cargo.lock daemon/src/pipeline
git commit -m "refactor: split daemon pipeline modules"
```

---

## Task 3: Implement OXC-Backed Package Resolution

**Files:**
- Modify: `daemon/src/pipeline/resolver.rs`
- Modify: `daemon/src/pipeline/analyze.rs`
- Test: `daemon/tests/analyze.rs`

- [ ] **Step 1: Add resolver tests first**

Add tests that assert resolution starts from `active_document_path`, rejects unsafe package names, honors `exports`, and uses ESM conditions before CJS.

```rust
#[test]
fn resolver_rejects_traversal_package_names() {
    let context = AnalysisContext {
        workspace_root: PathBuf::from("C:/workspace"),
        active_document_path: PathBuf::from("C:/workspace/src/app.ts"),
    };

    let request = import_request("../evil", "../evil", ImportKind::Namespace, &[]);
    let result = analyze_import(&context, &request);

    assert!(result.error.as_deref().unwrap_or("").contains("unsafe package name"));
}
```

- [ ] **Step 2: Implement resolver API**

Create public API in `resolver.rs`:

```rust
use crate::ipc::protocol::ImportRequest;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub package_root: PathBuf,
    pub package_json: Value,
    pub entry_path: PathBuf,
    pub is_cjs: bool,
    pub side_effects: bool,
}

pub fn resolve_package_entry(
    active_document_path: &std::path::Path,
    request: &ImportRequest,
) -> Result<ResolvedPackage, String> {
    // Use oxc_resolver for package entry resolution.
    // Keep package-name validation before any path construction.
    // Preserve the existing manual resolver only as a compatibility fallback
    // for packages oxc_resolver cannot describe clearly.
    todo!("implemented in this task")
}
```

Replace the `todo!` before committing. The implementation must not use workspace root as the starting point.

- [ ] **Step 3: Wire resolver into analyzer**

In `analyze.rs`, replace `find_package_manifest` plus `resolve_entry_path` call sites with `resolve_package_entry(&context.active_document_path, request)`.

- [ ] **Step 4: Verify tests**

Run:

```powershell
pnpm test:rust
```

Expected: resolver tests pass; named-vs-namespace size tests from Task 1 still fail until graph/tree-shake is implemented.

- [ ] **Step 5: Commit**

```powershell
git add daemon/src/pipeline/resolver.rs daemon/src/pipeline/analyze.rs daemon/tests/analyze.rs Cargo.lock
git commit -m "feat: resolve package entries with oxc resolver"
```

---

## Task 4: Build Module Graph and Reachability

**Files:**
- Modify: `daemon/src/pipeline/graph.rs`
- Modify: `daemon/src/pipeline/reachability.rs`
- Modify: `daemon/src/pipeline/analyze.rs`
- Test: `daemon/tests/graph.rs`

- [ ] **Step 1: Write graph tests**

Create `daemon/tests/graph.rs`:

```rust
#[test]
fn graph_marks_only_requested_named_export_reachable() {
    let graph = graph_from_sources([
        ("entry.js", "export { used } from './lib.js';"),
        ("lib.js", "export const used = 1; export const unused = heavy();"),
    ]);

    let reachable = reachable_exports(&graph, &["used".to_owned()], false);

    assert!(reachable.contains_symbol("used"));
    assert!(!reachable.contains_symbol("unused"));
}
```

- [ ] **Step 2: Implement graph parser**

Use `oxc_parser` and `oxc_semantic` to parse each module and extract:
- static import specifiers
- export declarations
- re-export declarations
- top-level side-effect markers

Top-level side-effect markers must conservatively include:
- expression statements
- assignments
- calls
- class static blocks
- import of files with no imported bindings

- [ ] **Step 3: Implement reachability**

Rules:
- Namespace and dynamic imports mark the entry module as fully reachable.
- Default imports mark `default` and dependencies reachable.
- Named imports mark only requested names and their transitive dependencies reachable.
- If `sideEffects` is absent, `true`, or array, include modules with side effects even if their exports are not used.
- If `sideEffects` is `false`, prune modules without reachable symbols or side effects.

- [ ] **Step 4: Run tests**

Run:

```powershell
cargo test -p import-lens-daemon --test graph
pnpm test:rust
```

Expected: graph tests pass; integration size tests may still fail until bundle/minify is wired.

- [ ] **Step 5: Commit**

```powershell
git add daemon/src/pipeline/graph.rs daemon/src/pipeline/reachability.rs daemon/src/pipeline/analyze.rs daemon/tests/graph.rs
git commit -m "feat: build daemon module graph reachability"
```

---

## Task 5: Implement Transform, Scope-Safe Bundle, Minify, Mangle, Codegen

**Files:**
- Modify: `daemon/src/pipeline/bundle.rs`
- Modify: `daemon/src/pipeline/minify.rs`
- Modify: `daemon/src/pipeline/analyze.rs`
- Test: `daemon/tests/analyze.rs`

- [ ] **Step 1: Add collision test**

Add a fixture with two modules that both declare the same local binding:

```rust
#[test]
fn bundle_renames_module_scoped_bindings_to_avoid_collisions() {
    let fixture = fixture_workspace("scope-collision");
    let context = AnalysisContext {
        workspace_root: fixture.clone(),
        active_document_path: fixture.join("src/app.ts"),
    };

    let result = analyze_import(&context, &import_request("scope-collision", "scope-collision", ImportKind::Named, &["sum"]));

    assert_eq!(result.error, None);
    assert!(result.minified_bytes > 0);
    assert!(result.brotli_bytes > 0);
}
```

- [ ] **Step 2: Implement virtual entry generation**

Generate these exact virtual entries:

```javascript
export { debounce } from 'lodash-es';
export { default } from 'react';
export * from 'zod';
```

Dynamic imports skip virtual entry and resolve the package entry directly.

- [ ] **Step 3: Implement bundle output**

`bundle.rs` must emit one JavaScript string containing only reachable module code. Apply module-prefix renaming before concatenation, for example `__il_m3_value`.

- [ ] **Step 4: Implement OXC minification**

`minify.rs` must:
- run `oxc_transformer` to strip TypeScript and lower JSX
- run `oxc_minifier`
- run `oxc_mangler`
- run `oxc_codegen` with minify enabled
- return the final JavaScript string

- [ ] **Step 5: Wire compression after codegen**

In `analyze.rs`, compute:

```rust
let raw_bytes = bundled_source.len() as u64;
let minified = minify_source(&bundled_source)?;
let compressed = compress_all(&minified)?;
```

- [ ] **Step 6: Verify real fixture tests**

Run:

```powershell
pnpm test:rust
```

Expected: lodash-es/date-fns/uuid named imports are smaller than namespace imports; CJS packages are marked conservative.

- [ ] **Step 7: Commit**

```powershell
git add daemon/src/pipeline/bundle.rs daemon/src/pipeline/minify.rs daemon/src/pipeline/analyze.rs daemon/tests
git commit -m "feat: compute import sizes with oxc pipeline"
```

---

## Task 6: Implement Persistent Cache Schema and Startup Preload

**Files:**
- Modify: `daemon/src/cache/disk.rs`
- Modify: `daemon/src/cache/memory.rs`
- Modify: `daemon/src/service.rs`
- Test: `daemon/tests/cache_disk.rs`

- [ ] **Step 1: Add disk cache tests**

Create `daemon/tests/cache_disk.rs` with tests for:
- creates metadata table with schema version `1`
- reloads disk entries into memory on service startup
- deletes and recreates database when schema mismatches
- clears both disk and memory on `CacheInvalidateAll`

- [ ] **Step 2: Rename table and add metadata**

Use these table names:

```rust
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 1;
```

- [ ] **Step 3: Implement corruption recovery**

If `Database::create`, `begin_read`, metadata open, or cache table open fails on startup:
- log a warning through a daemon logger abstraction
- delete `importlens.redb`
- create a fresh database
- continue with memory cache enabled

- [ ] **Step 4: Implement preload**

Add `DiskCache::load_all() -> Vec<(String, ImportResult)>` and call it from `ImportCache::new()` to fill papaya.

- [ ] **Step 5: Run tests**

```powershell
cargo test -p import-lens-daemon --test cache_disk
pnpm test:rust
```

- [ ] **Step 6: Commit**

```powershell
git add daemon/src/cache daemon/src/service.rs daemon/tests/cache_disk.rs
git commit -m "feat: add versioned persistent cache schema"
```

---

## Task 7: Fix Daemon Lifecycle, Backoff, Shutdown, and Recycle Guard

**Files:**
- Create: `daemon/src/lifecycle.rs`
- Modify: `daemon/src/main.rs`
- Modify: `daemon/src/ipc/server.rs`
- Modify: `daemon/src/service.rs`
- Create: `extension/src/daemon/recycleGuard.ts`
- Modify: `extension/src/daemon/manager.ts`
- Test: `daemon/tests/lifecycle.rs`
- Test: `extension/test/daemon/recycleGuard.test.ts`

- [ ] **Step 1: Add recycle guard tests**

Test extension behavior:

```ts
test("recycle guard blocks more than five recycles in ten minutes", async () => {
  const guard = new RecycleGuard(tempStoragePath);
  const now = 1_800_000;

  await guard.recordRecycleTimes([now - 100, now - 90, now - 80, now - 70, now - 60, now - 50]);

  assert.equal(await guard.shouldEnterDegradedMode(now), true);
});
```

- [ ] **Step 2: Fix crash backoff**

In `DaemonManager`, use:
- first restart after `1000ms`
- then `2000ms`, `4000ms`, `8000ms`, capped at `30000ms`
- enter unavailable after 3 crashes inside a rolling 60 second window

- [ ] **Step 3: Implement graceful shutdown escalation**

On deactivate:
1. send `shutdown`
2. wait up to 5 seconds for daemon exit
3. send `SIGTERM` on Unix or kill process on Windows
4. wait 2 more seconds on Unix
5. send `SIGKILL` on Unix

- [ ] **Step 4: Implement daemon recycle**

Daemon recycles when:
- uptime is over 4 hours and no `BatchRequest` occurred in the last 15 minutes
- cache length exceeds 200,000 entries

Before exiting:
- abort prewarm jobs
- write recycle timestamp to `<storage>/importlens-recycles.json`
- flush/close disk cache
- exit with code `0`

- [ ] **Step 5: Run tests**

```powershell
pnpm test:ts
pnpm test:rust
```

- [ ] **Step 6: Commit**

```powershell
git add daemon/src/lifecycle.rs daemon/src/main.rs daemon/src/ipc/server.rs daemon/src/service.rs extension/src/daemon/recycleGuard.ts extension/src/daemon/manager.ts daemon/tests/lifecycle.rs extension/test/daemon/recycleGuard.test.ts
git commit -m "feat: harden daemon lifecycle management"
```

---

## Task 8: Implement Package.json Prewarm

**Files:**
- Modify: `daemon/src/ipc/protocol.rs`
- Modify: `daemon/src/ipc/server.rs`
- Create: `daemon/src/prefetch.rs`
- Modify: `daemon/src/service.rs`
- Create: `extension/src/prewarm/packageJson.ts`
- Modify: `extension/src/extension.ts`
- Test: `daemon/tests/prefetch.rs`
- Test: `extension/test/prewarm/packageJson.test.ts`

- [ ] **Step 1: Add protocol message**

Add a client message:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct PrewarmPackageJsonMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub package_json_path: String,
    pub active_document_path: String,
}
```

TypeScript mirror:

```ts
export interface PrewarmPackageJsonMessage {
  type: "prewarm_package_json";
  package_json_path: string;
  active_document_path: string;
}
```

- [ ] **Step 2: Add prewarm extension listeners**

Register:
- `workspace.onDidOpenTextDocument`
- `workspace.onDidSaveTextDocument`

Only send when `path.basename(document.uri.fsPath) === "package.json"`.

- [ ] **Step 3: Implement daemon prewarm**

`prefetch.rs` reads `dependencies` and `devDependencies`, then queues two requests per package:
- default export
- namespace export

Use a secondary Rayon thread pool sized to half the primary pool, minimum 1.

- [ ] **Step 4: Cancel on real work**

When a `BatchRequest` arrives:
- signal prewarm cancellation
- do not block user request processing
- discard incomplete prewarm results

- [ ] **Step 5: Run tests**

```powershell
pnpm test:ts
pnpm test:rust
```

- [ ] **Step 6: Commit**

```powershell
git add daemon/src/prefetch.rs daemon/src/ipc daemon/src/service.rs extension/src/prewarm extension/src/extension.ts daemon/tests/prefetch.rs extension/test/prewarm
git commit -m "feat: prewarm package import sizes"
```

---

## Task 9: Implement Real Workspace Report

**Files:**
- Create: `extension/src/report/workspaceScanner.ts`
- Create: `extension/src/report/reportModel.ts`
- Modify: `extension/src/ui/report.ts`
- Modify: `extension/src/extension.ts`
- Test: `extension/test/report/workspaceScanner.test.ts`
- Test: `extension/test/report/reportModel.test.ts`

- [ ] **Step 1: Add scanner tests**

Test pure file filtering and sorting logic without VS Code APIs where possible:

```ts
test("report model sorts imports by brotli size descending", () => {
  const rows = buildReportRows([
    stateWithResult("small", 10),
    stateWithResult("large", 100),
  ]);

  assert.deepEqual(rows.map((row) => row.specifier), ["large", "small"]);
});
```

- [ ] **Step 2: Implement workspace scan**

`workspaceScanner.ts` must:
- use `vscode.workspace.findFiles("**/*.{js,jsx,ts,tsx,svelte,astro}", "**/{node_modules,dist,build,out,coverage}/**")`
- read documents through VS Code workspace APIs
- reuse `extractRuntimeImports`
- reuse `resolveInstalledPackage`
- chunk daemon batches to avoid huge single messages

- [ ] **Step 3: Render report**

`report.ts` must:
- show a progress notification while scanning
- render package, specifier, source file, line, runtime, minified, gzip, brotli, zstd, warning state
- sort by brotli descending
- escape all HTML
- keep scripts disabled

- [ ] **Step 4: Run tests**

```powershell
pnpm test:ts
```

- [ ] **Step 5: Commit**

```powershell
git add extension/src/report extension/src/ui/report.ts extension/src/extension.ts extension/test/report
git commit -m "feat: build workspace import report"
```

---

## Task 10: Add Transport Abstraction for Native and WASM

**Files:**
- Create: `extension/src/daemon/transport.ts`
- Create: `extension/src/daemon/nativeTransport.ts`
- Modify: `extension/src/daemon/manager.ts`
- Test: `extension/test/daemon/transport.test.ts`

- [ ] **Step 1: Define shared transport interface**

```ts
export interface AnalysisTransport extends vscode.Disposable {
  readonly state: "ready" | "unavailable";
  start(): Promise<"ready" | "unavailable">;
  sendBatch(request: BatchRequest): Promise<BatchResponse | null>;
  invalidatePackage(packageName: string): void;
  invalidateAll(): void;
  shutdown(): Promise<void>;
}
```

- [ ] **Step 2: Move current native logic**

Move current `DaemonManager` native process and `IpcClient` logic into `NativeDaemonTransport`.

- [ ] **Step 3: Keep manager as coordinator**

`DaemonManager` should:
- select native transport first
- later fall back to WASM transport when native unavailable
- expose the same public API used by `DocumentAnalysisController`

- [ ] **Step 4: Run tests**

```powershell
pnpm test:ts
```

- [ ] **Step 5: Commit**

```powershell
git add extension/src/daemon/transport.ts extension/src/daemon/nativeTransport.ts extension/src/daemon/manager.ts extension/test/daemon/transport.test.ts
git commit -m "refactor: abstract daemon analysis transport"
```

---

## Task 11: Implement WASM Desktop Fallback

**Files:**
- Modify: `Cargo.toml`
- Modify: `daemon/Cargo.toml`
- Create: `extension/src/daemon/wasmTransport.ts`
- Create: `extension/src/daemon/wasmWorker.ts`
- Modify: `package.json`
- Modify: `scripts/build-daemon.mjs`
- Modify: `scripts/package-vsix.mjs`
- Modify: `.github/workflows/release.yml`
- Test: `extension/test/daemon/wasmTransport.test.ts`

- [ ] **Step 1: Add WASM build target**

Add a package script:

```json
"build:daemon:wasm": "cargo build -p import-lens-daemon --release --target wasm32-wasip1-threads"
```

- [ ] **Step 2: Add worker transport**

`wasmTransport.ts` must implement `AnalysisTransport` and use the same `BatchRequest`, `BatchResponse`, invalidate, and shutdown semantics as native.

- [ ] **Step 3: Package WASM asset**

Include:

```json
"wasm/"
```

in `package.json.files`, and copy the WASM binary into `wasm/import-lens-daemon.wasm`.

- [ ] **Step 4: Fallback ordering**

Manager startup order:
1. native transport
2. WASM transport on VS Code Desktop only
3. degraded unavailable mode

- [ ] **Step 5: Run tests and package**

```powershell
pnpm check
pnpm test
pnpm package:win32-x64
```

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml daemon/Cargo.toml extension/src/daemon package.json scripts .github/workflows/release.yml extension/test/daemon
git commit -m "feat: add wasm daemon fallback"
```

---

## Task 12: Acceptance Coverage and Release Gates

**Files:**
- Modify: `package.json`
- Modify: `.github/workflows/release.yml`
- Create: `scripts/check-coverage.mjs`
- Create: `docs/release-checklist.md`
- Modify: `README.md`

- [ ] **Step 1: Add coverage gate**

Add a Rust coverage command suitable for CI, then enforce at least 70% daemon core computation line coverage. If using `cargo llvm-cov`, add a pinned install step in CI and a script:

```json
"coverage:rust": "cargo llvm-cov --workspace --fail-under-lines 70"
```

- [ ] **Step 2: Add performance smoke checks**

Create a deterministic benchmark test command that verifies:
- cache hit under 50ms
- typical fixture miss under 500ms on CI-class hardware with a generous CI multiplier

- [ ] **Step 3: Update README accessibility note**

Add the FR-039b note:

```markdown
ImportLens uses VS Code inlay hints by default because they are part of the
editor document model and are exposed to accessibility tooling. End-of-line
decorations remain available, but they are less accessible to screen readers.
```

- [ ] **Step 4: Run full release verification**

```powershell
pnpm check
pnpm test
cargo fmt --check
pnpm package:win32-x64
pnpm docker:build
```

- [ ] **Step 5: Commit**

```powershell
git add package.json .github/workflows/release.yml scripts/check-coverage.mjs docs/release-checklist.md README.md
git commit -m "test: add release acceptance gates"
```

---

## Self-Review

Spec coverage:
- FR-016 through FR-024 are covered by Tasks 1 through 5.
- FR-026 and FR-026a are covered by Task 6.
- FR-015, FR-038, NFR-004a, NFR-004b, and NFR-004c are covered by Task 7.
- FR-028 is covered by Task 8.
- FR-036 is covered by Task 9.
- WASM Tier 2 and the `web` packaging target are covered by Tasks 10 and 11.
- NFR-017 and FR-039b are covered by Task 12.

Intentional ordering:
- Do not implement WASM before Task 10. The transport abstraction prevents duplicating native protocol behavior in a worker path.
- Do not implement prewarm before the real OXC pipeline. Prewarming inaccurate static-entry estimates would populate the cache with wrong results.
- Do not publish a release VSIX until Tasks 1 through 9 and 12 pass. Task 11 can be treated as post-native if the release target is explicitly native-only and the SRS is updated to mark WASM as v1.1.

Verification baseline before execution:

```powershell
pnpm check
pnpm test
cargo fmt --check
pnpm package:win32-x64
pnpm docker:build
```

Expected: all currently pass before beginning Task 1.

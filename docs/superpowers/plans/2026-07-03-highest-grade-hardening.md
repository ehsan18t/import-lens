# Highest-Grade Codebase Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every verified correctness bug, performance issue, dead-code accumulation, duplication, and build/CLI defect found in the deep review, each as its own tested commit, without regressing any existing behavior.

**Architecture:** ImportLens is a TypeScript VS Code extension host driving a Rust daemon over a length-prefixed MessagePack IPC channel. Fixes touch four daemon areas (pipeline, cache, ipc/document, registry) and four extension areas (daemon transport, UI, analysis, scripts/CLI). Every fix is test-first (RED -> GREEN -> REFACTOR -> commit); daemon changes end with a Windows rebuild + hash refresh.

**Tech Stack:** Rust (oxc, tokio, papaya, redb, rayon, ureq) / TypeScript 6.x (tsdown; tests via tsc + node:test) / Node ESM scripts (node:test) / pnpm.

**Validation status:** Every task below carries a `Verified:` line describing the exact source evidence read during the 2026-07-03 validation pass. Findings that did NOT survive verification were dropped (see Part F). Two plan defects found during validation are already corrected here: the extension test harness is `node --test` on compiled `extension/test-dist` output (NOT vitest), and the insights fix must also clear stale insights when the recomputed set is empty.

## Global Constraints

- Package manager is `pnpm` only - never `npm`/`yarn`. (AGENTS.md)
- Windows (win32-x64) is the primary supported platform; keep Windows build + packaging green. (AGENTS.md)
- All files use LF line endings; never save CRLF. (AGENTS.md)
- TypeScript: prefer arrow functions; no double casting / unnecessary cast chains. (AGENTS.md)
- The SRS `docs/ImportLens-SRS.md` is the source of truth; if any fix changes behavior the SRS describes, update the SRS in the same task. (AGENTS.md)
- Add/update tests for every behavior change and bug fix; do not add unnecessary tests. (AGENTS.md)
- Daemon changes: run `cargo fmt` + daemon tests; rebuild/package for Windows and refresh `extension/src/daemon/knownHashes.generated.ts` before final handoff. (AGENTS.md)
- Extension changes: run `pnpm check` + extension tests. (AGENTS.md)
- Commits are focused per task with professional messages explaining the user-visible change and technical rationale. (AGENTS.md)
- IPC invariants (do not change): 4-byte big-endian length prefix, 32 MiB max frame, MessagePack (`rmp_serde::to_vec_named` <-> `@msgpack/msgpack`), protocol version 7. (SRS FR-010/FR-011)
- Compression levels (do not change): gzip 6, brotli 4 (window 22), zstd 3. (SRS FR-020)
- Graph limits (do not change): 2000 modules, 20 MiB per module, 100 MiB total. (SRS FR-018)

## Test Harness Notes (validated 2026-07-03)

- **Extension tests** are plain `node:test` files under `extension/test/**/*.test.ts`, compiled by `tsc -p tsconfig.test.json` into `extension/test-dist`, then run with `node --test`. There is NO vitest/jest: use `import { test, mock } from "node:test";` and `import assert from "node:assert/strict";`. Match the imports and helper patterns of the existing test file you are extending.
  - Full run: `pnpm test:ts`
  - Targeted run (after `tsc -p tsconfig.test.json`): `node --test "extension/test-dist/analysis/insights.test.js"`
- **Script tests**: `node --test "scripts/**/*.test.mjs"` (`pnpm test:scripts`).
- **Daemon tests**: `cargo test -p import-lens-daemon --test <name>` (package name verified in `daemon/Cargo.toml`). Full: `pnpm test:rust` (= `cargo test --workspace`). Integration tests use the pinned fixtures under `daemon/tests/fixtures/` and shared helpers in `daemon/tests/common/mod.rs` - reuse those harnesses; do not invent new fixture mechanisms.
- Test snippets in this plan are exact in intent but MUST be adapted to the enclosing file's existing helper names; where a helper is presumed, the step says so.

**Verification command set** (narrowest relevant subset per task; full set before handoff):

```powershell
cargo fmt --check
cargo test -p import-lens-daemon
pnpm check
pnpm test
pnpm package:win32-x64
```

---

## File Structure (what each task touches)

**Part A - Correctness (Tasks 1-14)**
- `daemon/src/pipeline/bundle.rs` - default-export rewrite classifier (1)
- `daemon/src/document/script_regions.rs` - Astro frontmatter bounds (2); `</script >` end tag + `lang` attribute parse (9)
- `daemon/src/service.rs` + `daemon/src/report/executor.rs` - report panic isolation (3)
- `daemon/src/ipc/server.rs` - malformed-frame tolerance (4)
- `extension/src/daemon/nativeTransport.ts` - restart disposal latch, timer hygiene, restart-timer rejection guard (5)
- `extension/src/analysis/insights.ts` - insight idempotency + stale clearing (6)
- `extension/src/extension.ts` - daemon-recovery document refresh (7)
- `daemon/src/registry/client.rs` + `daemon/src/registry/service.rs` - `latest_published_at` (8)
- `daemon/src/document/completion.rs` - empty-brace completions (10)
- `extension/src/analysis/gitDiff.ts` + `extension/src/listener.ts` - buffer-accurate working-tree deltas (11)
- `extension/src/analysis/history.ts` - lost-update + identity dedup (12)
- `extension/src/ui/inlineHintDecorationLayerBuilder.ts` - suffix overflow (13)
- `extension/src/guidance/packageJsonAnalysis.ts` - failure-path freshness guards (14)

**Part B - Performance / robustness (Tasks 15-21)**
- `daemon/src/pipeline/graph.rs` - quadratic binding-dependency scan (15)
- `daemon/src/cache/project.rs` - lock scope (16); invalidation batching (17)
- `daemon/src/cache/memory.rs` - hit-path clone reduction (18)
- `daemon/src/cache/disk.rs` - recents monotonicity (19)
- `extension/src/guidance/registryRefresh.ts` - per-target refresh generations (20)
- `daemon/src/service.rs` - per-report `.importlensignore` memo (21, promoted from DF-8 backlog)

**Part C - Dead code (Tasks 22-26)**
- `extension/src/ui/inlineHintDecorations.ts` + `extension/src/ui/packageJsonDecorationSegments.ts` (22)
- `extension/src/ui/format.ts` (23), `extension/src/ui/compareImportItems.ts` (24)
- `daemon/src/prefetch.rs` (25)
- Batch surface: `extension/src/analysis/batchPartial.ts`, `extension/src/analysis/status.ts`, `sendBatch` chain, `BatchRequest`/`BatchResponse` TS types (26)

**Part D - DRY (Tasks 27-33)**
- Compression selector (27), decoration-controller base (28), `SourceRange`->`Range` (29), `server.rs` plumbing (30), debounced scheduler (31), pipeline helpers (32), `client.ts` timer-init duplication (33)

**Part E - Scripts / CLI (Tasks 34-40)**
- `cli/importlens.mjs` (34), `scripts/update-oxc-stack.mjs` (35), `scripts/daemon-hashes.mjs` (36), `scripts/assert-vsix-size.mjs` (37), `scripts/accuracy-compare.mjs` (38), `scripts/oxc-stack-helpers.mjs` (39), `scripts/package-vsix-manifest.mjs` (40)

**Final:** Task 41 - rebuild daemon + refresh hashes + full verification.

**Part F (appendix):** items deliberately NOT fixed, with reasons - so future reviews do not re-litigate them.

---

# PART A - Correctness Bugs

### Task 1: Fix `export default` rewrite of anonymous `class extends` / generator declarations

**Files:**
- Modify: `daemon/src/pipeline/bundle.rs:484-494` (`is_named_default_declaration`)
- Test: `daemon/tests/bundle.rs`

**Verified:** Read `bundle.rs:436-494`. `is_named_default_declaration` returns "named" whenever `class` is not immediately followed by `{` and `function` is not immediately followed by `(`. Hand-traced failures:
- `export default class extends Base {}` -> strip `"class"` -> `" extends Base {}"` -> not `{` -> classified NAMED -> only `export default ` removed -> emits `class extends Base {}` (nameless class declaration, syntax error).
- `export default function* () {}` -> strip `"function"` -> `"* () {}"` -> not `(` -> NAMED -> emits `function* () {}` (syntax error).
- `export default async function () {}` -> neither prefix matches (leading `async`) -> treated as expression -> wrapped; but `export default async function foo() {}` is ALSO treated as expression, hiding the named binding `foo` from module scope.
The corrupted statement is concatenated into the assembled bundle, the whole-bundle parse in `minify_source_with_markers` fails, and `analyze_with_oxc_pipeline` silently degrades the entire package to the low-confidence static-entry fallback.
Also verified: the current classifier shares a word-boundary hole (`export default functionFoo` -> strip `"function"` -> `"Foo"` -> NAMED -> emits bare `functionFoo;`, losing the default binding). The rewrite below fixes that too.

- [ ] **Step 1: Write the failing tests** - add to `daemon/tests/bundle.rs`, reusing that file's existing bundle-fixture helpers (read its header first; if no single-module helper exists, add a thin wrapper over the existing graph+`bundle_reachable_modules_with_metadata`+`minify_source_with_markers` path used by neighboring tests):

```rust
#[test]
fn rewrites_anonymous_default_class_with_extends_into_valid_binding() {
    let entry = "export class Base { value() { return 1; } }\n\
                 export default class extends Base { extra() { return 2; } }\n";
    let bundled = bundle_default_import_of_source(entry);
    assert!(
        !bundled.contains("class extends Base"),
        "anonymous default class must be wrapped in a named binding, got:\n{bundled}"
    );
    assert!(minify_ok(&bundled), "rewritten default-class bundle must parse and minify");
}

#[test]
fn rewrites_anonymous_default_generator_into_valid_binding() {
    let entry = "export default function* () { yield 1; }\n";
    let bundled = bundle_default_import_of_source(entry);
    assert!(minify_ok(&bundled), "anonymous default generator must parse and minify");
}

#[test]
fn keeps_named_async_default_function_as_declaration() {
    let entry = "export default async function loadThing() { return 1; }\n";
    let bundled = bundle_default_import_of_source(entry);
    assert!(bundled.contains("async function"), "named async default fn must stay a declaration");
    assert!(minify_ok(&bundled));
}
```

- [ ] **Step 2: Run tests to verify they fail** - `cargo test -p import-lens-daemon --test bundle rewrites_anonymous_default` -> Expected: FAIL (minify error / `class extends Base` present).

- [ ] **Step 3: Write minimal implementation** - replace `is_named_default_declaration` with a keyword-boundary-correct classifier (`is_identifier_start` / `is_identifier_continue` already exist in `bundle.rs`):

```rust
/// Strip `keyword` only when it is a whole word (not a prefix of a longer
/// identifier such as `functionFoo` or `classNames`).
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(keyword)?;
    match rest.chars().next() {
        Some(next) if is_identifier_continue(next) => None,
        _ => Some(rest),
    }
}

/// An `export default` declaration is *named* only when a binding identifier
/// follows the (optionally `async`-prefixed, optionally `*`-suffixed) keyword.
/// Anonymous forms (`class {}`, `class extends X {}`, `function () {}`,
/// `function* () {}`, `async function () {}`) and plain expressions must be
/// wrapped as `const <default binding> = <expr>` instead of being left as a
/// nameless declaration statement.
fn is_named_default_declaration(trimmed_after_default: &str) -> bool {
    let rest = strip_keyword(trimmed_after_default, "async")
        .map(str::trim_start)
        .unwrap_or(trimmed_after_default);

    if let Some(after_function) = strip_keyword(rest, "function") {
        let after_star = after_function
            .trim_start()
            .strip_prefix('*')
            .unwrap_or(after_function);
        return after_star.trim_start().starts_with(is_identifier_start);
    }

    if let Some(after_class) = strip_keyword(rest, "class") {
        let next = after_class.trim_start();
        // `class Foo ...` is named; `class {` and `class extends ...` are
        // anonymous. `extends` must be a whole keyword: `class extendsFoo {}`
        // names the class `extendsFoo`.
        return next.starts_with(is_identifier_start) && strip_keyword(next, "extends").is_none();
    }

    false
}
```

- [ ] **Step 4: Run tests to verify they pass** - `cargo test -p import-lens-daemon --test bundle` -> Expected: PASS (all bundle tests).

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/bundle.rs daemon/tests/bundle.rs
git commit -m "fix(bundle): wrap anonymous default class/generator exports

is_named_default_declaration only recognized anonymity when a '(' followed
'function' or a '{' followed 'class'. Anonymous forms such as
'export default class extends Base {}' and 'export default function* () {}'
were classified as named declarations, so the rewriter stripped only
'export default ' and left a nameless declaration behind. That statement
fails to parse, which poisoned the whole assembled bundle and silently
degraded the import to the low-confidence static-entry estimate.

Classify a default export as named only when a binding identifier follows
the (optionally async-prefixed, optionally *-suffixed) keyword, with proper
word boundaries so identifiers like 'functionFoo' are treated as
expressions. Anonymous declarations are now wrapped as 'const <binding> ='
and named async default functions keep their declaration form."
```

---

### Task 2: Fix empty Astro frontmatter slice panic

**Files:**
- Modify: `daemon/src/document/script_regions.rs:199-207` (`astro_frontmatter` closing-delimiter branch)
- Test: `daemon/tests/document_analysis.rs`

**Verified:** Read `script_regions.rs:107-248`. Hand-traced `astro_frontmatter("---\n---\n...")`: `content_start = 4`; the closing `---` is found on the immediately following line, so `content_end = previous_line_end(source, 4) = 3`; the returned `Frontmatter { source_start: 4, source_end: 3 }` makes `&source[frontmatter.source_start..frontmatter.source_end]` at line 113 panic (`slice index starts at 4 but ends at 3`). Empty frontmatter is valid, common Astro. The panic escapes `analyze_imports` (run inside `spawn_blocking`), so the document returns a protocol error instead of "no imports".

- [ ] **Step 1: Write the failing test** - add to `daemon/tests/document_analysis.rs`, using the same request-builder helper as `analyze_imports_supports_component_and_astro_regions`:

```rust
#[test]
fn analyze_imports_handles_empty_astro_frontmatter_without_panicking() {
    let response = analyze_astro_source("---\n---\n<h1>Hi</h1>\n");
    assert!(response.error.is_none());
    assert!(response.imports.is_empty());

    let crlf = analyze_astro_source("---\r\n---\r\n<h1>Hi</h1>\r\n");
    assert!(crlf.error.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails** - `cargo test -p import-lens-daemon --test document_analysis analyze_imports_handles_empty_astro` -> Expected: FAIL (slice panic surfaces as a protocol error / test panic).

- [ ] **Step 3: Write minimal implementation** - clamp the end to never precede the start:

```rust
        if source[line_start..line_end].trim_end_matches('\r') == "---" {
            let content_end = previous_line_end(source, line_start).max(content_start);
            return Some(Frontmatter {
                source_start: content_start,
                source_end: content_end,
            });
        }
```

(Traced: `---\n---` -> `[4..4]` empty region; `---\r\n---\r\n` -> `[5..5]`; non-empty frontmatter unchanged, e.g. `---\nimport x\n---` still yields `[4..12]`.)

- [ ] **Step 4: Run test to verify it passes** - `cargo test -p import-lens-daemon --test document_analysis` -> Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/document/script_regions.rs daemon/tests/document_analysis.rs
git commit -m "fix(document): guard empty Astro frontmatter slice bounds

An Astro file with empty frontmatter ('---' immediately followed by '---')
computed source_end < source_start, so slicing the frontmatter region
panicked and the daemon returned a protocol error for the whole document.
Clamp the frontmatter end to at least the content start so empty
frontmatter yields an empty script region, for both LF and CRLF files."
```

---

### Task 3: Isolate per-file panics in the workspace report

**Files:**
- Modify: `daemon/src/service.rs:236-267` (the report `par_iter().flat_map(...)`)
- Modify: `daemon/src/report/executor.rs:10-17` (`ThreadPoolBuilder`)
- Test: `daemon/tests/report.rs`

**Verified:** Read `service.rs:232-290` and `report/executor.rs` in full. The per-file `self.handle_analyze_document(document_request)` at `:251` runs inside a rayon `par_iter().flat_map` with NO `catch_unwind`; `spawn_workspace_report` (`:281-290`) fire-and-forgets the job on a `rayon::ThreadPool` built with only `num_threads` + `thread_name` (no `panic_handler`). Contrast: the registry worker wraps each unit in `catch_unwind` (`ipc/server.rs:497-510`). A single panicking file (e.g. Task 2's Astro case) kills or aborts the whole report.

- [ ] **Step 1: Write the failing test** - add to `daemon/tests/report.rs`, reusing its existing workspace-builder harness. Drive the panic through real malformed input (an empty-frontmatter `.astro` file) if Task 2 has not landed yet; if Task 2 landed first, use another real panic input or reorder so this test is authored against the pre-Task-2 tree. If neither is practical, the fallback seam is a workspace file crafted to hit any remaining panic path - do NOT add a production test-only hook. Assert on the actual `WorkspaceReportResponse` shape (rows + error; there is no `healthy` field - use the model's real fields):

```rust
#[test]
fn workspace_report_survives_a_single_panicking_file() {
    // workspace: one healthy .ts file importing a fixture package
    // + one file that makes handle_analyze_document panic.
    let workspace = report_workspace_with_panicking_file();
    let response = build_report_for(&workspace);
    assert!(response.error.is_none(), "one bad file must not fail the whole report");
    assert!(!response.rows.is_empty(), "healthy files must still be reported");
}
```

- [ ] **Step 2: Run test to verify it fails** - `cargo test -p import-lens-daemon --test report workspace_report_survives` -> Expected: FAIL (panic escapes; response missing or errored).

- [ ] **Step 3: Write minimal implementation**
  1. `report/executor.rs`: add `.panic_handler(|_| { crate::logging::log_warn("report", "workspace report worker panicked".to_owned()); })` to the builder so a panicking job can never abort the process.
  2. `service.rs`: wrap the per-file analysis in `catch_unwind`, mirroring the registry pattern:

```rust
                let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.handle_analyze_document(document_request)
                }));
                let response = match response {
                    Ok(response) => response,
                    Err(_) => {
                        crate::logging::log_warn(
                            "report",
                            format!("analysis panicked for {}", source_path.display()),
                        );
                        return Vec::new();
                    }
                };
```

- [ ] **Step 4: Run test to verify it passes** - `cargo test -p import-lens-daemon --test report` -> Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/service.rs daemon/src/report/executor.rs daemon/tests/report.rs
git commit -m "fix(report): isolate per-file panics during workspace scan

A panic while analyzing one workspace file escaped the report worker's
fire-and-forget rayon job (no panic_handler on the pool, no catch_unwind
around per-file analysis), killing or aborting the entire report. Wrap each
per-file analysis in catch_unwind and give the report pool a panic_handler,
mirroring the registry worker, so a single bad file degrades to a logged
skip instead of failing the whole report."
```

---

### Task 4: Tolerate malformed/unknown IPC messages without dropping the connection

**Files:**
- Modify: `daemon/src/ipc/server.rs:177`
- Test: `daemon/tests/server.rs`

**Verified:** Read `server.rs:168-189`. `let message = decode_payload::<ClientMessage>(&payload)?;` propagates any decode failure out of the connection loop, closing the socket. A version-skewed client (message type this daemon predates) or one corrupt-but-well-framed payload discards warm in-memory cache and every in-flight response.

- [ ] **Step 1: Write the failing test** - add to `daemon/tests/server.rs`, reusing its existing connect/send/request harness (875 lines of established helpers - adapt names to what exists):

```rust
#[tokio::test]
async fn server_ignores_an_undecodable_frame_and_keeps_serving() {
    // 1. connect and complete a normal hello handshake (existing helper)
    // 2. send one well-framed payload that fails ClientMessage decode,
    //    e.g. rmp-serde encoding of {"type":"message_from_the_future"}
    // 3. send a valid request (e.g. cache_status) on the same connection
    // 4. assert the response for step 3 arrives (connection survived)
}
```

- [ ] **Step 2: Run test to verify it fails** - `cargo test -p import-lens-daemon --test server server_ignores_an_undecodable_frame` -> Expected: FAIL (connection closed after the bad frame; no response).

- [ ] **Step 3: Write minimal implementation**

```rust
        let message = match decode_payload::<ClientMessage>(&payload) {
            Ok(message) => message,
            Err(error) => {
                logging::log_warn("ipc", format!("ignoring undecodable client frame: {error}"));
                continue;
            }
        };
```

(No error frame is sent back: an undecodable payload has no recoverable `request_id` to correlate a response to. Oversized-frame io errors from the length-delimited codec still tear down the connection - that is framing-integrity loss, not message-level noise, and stays fatal.)

- [ ] **Step 4: Run test to verify it passes** - `cargo test -p import-lens-daemon --test server` -> Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/ipc/server.rs daemon/tests/server.rs
git commit -m "fix(ipc): keep the connection alive on an undecodable frame

A single frame that failed ClientMessage decode (an unknown message type
from a version-skewed client, or a corrupt payload) propagated out of the
connection loop and closed the socket, dropping the warm cache and every
in-flight response. Log and skip the offending frame instead; framing-level
errors (oversized frames, io failures) remain fatal."
```

---

### Task 5: Fix daemon permanently disabled after `restart()` (+ restart-lifecycle timer hygiene)

**Files:**
- Modify: `extension/src/daemon/nativeTransport.ts` - `start()` (`:94-95`), `#cleanup()` (`:286-300`), `#scheduleRestart()` (`:277-284`)
- Test: Create `extension/test/daemon/nativeTransport.test.ts`

**Verified:** Grep of `#isDisposed` in `nativeTransport.ts`: initialized `false` (`:65`), guard `if (this.#isDisposed) return "unavailable"` at `:95`, set `true` in `shutdown()` (`:515`) - and NEVER reset anywhere. Read `manager.ts:137-143` (`restart()` = `dispose().then(() => start())`), `transport.ts:69-133` (coordinator holds one fixed `NativeDaemonTransport` instance and `#startTransports` reuses it), and `configChange.ts:8-18` (`enableDiskCache` / `cacheMaxSizeMB` / `cacheMaxAgeDays` classify as `"daemonRestart"`) -> `extension.ts:282-283` calls `restartDaemonAndRefresh()`. Net: changing any of those three settings permanently disables the daemon until window reload. The crash-recovery path (`#scheduleRestart` -> `void this.start()`) never calls `shutdown()`, which is why this went unnoticed.
Also verified in the same read:
- `#cleanup()` (`:286-300`) clears only the disconnect timer; `#stabilityTimer` / `#cleanRecycleTimer` survive a crash and, firing later, reset `#restartAttempt`/`#crashTimes` - weakening the 3-crashes-in-60s breaker and the backoff exactly when they matter.
- `#scheduleRestart` (`:282`) invokes `void this.start(...)` with no rejection handling; `start()` contains unguarded `await mkdir(...)` calls (`:141-142`), so a storage failure on the timer path becomes an unhandled promise rejection with no state transition.

- [ ] **Step 1: Write the failing test** - new `extension/test/daemon/nativeTransport.test.ts` (node:test; follow the vscode-stubbing pattern used by existing `extension/test/daemon/*.test.ts` files). Observable seam (no production hooks): a capturing fake logger. With a fake `ExtensionContext` rooted in a temp dir and no daemon binary present, `start(root)` runs to binary verification and logs `Starting ImportLens daemon for workspace ...` before returning `"unavailable"`; a disposal-latched `start()` returns `"unavailable"` with NO log output. So:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
// + the existing fake-context/fake-logger helpers used by daemon tests

test("start() after shutdown() re-attempts startup instead of latching disposed", async () => {
  const logs: string[] = [];
  const transport = new NativeDaemonTransport(fakeContext(tempDir()), capturingLogger(logs));

  await transport.start(tempWorkspaceDir());   // runs, logs "Starting ImportLens daemon..."
  await transport.shutdown();
  logs.length = 0;

  const state = await transport.start(tempWorkspaceDir());
  assert.equal(state, "unavailable");          // no binary in the fake context - expected
  assert.ok(
    logs.some((line) => line.includes("Starting ImportLens daemon")),
    "start() after shutdown() must re-attempt startup, not bail at the disposal latch",
  );
});
```

- [ ] **Step 2: Run test to verify it fails** - `tsc -p tsconfig.test.json && node --test "extension/test-dist/daemon/nativeTransport.test.js"` -> Expected: FAIL (no startup log after shutdown).

- [ ] **Step 3: Write minimal implementation**
  1. `start()` - an explicit start is intent to run; reset the latch first:

```ts
  async start(analysisRoot?: string): Promise<DaemonState> {
    this.#isDisposed = false;
    if (this.#state === "ready" && this.#process && this.#client) return "ready";
```

  Rationale: the auto-restart timer still checks `#isDisposed` before calling `start()`, so disposal still prevents self-resurrection; only an explicit caller-initiated `start()` revives the transport. (Alternative considered: a separate non-terminal `stop()` threaded through coordinator + manager; rejected as a 3-layer interface change for the same observable behavior. Revive-after-deactivate is not reachable: VS Code disposes the subscriptions that could call `start()`.)
  2. `#cleanup()` - clear the session-scoped timers so a crashed session cannot later earn a "stable session" reset:

```ts
    if (this.#stabilityTimer) {
      clearTimeout(this.#stabilityTimer);
      this.#stabilityTimer = null;
    }
    if (this.#cleanRecycleTimer) {
      clearTimeout(this.#cleanRecycleTimer);
      this.#cleanRecycleTimer = null;
    }
```

  3. `#scheduleRestart()` - guard the fire-and-forget start:

```ts
      if (!this.#isDisposed) {
        void this.start(this.#lastAnalysisRoot).catch((error: unknown) => {
          this.#logger.warn(`Scheduled daemon restart failed: ${error instanceof Error ? error.message : String(error)}`);
          this.#setState("unavailable");
        });
      }
```

- [ ] **Step 4: Run tests to verify they pass** - `pnpm test:ts` (the new file + `transport.test.ts` + `restartPolicy.test.ts` must all stay green).

- [ ] **Step 5: Commit**

```bash
git add extension/src/daemon/nativeTransport.ts extension/test/daemon/nativeTransport.test.ts
git commit -m "fix(daemon): re-arm transport on explicit restart

shutdown() latched #isDisposed and nothing ever reset it, so
DaemonManager.restart() - triggered by changing enableDiskCache,
cacheMaxSizeMB, or cacheMaxAgeDays - gracefully shut the daemon down and
then bailed at the disposal guard on the follow-up start(), leaving
ImportLens unavailable until the window was reloaded. Reset the latch at
the top of start() so an explicit start revives the transport while the
auto-restart timer still honors disposal.

Also harden the restart lifecycle: clear the stability and clean-recycle
timers during cleanup so a timer armed by a previous session cannot reset
the crash breaker and backoff after a crash, and surface scheduled-restart
failures as an unavailable state instead of an unhandled rejection."
```

---

### Task 6: Stop insights duplicating on re-apply, and clear stale ones

**Files:**
- Modify: `extension/src/analysis/insights.ts:38-46`
- Test: `extension/test/analysis/insights.test.ts`

**Verified:** Read `insights.ts` in full and `extension.ts:136-155`. `applyImportAnalysisInsights` appends (`insights: [...(state.insights ?? []), ...insights]`), and `reapplyInsightsForVisibleDocuments` feeds it the STORED states (already insight-bearing) on every uiOnly config change -> duplicate tags accumulate (`barrel barrel ...`). Validation also caught a defect in the originally planned fix: the `insights.length === 0` early-return keeps the OLD insights, so a stale `over budget` label would survive a budget raise. Both branches must be fixed. All insight inputs (changedLines, budgets, history, shared modules, barrel syntax) derive from the current state + options - never from prior insights - so full recomputation is correct.

- [ ] **Step 1: Write the failing tests** - add to `extension/test/analysis/insights.test.ts` (node:test; reuse its existing state-builder helpers):

```ts
test("re-applying insights replaces rather than accumulates", () => {
  const base = readyStarReexportState(); // yields the "barrel" insight
  const once = applyImportAnalysisInsights([base], { importCostHistory: [] });
  const twice = applyImportAnalysisInsights(once, { importCostHistory: [] });
  const barrels = (twice[0].insights ?? []).filter((insight) => insight.label === "barrel");
  assert.equal(barrels.length, 1);
});

test("re-applying insights clears entries whose inputs no longer produce them", () => {
  const base = readyState({ brotliBytes: 50_000 });
  const over = applyImportAnalysisInsights([base], {
    importCostHistory: [],
    budgets: { perImportBrotliBytes: 10_000 },
  });
  assert.ok((over[0].insights ?? []).some((insight) => insight.label === "over budget"));

  const relaxed = applyImportAnalysisInsights(over, { importCostHistory: [] }); // budget removed
  assert.equal((relaxed[0].insights ?? []).length, 0);
});
```

- [ ] **Step 2: Run tests to verify they fail** - `tsc -p tsconfig.test.json && node --test "extension/test-dist/analysis/insights.test.js"` -> Expected: FAIL (2 barrels; stale over-budget survives).

- [ ] **Step 3: Write minimal implementation** - recompute and replace in BOTH branches:

```ts
    if (insights.length === 0) {
      if (!state.insights || state.insights.length === 0) {
        return state;
      }
      return { ...state, insights: undefined };
    }

    return { ...state, insights };
```

- [ ] **Step 4: Run tests to verify they pass** - `node --test "extension/test-dist/analysis/insights.test.js"` -> Expected: PASS (plus the rest of `pnpm test:ts`).

- [ ] **Step 5: Commit**

```bash
git add extension/src/analysis/insights.ts extension/test/analysis/insights.test.ts
git commit -m "fix(insights): replace insights on re-apply instead of appending

applyImportAnalysisInsights appended to any insights already on the state,
so re-applying on each UI-only configuration change accumulated duplicate
tags ('barrel' twice, then three times), and the zero-insight early return
kept stale entries alive (an 'over budget' label survived raising the
budget). Insights are derived entirely from the current state and options,
so recompute them and replace: set the fresh list when non-empty and clear
any previous list when the recomputed set is empty."
```

---

### Task 7: Reanalyze open documents when the daemon auto-recovers

**Files:**
- Modify: `extension/src/extension.ts:298-312` (the `daemon.onDidChangeState` handler)
- Test: `extension/test/configRefresh.test.ts` or a focused new test (see step 1)

**Verified:** Read `extension.ts:276-317`. The `ready` handler refreshes package.json analysis/decorations and replays prewarm but never re-schedules `DocumentAnalysisController`, while the explicit-restart path calls `refreshVisibleDocuments(nextConfig, "reanalyze")`. On daemon crash the in-flight analyze clears document states (`listener.ts:112-116` clears on null response), so inline sizes stay blank after auto-recovery until the user edits.

- [ ] **Step 1: Write the failing test** - the handler body is a closure in `activate()`, so test at the unit that IS exported: if `configRefresh.ts` exposes the refresh helper, assert wiring via a small extracted function. Concretely: extract the ready-transition behavior into an exported `onDaemonStateChanged(...)` helper (same file or `configRefresh.ts`) taking `{ statusBar, prewarm, packageJsonAnalysis, packageJsonDecorations, refreshVisibleDocuments }`, and unit-test THAT with node:test fakes: a `"ready"` transition must call `refreshVisibleDocuments` with mode `"reanalyze"`, a non-ready transition must not.

- [ ] **Step 2: Run test to verify it fails** - `tsc -p tsconfig.test.json && node --test "extension/test-dist/<chosen>.test.js"` -> Expected: FAIL (`refreshVisibleDocuments` not called).

- [ ] **Step 3: Write minimal implementation** - in the ready branch, after the package.json refreshes:

```ts
    packageJsonAnalysis.refreshVisibleDocuments();
    packageJsonDecorations.refreshVisibleEditors();
    refreshVisibleDocuments(getImportLensConfig(), "reanalyze");
```

- [ ] **Step 4: Run tests to verify they pass** - `pnpm test:ts` -> Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add extension/src/extension.ts extension/test/
git commit -m "fix(analysis): reanalyze documents after daemon auto-recovery

When the daemon crashed, in-flight analyses cleared document states, and
the automatic restart's ready transition refreshed only package.json hints
and prewarm - open source files kept blank inline sizes until the next
edit. Reanalyze visible documents on the ready transition, matching what
the explicit restart command already does."
```

---

### Task 8: Populate or retire `latest_published_at`

**Files:**
- Modify: `daemon/src/registry/client.rs:32-35` (Accept header) and/or `daemon/src/registry/service.rs:482-486` (parser); possibly `daemon/src/registry/types.rs` + `extension/src/ipc/protocol.ts` + SRS section 10 (if retired)
- Test: `daemon/tests/registry.rs`

**Verified:** Read `client.rs:26-45` and `service.rs:474-493`. The client requests abbreviated ("corgi") metadata via `accept: application/vnd.npm.install-v1+json, application/json`; the abbreviated packument omits the top-level `time` object; the parser reads `document.get("time")` -> `latest_published_at` is always `None` in production. Existing tests mask this with full-format fixtures.

**Execution decision (resolve in step 0):** grep `latestPublishedAt` / `latest_published_at` across `extension/src`. If the extension never renders it, REMOVE the field end-to-end (daemon model, protocol, TS type, SRS section 10) - YAGNI beats carrying a dead field. If it IS rendered, populate it correctly: drop the corgi Accept type (request the full packument) OR keep corgi and take the abbreviated document's `modified` timestamp as an explicitly renamed `last_modified_at` (do not mislabel it). Whichever path: update the SRS in the same commit.

- [ ] **Step 1: Write the failing test** - add a `daemon/tests/registry.rs` case whose fake HTTP body is a genuine ABBREVIATED response (`dist-tags` + `versions` + `modified`, NO `time`), asserting the decided behavior.
- [ ] **Step 2: Run test to verify it fails** - `cargo test -p import-lens-daemon --test registry` -> Expected: FAIL.
- [ ] **Step 3: Implement the decided fix.**
- [ ] **Step 4: Run test to verify it passes** -> Expected: PASS.
- [ ] **Step 5: Commit** - e.g. (removal path):

```bash
git commit -m "fix(registry): stop advertising a published-at hint that never populated

The registry client requests abbreviated npm metadata
(application/vnd.npm.install-v1+json), which does not include the packument
'time' object, but the parser sourced latest_published_at from 'time' - so
the field was always null in production and only test fixtures (full-format
bodies) ever populated it. Remove the dead field from the daemon model, the
IPC protocol, and the SRS <OR: source it from the abbreviated document /
full packument>, and pin the behavior with a genuinely abbreviated fixture."
```

---

### Task 9: Tolerate `</script >` end-tag whitespace and parse `lang` as a real attribute

**Files:**
- Modify: `daemon/src/document/script_regions.rs:164` (end-tag scan) and `:66-89` (`language_from_attributes`)
- Test: `daemon/tests/document_analysis.rs`

**Verified:** Read `script_regions.rs:146-182` and `:66-89`.
- End tag: `lower_source[content_start..].find("</script>")` is a fixed literal; a legal `</script >` is not found, the loop `break`s, and that block plus ALL subsequent script blocks are dropped -> every import in the component silently undetected.
- `lang`: `find("lang")` is a substring search. Hand-traced `<script data-slang="x" lang="ts">`: the first "lang" hit is inside `data-slang`, the parser consumes its `="x"` value, returns `Js`, and the real `lang="ts"` is never examined - the block parses as JS and TS-only syntax fails.

- [ ] **Step 1: Write the failing tests** - Svelte/Vue source whose script closes with `</script >` must still yield its imports; `<script data-slang="x" lang="ts">` must parse as TS (use a TS-only construct plus an import to make the language observable through the public analyze API).
- [ ] **Step 2: Run tests to verify they fail** - `cargo test -p import-lens-daemon --test document_analysis` -> Expected: FAIL.
- [ ] **Step 3: Implement** - end tag: search for `</script` then scan optional ASCII whitespace up to the next `>`, symmetric with the opening-tag handling; `lang`: iterate attribute tokens (name, optional `=value`) instead of substring `find`, or require the char before "lang" to be a whitespace/start boundary and the parse to complete - implement the token walk; it is a handful of lines and removes the class of bug.
- [ ] **Step 4: Run tests to verify they pass** -> Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(document): parse script end tags and lang attributes structurally

The script-region scanner matched the literal '</script>' only, so a legal
end tag with internal whitespace ('</script >') dropped that block and
every later script block - all imports in the component vanished. The
language sniffing used a substring find('lang'), so an earlier attribute
containing 'lang' (e.g. data-slang=\"x\") consumed the match and forced JS
mode for a lang=\"ts\" block. Scan the end tag allowing whitespace before
'>' and walk attributes as tokens when resolving the language."
```

---

### Task 10: Serve completions for empty-brace imports

**Files:**
- Modify: `daemon/src/document/completion.rs:65-109` (`completion_context_from_module_record`)
- Test: inline `#[cfg(test)]` in `completion.rs` (matches the existing inline test pattern) or `daemon/tests/document_analysis.rs`

**Verified:** Read `completion.rs:60-114`. Candidate groups are built exclusively from `module_record.import_entries`; `import {} from "lodash"` produces zero entries, so no group exists, the function returns `None`, and the daemon returns an empty `CompleteImportMembersResponse` - exactly when the user opens the braces wanting the export list. `requested_modules` on the module record still carries the specifier and statement span.

- [ ] **Step 1: Write the failing test** - completion context requested with the cursor inside `import {  } from "lodash"` must resolve `specifier == "lodash"` with empty `imported_names`.
- [ ] **Step 2: Run test to verify it fails** -> Expected: FAIL (`None`).
- [ ] **Step 3: Implement** - after the `import_entries` pass finds no group containing the offset, fall back to `module_record.requested_modules`: for each requested module whose statement span's brace range (`named_import_member_range`) contains the offset, return a context with that specifier and no imported names.
- [ ] **Step 4: Run test to verify it passes** -> Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(completion): resolve the specifier for empty-brace imports

Completion context was built only from module-record import entries, and
'import {} from \"pkg\"' has none - so member completion returned nothing
precisely when the user opened the braces to discover exports, and only
started working after the first identifier character. Fall back to the
module record's requested modules to recover the specifier for empty
named-import braces."
```

---

### Task 11: Make working-tree deltas buffer-accurate (replaces the diff-output parser)

**Files:**
- Modify: `extension/src/analysis/gitDiff.ts` (replace `changedLinesFromGitDiff` + `changedLinesForFile` internals; keep `isGitRepository`)
- Modify: `extension/src/listener.ts:85` (pass the buffer text)
- Test: `extension/test/analysis/gitDiff.test.ts` (retarget)

**Verified:** Read `gitDiff.ts` in full and `listener.ts:75-160`. Three defects share one root cause - the delta is computed from a `git diff` of the ON-DISK file while the daemon analyzes the BUFFER:
1. `analyze()` runs on every debounced keystroke, when the buffer is essentially always dirty; unsaved insertions/deletions above a saved-and-changed line shift the line mapping, so `+N br` badges attach to the wrong imports. (The originally proposed "suppress when dirty" fix would instead disable the README-promised while-you-type delta for non-autosave users - rejected during validation.)
2. Parser edge: an added line whose content starts at column 0 with `++` (e.g. `++i;`) appears in the diff as `+++i;` and is swallowed by the `line.startsWith("+++")` header check at `:26` - inside a hunk, where headers cannot occur in this single-file `--unified=0` invocation - desynchronizing every subsequent line in the hunk.
3. The exec wrapper (`changedLinesForFile`) had no test coverage at all.

**Fix - compare the buffer against HEAD content directly:** fetch the base with `git show HEAD:<repo-relative-path>`, then compute changed lines in-process with a Myers line diff. This removes the unified-diff parser (and its edge cases) entirely, keeps deltas live while typing, and turns the core into a pure, thoroughly testable function.

- [ ] **Step 1: Write the failing tests** - retarget `extension/test/analysis/gitDiff.test.ts` at the new pure function:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { changedLinesBetween } from "../../src/analysis/gitDiff.js";

test("pure insertion marks only the inserted lines", () => {
  const base = "a\nb\nc\n";
  const current = "a\nX\nY\nb\nc\n";
  assert.deepEqual([...changedLinesBetween(base, current)].sort(), [1, 2]);
});

test("replacement marks the replacing lines", () => {
  assert.deepEqual([...changedLinesBetween("a\nb\nc\n", "a\nB\nc\n")], [1]);
});

test("pure deletion marks nothing", () => {
  assert.equal(changedLinesBetween("a\nb\nc\n", "a\nc\n").size, 0);
});

test("two separated edits do not mark the unchanged lines between them", () => {
  const base = "a\nb\nc\nd\ne\n";
  const current = "A\nb\nc\nd\nE\n";
  assert.deepEqual([...changedLinesBetween(base, current)].sort(), [0, 4]);
});

test("content lines starting with ++ are handled like any other line", () => {
  const base = "let i = 0;\n";
  const current = "let i = 0;\n++i;\n";
  assert.deepEqual([...changedLinesBetween(base, current)], [1]);
});

test("CRLF base against LF buffer compares by line content", () => {
  assert.equal(changedLinesBetween("a\r\nb\r\n", "a\nb\n").size, 0);
});

test("identical inputs mark nothing", () => {
  assert.equal(changedLinesBetween("a\nb\n", "a\nb\n").size, 0);
});
```

- [ ] **Step 2: Run tests to verify they fail** - `tsc -p tsconfig.test.json && node --test "extension/test-dist/analysis/gitDiff.test.js"` -> Expected: FAIL (`changedLinesBetween` not exported).

- [ ] **Step 3: Write the implementation** - in `gitDiff.ts`:

```ts
// Greedy Myers diff over lines. Returns the 0-based line numbers in
// `current` that are inserted or replaced relative to `base`. Bounded: on
// inputs whose edit distance exceeds MAX_EDIT_DISTANCE the function returns
// an empty set (no badges) rather than burning CPU mid-keystroke.
const MAX_EDIT_DISTANCE = 2000;

export const changedLinesBetween = (base: string, current: string): Set<number> => {
  const changed = new Set<number>();
  if (base === current) {
    return changed;
  }

  const a = base.split(/\r?\n/u);
  const b = current.split(/\r?\n/u);

  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) {
    start += 1;
  }
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA -= 1;
    endB -= 1;
  }

  const n = endA - start;
  const m = endB - start;
  if (m === 0) {
    return changed; // pure deletion: no current line changed
  }
  if (n === 0) {
    for (let line = 0; line < m; line += 1) {
      changed.add(start + line);
    }
    return changed;
  }

  const maxD = Math.min(n + m, MAX_EDIT_DISTANCE);
  const offset = maxD;
  let v = new Int32Array(2 * maxD + 1);
  const trace: Int32Array[] = [];
  let foundD = -1;

  outer: for (let d = 0; d <= maxD; d += 1) {
    trace.push(v.slice());
    const next = v.slice();
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1])
        ? v[offset + k + 1]
        : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[start + x] === b[start + y]) {
        x += 1;
        y += 1;
      }
      next[offset + k] = x;
      if (x >= n && y >= m) {
        foundD = d;
        break outer;
      }
    }
    v = next;
  }

  if (foundD < 0) {
    return changed; // too different for a mid-keystroke diff; degrade to no badges
  }

  // Walk the trace back, marking every line inserted into `b`.
  let x = n;
  let y = m;
  for (let d = foundD; d > 0; d -= 1) {
    const previous = trace[d];
    const k = x - y;
    const cameFromDown = k === -d || (k !== d && previous[offset + k - 1] < previous[offset + k + 1]);
    const previousK = cameFromDown ? k + 1 : k - 1;
    const previousX = previous[offset + previousK];
    const previousY = previousX - previousK;

    while (x > previousX && y > previousY) {
      x -= 1;
      y -= 1; // snake: unchanged line
    }
    if (cameFromDown) {
      y -= 1;
      changed.add(start + y); // insertion into `b`
    } else {
      x -= 1; // deletion from `a`
    }
  }
  while (y > 0 && x > 0) {
    x -= 1;
    y -= 1;
  }
  while (y > 0) {
    y -= 1;
    changed.add(start + y);
  }

  return changed;
};

export const changedLinesForFile = async (fileName: string, currentText: string): Promise<Set<number>> => {
  if (!(await isGitRepository(fileName))) {
    return new Set();
  }

  try {
    const directory = path.dirname(fileName);
    const { stdout: topLevel } = await execFileAsync(
      "git",
      ["-C", directory, "rev-parse", "--show-toplevel"],
      { encoding: "utf8", timeout: 500 },
    );
    const relativePath = path
      .relative(topLevel.trim(), fileName)
      .split(path.sep)
      .join("/");
    const { stdout: baseText } = await execFileAsync(
      "git",
      ["-C", directory, "show", `HEAD:${relativePath}`],
      { encoding: "utf8", maxBuffer: 8 * 1024 * 1024, timeout: 1500 },
    );

    return changedLinesBetween(baseText, currentText);
  } catch {
    // Untracked file, no HEAD, or oversized/timed-out git call: no badges,
    // matching the previous behavior for files absent from the HEAD diff.
    return new Set();
  }
};
```

  Delete `changedLinesFromGitDiff` and `hunkPattern` (nothing else consumes them). In `listener.ts:85`, capture the buffer text with the call:

```ts
    const changedLinesPromise = changedLinesForFile(document.fileName, document.getText());
```

- [ ] **Step 4: Run tests to verify they pass** - `node --test "extension/test-dist/analysis/gitDiff.test.js"`, then full `pnpm test:ts` -> Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add extension/src/analysis/gitDiff.ts extension/src/listener.ts extension/test/analysis/gitDiff.test.ts
git commit -m "fix(insights): compute working-tree deltas against the live buffer

Working-tree '+N br' badges were derived from 'git diff' of the file on
disk while the daemon analyzed the in-memory buffer. During editing - when
the analysis actually runs - unsaved insertions or deletions shifted the
line mapping, attaching delta badges to the wrong imports and hiding them
on genuinely changed ones. The unified-diff parser also swallowed added
lines whose content begins with '++' (rendered as '+++' in the diff),
desynchronizing the rest of the hunk.

Fetch the HEAD blob with 'git show' and compute changed lines in-process
with a bounded Myers line diff of base vs buffer. This keeps deltas
accurate while typing, removes the diff-output parsing (and its header
ambiguity) entirely, and turns the core into a pure function with direct
tests. Untracked files and oversized diffs degrade to no badges, matching
the previous behavior."
```

---

### Task 12: Fix history lost-update race and duplicate-identity accumulation

**Files:**
- Modify: `extension/src/analysis/history.ts:110-126` (`recordImportCostHistory`)
- Test: `extension/test/analysis/history.test.ts`

**Verified:** Read `history.ts:95-133`. (a) `recordImportCostHistory` is a read-modify-write against `globalState` with no serialization; concurrent analyses (tab switch while a previous file's analyze is in flight - both invoked from `listener.ts:153`) read the same `existing` and the later `update` overwrites the earlier (lost update). (b) `[...changedItems, ...existing]` never removes prior entries with the same `identity`; a frequently-edited import accumulates rows and, under the 200 cap, evicts every other import's history (`previousImportCostFor` uses `find` = newest-first, so reads stay right while the cap fills with duplicates).

- [ ] **Step 1: Write the failing tests** (node:test, fake store with async `update`):

```ts
test("concurrent history writes both persist", async () => {
  const store = slowFakeStore(); // update() awaits a tick before committing
  await Promise.all([
    recordImportCostHistory(store, [item("react", 100)]),
    recordImportCostHistory(store, [item("lodash-es", 200)]),
  ]);
  const identities = store.get(importCostHistoryKey, []).map((entry) => entry.identity);
  assert.ok(identities.includes(identityOf("react")));
  assert.ok(identities.includes(identityOf("lodash-es")));
});

test("recording a changed cost keeps one row per identity", async () => {
  const store = fakeStore();
  await recordImportCostHistory(store, [item("react", 100)]);
  await recordImportCostHistory(store, [item("react", 150)]);
  const rows = store.get(importCostHistoryKey, []).filter((entry) => entry.identity === identityOf("react"));
  assert.equal(rows.length, 1);
  assert.equal(rows[0].brotliBytes, 150);
});
```

- [ ] **Step 2: Run tests to verify they fail** -> Expected: FAIL (lost update; 2 rows).

- [ ] **Step 3: Implement** - serialize through a module-level promise chain and dedupe by identity:

```ts
let historyWriteChain: Promise<void> = Promise.resolve();

export const recordImportCostHistory = (
  store: BundleImpactHistoryStore,
  items: readonly ImportCostHistoryItem[],
  limit = 200,
): Promise<void> => {
  const write = historyWriteChain.then(async () => {
    const existing = store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []);
    const changedItems = items.filter((item) => {
      const previous = existing.find((entry) => entry.identity === item.identity);
      return !previous || !sameImportCost(item, previous);
    });

    if (changedItems.length === 0) {
      return;
    }

    const changedIdentities = new Set(changedItems.map((item) => item.identity));
    const retained = existing.filter((entry) => !changedIdentities.has(entry.identity));
    await store.update(importCostHistoryKey, [...changedItems, ...retained].slice(0, Math.max(1, limit)));
  });
  historyWriteChain = write.catch(() => {});
  return write;
};
```

  Note: if `previousImportCostFor`'s trend semantics rely on the superseded row remaining (verify against `historyTrendInsight` usage first), keep ONE previous row per identity instead of zero - the test in step 1 then asserts `rows.length === 2` capped per identity. Decide from the actual insight math during execution; the lost-update fix is unconditional.

- [ ] **Step 4: Run tests to verify they pass** -> Expected: PASS (plus existing history tests).

- [ ] **Step 5: Commit**

```bash
git add extension/src/analysis/history.ts extension/test/analysis/history.test.ts
git commit -m "fix(history): serialize import-cost writes and dedupe by identity

recordImportCostHistory read-modify-wrote globalState with no
serialization, so analyses racing across tabs lost each other's entries,
and changed costs were prepended without removing the identity's prior
rows - one hot import could evict every other import's history from the
200-row cap. Chain writes through a single promise so updates are atomic
in order, and keep the newest row per identity."
```

---

### Task 13: Stop silently dropping inline-hint suffixes beyond the 4 slots

**Files:**
- Modify: `extension/src/ui/inlineHintDecorationLayerBuilder.ts:44-60`
- Test: `extension/test/ui/inlineHintSegments.test.ts` (or the builder's own test file if present)

**Verified:** Read `inlineHintDecorationLayerBuilder.ts` in full and `importHintParts.ts` in full. `INLINE_HINT_SUFFIX_SLOT_COUNT = 4`; `slotForSegmentIndex` returns `undefined` for suffix index >= 4 and the bucket builder `continue`s - silently discarding the segment. `importHintParts` emits up to 3 tag suffixes (`server`, `types only`, `CJS` - from `importHintTagLabels`) plus up to 3 labeled insight suffixes (`+N br`, `over budget`, `barrel`), so up to 6 suffixes can arrive: suffixes 5 and 6 vanish in the colored renderer while the native-inlay and CodeLens renderers (which map ALL segments) show them - inconsistent output across renderers.

- [ ] **Step 1: Write the failing test** - build 6 suffix segments and assert every suffix text survives bucketing (the overflow folded into the last slot):

```ts
test("suffixes beyond the slot count fold into the last slot", () => {
  const segments = segmentsFor({
    primary: "1.2 kB br",
    suffixes: ["server", "types only", "CJS", "+2 kB br", "over budget", "barrel"],
  });
  const buckets = inlineHintDecorationLayerBuckets(segments);
  const rendered = [
    ...buckets.suffix0, ...buckets.suffix1, ...buckets.suffix2, ...buckets.suffix3,
  ].map((segment) => segment.text).join(" ");
  for (const expected of ["server", "types only", "CJS", "+2 kB br", "over budget", "barrel"]) {
    assert.ok(rendered.includes(expected), `missing suffix: ${expected}`);
  }
});
```

- [ ] **Step 2: Run test to verify it fails** -> Expected: FAIL (`over budget`, `barrel` missing).

- [ ] **Step 3: Implement** - in `inlineHintDecorationLayerBuckets`, fold overflow into `suffix3` instead of dropping (merge the overflow segment's text into the final slot's last segment, preserving that segment's tone):

```ts
  for (const [index, segment] of segments.entries()) {
    const slot = slotForSegmentIndex(index);

    if (slot) {
      buckets[slot].push(segment);
      continue;
    }

    const lastSlot = buckets.suffix3;
    const last = lastSlot[lastSlot.length - 1];
    if (last) {
      lastSlot[lastSlot.length - 1] = { ...last, text: `${last.text} ${segment.text}` };
    } else {
      lastSlot.push(segment);
    }
  }
```

  (Adapt the property name to `InlineHintSegment`'s actual text field; check `inlineHintSegments.ts` - the plan uses `text` per `importHintParts`' suffix shape.)

- [ ] **Step 4: Run tests to verify they pass** -> Expected: PASS, including existing builder/segment tests.

- [ ] **Step 5: Commit**

```bash
git add extension/src/ui/inlineHintDecorationLayerBuilder.ts extension/test/ui/
git commit -m "fix(ui): fold overflow inline-hint suffixes into the last slot

The colored inline renderer has four fixed suffix decoration slots and
silently discarded any suffix past them, while the native inlay and
CodeLens renderers show every suffix - so an import carrying tags plus
delta, budget, and barrel insights rendered different information
depending on the renderer. Fold overflow suffixes into the final slot's
text so all renderers agree."
```

---

### Task 14: Guard package.json failure paths with the freshness check

**Files:**
- Modify: `extension/src/guidance/packageJsonAnalysis.ts:131-134` and `:149-155`
- Test: `extension/test/guidance/packageJsonPartial.test.ts` or a focused controller test

**Verified:** Read `packageJsonAnalysis.ts:100-156`. The success path guards with `if (!this.#freshness.isCurrent(key, response.request_id)) return;` (`:136`), but the `!response` branch (`markLoadingUnavailable`, `:131-134`) and the catch branch (`:149-155`) mutate state with no freshness check - a stale request resolving null/throwing can flip the CURRENT request's in-progress `loading` entries to `unavailable` (transient flicker; partials heal it, but the guard asymmetry is real).

- [ ] **Step 1: Write the failing test** - drive two overlapping analyses through the controller with a fake daemon: the older one resolves `null` AFTER the newer one has begun; assert the newer request's `loading` states are not flipped to `unavailable`.
- [ ] **Step 2: Run test to verify it fails** -> Expected: FAIL.
- [ ] **Step 3: Implement** - both failure paths first check `if (!this.#freshness.isCurrent(key, requestId)) return;` (the request's own id - captured at `:78`).
- [ ] **Step 4: Run tests to verify they pass** -> Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(guidance): ignore stale package.json failures

The package.json controller checked response freshness on the success path
but not on the null-response or thrown-error paths, so a superseded
request that failed late could mark the live request's loading rows
unavailable until its partials repaired them. Apply the same freshness
guard to both failure paths."
```

---

# PART B - Performance / Robustness

### Task 15: Make `binding_dependencies_from` sub-quadratic

**Files:**
- Modify: `daemon/src/pipeline/graph.rs:1147-1186`
- Test: `daemon/tests/graph.rs` (characterization)

**Verified:** Read `graph.rs:1130-1200`. Nested loop: for every top-level statement range, every root-scope reference in the module is scanned (`O(S x R)`); both grow with module size, and a single module may be up to 20 MiB (`MAX_MODULE_SOURCE_BYTES`), so large pre-bundled entries pay a many-second cliff on first analysis. `statement_binding_ranges` is built from `program.body` in order - ranges are non-overlapping and sorted.

- [ ] **Step 1: Write the characterization test** - assert the exact `binding_dependencies` output for a module mixing several declarations, cross-references, and dedup cases. Run it: it must PASS against the current code (this is a guard enabling a pure perf refactor of already-covered logic - the one deliberate deviation from failing-first in this plan).
- [ ] **Step 2: Implement** - sort `references` by span start once; for each statement range, binary-search the first reference at/after `range.start` and walk forward while `end <= range.end` -> `O((S + R) log R)`. Keep the output sort + dedup identical.
- [ ] **Step 3: Run tests** - `cargo test -p import-lens-daemon --test graph` -> Expected: PASS with identical output.
- [ ] **Step 4: Commit**

```bash
git commit -m "perf(graph): index references by span in binding-dependency extraction

binding_dependencies_from rescanned every root-scope reference for every
top-level statement range - O(statements x references), both proportional
to module size, which stalls first-time analysis of large pre-bundled
entries (modules up to the 20 MiB limit). Sort references once and walk
each statement's window via binary search, preserving identical output."
```

---

### Task 16: Open cache shards outside the global `loaded` lock

**Files:**
- Modify: `daemon/src/cache/project.rs:64-96` (`cache_for_root`), plus the `write_metadata_for_loaded` call sites in it
- Test: `daemon/tests/project_cache.rs`

**Verified:** Read `project.rs:64-96`. The entire body runs under `self.loaded.lock()`: the miss path constructs `ImportCache::new` (opens redb, preloads recents) and writes shard metadata (`fs::write`) while holding the lock; the hit path also does a metadata `fs::write` under the lock every 60s. `cache_for_root` is called per import from parallel rayon workers - the cold batch serializes behind one DB open.

- [ ] **Step 1: Write the guard test** - two threads calling `cache_for_root` concurrently for the same fresh root must observe the same shard (same underlying cache: an entry inserted through one Arc is visible through the other) and produce exactly one metadata file. Passes before AND after; it pins the invariant the refactor must keep.
- [ ] **Step 2: Implement** - two-phase: under the lock, look up or reserve the shard id (e.g. `HashMap<ShardId, ShardSlot>` where `ShardSlot` holds an `Arc<OnceLock<Arc<ImportCache>>>`); release the lock; first reserver opens the DB and initializes the `OnceLock` (same-shard racers block only on that shard's init, other shards proceed); metadata writes happen after release using data cloned out of the critical section. Keep the poisoned-lock fallback behavior (`:95`).
- [ ] **Step 3: Run tests** - `cargo test -p import-lens-daemon --test project_cache` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "perf(cache): open project shards outside the registry lock

cache_for_root held the global loaded-shards mutex across the redb open
and recents preload on first use, and across a metadata fs::write every
60 seconds - so on a cold project every parallel analysis worker queued
behind one disk operation. Reserve the shard under the lock, initialize
it through a per-shard OnceLock after release, and write metadata outside
the critical section; only same-shard first-openers now wait."
```

---

### Task 17: Batch package invalidation across shards

**Files:**
- Modify: `daemon/src/cache/project.rs:230-255` (`invalidate_package` -> add a multi-package variant) and its `scan_disk_shards` usage; `daemon/src/service.rs:1101-1117` (`invalidate_package_json_paths`, `invalidate_package`)
- Test: `daemon/tests/project_cache.rs`

**Verified:** Read `project.rs:230-255` and `service.rs:1027-1119`. Per changed package (up to 20 per burst), `service.invalidate_package` -> `cache_registry.invalidate_package` -> `scan_disk_shards()` (which computes `directory_size` - a recursive fs walk - for every shard, unused by invalidation) plus a fresh redb open + full-table scan per non-loaded shard. A 15-package burst over 10 shards = ~150 redb opens + ~150 table scans + 150 discarded directory walks on the latency-sensitive watcher path. (`service.invalidate_package` also rebuilds shared resolvers and bumps the generation per package - the generation bump is a cheap atomic; fold the resolver rebuild into once-per-burst while here.)

- [ ] **Step 1: Write the guard test** - invalidating several package names across a loaded shard AND a disk-only shard evicts exactly the matching entries from both and leaves unrelated entries intact. (Behavior pin; perf is by construction.)
- [ ] **Step 2: Implement** - add `invalidate_packages(&self, names: &[String])` to the registry: one pass over loaded shards, ONE `scan_disk_shards`-lite call (new flag or sibling fn that skips `directory_size`), one open per non-loaded shard applying all names before closing. `service.invalidate_package_json_paths` collects names first, then calls the batch variant once, bumps the generation once, and invalidates graph caches/resolvers once. Keep single-name `invalidate_package` delegating to the batch variant.
- [ ] **Step 3: Run tests** - `cargo test -p import-lens-daemon --test project_cache` and `--test service` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "perf(cache): batch node_modules invalidation across shards

A watcher burst invalidated each changed package independently: every
package rescanned all disk shards (including an unused recursive
directory-size walk per shard) and reopened every non-loaded redb shard
for a full-table scan - O(packages x shards) opens on the invalidation
path. Collect the burst's package names, scan shards once without sizing,
open each cold shard once applying every name, and rebuild resolvers and
bump the cache generation once per burst."
```

---

### Task 18: Trim the memory-cache hit path

**Files:**
- Modify: `daemon/src/cache/memory.rs:92-117` (`ImportCache::get`)
- Test: `daemon/tests/memory_cache.rs`

**Verified:** Read `memory.rs:92-129`. On the restamp path (TTL lapsed or generation bumped - i.e. the first hit on every entry after any invalidation), the code deep-clones the whole `CachedImport` (result + per-transitive-module `dependency_fingerprints`) to update two `u64`s, then performs a SECOND `memory.get(key)` and clones `result` again. The fresh path also pays a redundant second lookup. Honest scope note (corrected during validation): removing the extra lookup and the clone-of-a-clone is safe and mechanical; a truly zero-copy restamp would require atomic stamp fields with a manual `Clone`/serde treatment - do that only if the mechanical fix leaves a measured hotspot.

- [ ] **Step 1: Strengthen the guard tests** - extend `memory_cache.rs`: (a) restamped hit returns the right result and a subsequent hit within the TTL skips re-stat (existing coverage - confirm); (b) close the audit-flagged gap: after `get()`, the STORED entry's `cache_hit` remains `false` (assert via a fresh clone-free accessor or by round-tripping through the disk tier - choose what the existing harness supports).
- [ ] **Step 2: Implement**

```rust
            if !fresh_without_restat {
                if !fingerprints_are_current(&cached.dependency_fingerprints) {
                    memory.remove(key);
                    self.disk.remove(key);
                    return None;
                }
                let mut restamped = cached.clone();
                restamped.verified_generation = generation;
                restamped.verified_at_millis = now;
                let mut result = restamped.result.clone();
                memory.insert(key.to_owned(), restamped);
                result.cache_hit = true;
                self.disk.touch(key);
                return Some(result);
            }

            let mut result = cached.result.clone();
            result.cache_hit = true;
            self.disk.touch(key);
            return Some(result);
```

- [ ] **Step 3: Run tests** - `cargo test -p import-lens-daemon --test memory_cache` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "perf(cache): drop the redundant re-lookup on memory-cache hits

ImportCache::get re-queried the map after restamping and cloned the result
out of the re-fetched entry - an extra lookup on every hit and an extra
result clone on the restamp path that every cached entry takes on its
first hit after an invalidation. Clone the result from the entry in hand
instead; the stored entry's cache_hit flag stays false, which the tests
now pin."
```

---

### Task 19: Keep recents timestamps monotonic across queue flushes

**Files:**
- Modify: `daemon/src/cache/disk.rs:550-557` (`write_pending_inserts`)
- Test: `daemon/tests/cache_disk.rs`

**Verified:** Read `disk.rs:135-155` (insert stamps `recorded_at_millis` = T1, removes any pending touch), `:195-211` (touch queues T2 with an independent 64-entry flush threshold), and `:550-557` (`recents.insert(key, entry.recorded_at_millis)` unconditionally). Sequence insert(T1) -> memory-hit touch(T2>T1) -> touch queue flushes -> insert queue flushes later: the recents row regresses to T1, demoting the key in the startup-preload/prewarm ranking (FR-026b). Results are never wrong; only recency ranking.

- [ ] **Step 1: Write the failing test** - insert K, flush touches carrying a later timestamp for K, then flush inserts; `recent_keys` ordering must reflect the later timestamp (use the existing cache_disk test helpers for forcing flushes).
- [ ] **Step 2: Run test to verify it fails** -> Expected: FAIL (K ranked by the older insert time).
- [ ] **Step 3: Implement** - in `write_pending_inserts`, never lower an existing recents value:

```rust
            let keep_newer = recents
                .get(key.as_str())
                .ok()
                .flatten()
                .map(|existing| existing.value() >= entry.recorded_at_millis)
                .unwrap_or(false);
            if !keep_newer {
                recents
                    .insert(key.as_str(), entry.recorded_at_millis)
                    .map_err(|error| format!("failed to update recents table: {error}"))?;
            }
```

- [ ] **Step 4: Run tests** - `cargo test -p import-lens-daemon --test cache_disk` -> Expected: PASS. While in this file, also strengthen the audit-flagged reload test (`flush_to_disk_persists_memory_entries_for_reload`): assert a couple of round-tripped fields (`brotli_bytes`, `confidence`) instead of only key presence.
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(cache): keep recents timestamps monotonic across flush ordering

Insert and touch queues flush at independent thresholds, and the insert
flush wrote its (older) recorded-at timestamp unconditionally - so a key
touched after insertion could regress in the recents table when the touch
batch flushed first, wrongly demoting it out of the startup preload and
prewarm set. Only write an insert's recents timestamp when it is newer
than the stored value. Also pin field-level integrity in the disk reload
test rather than key presence alone."
```

---

### Task 20: Key registry-refresh generations per target

**Files:**
- Modify: `extension/src/guidance/registryRefresh.ts:94-106` (generation bookkeeping) and its supersede checks
- Test: `extension/test/guidance/registryRefresh.test.ts`

**Verified:** Read `registryRefresh.ts:85-200` and `packageJsonAnalysis.ts:229-266`. Every streamed partial calls `queueRegistryRefreshes` (`:244`) -> `refresh` -> `#beginGeneration(keyFor(uri))` - a PER-URI counter - so partial N+1 supersedes partial N even though their target sets are disjoint; a superseded response preserves the old `registryHintRefreshStatus` (`:171-177`) instead of marking fresh/stale. The final full-target refresh (`:148`) eventually heals statuses, but the interim is wrong and every streaming analysis fires N+1 overlapping daemon refreshes whose earlier responses are discarded.

- [ ] **Step 1: Write the failing test** - two `refresh` calls for the same URI with disjoint targets; the first call's response arrives after the second call began; assert the first call's targets still get their `fresh`/`stale` status applied (currently preserved-stale because the per-URI generation superseded them).
- [ ] **Step 2: Run test to verify it fails** -> Expected: FAIL.
- [ ] **Step 3: Implement** - key generations by target: `#generations: Map<string, number>` keyed `${keyFor(uri)}::${name}@${installedVersion ?? ""}`; `#beginGeneration` stamps each target in the batch; `#isSuperseded(uri, target, generation)` compares per target. `forget(uri)` clears by key prefix.
- [ ] **Step 4: Run tests** -> Expected: PASS (plus existing registryRefresh tests).
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(guidance): track registry refresh generations per target

Refresh generations were keyed per document, so each streamed
package.json partial's refresh superseded the previous partial's even
though their dependency targets were disjoint - earlier targets' responses
were discarded, leaving their refresh status stale until the final
full-document refresh repaired it. Key generations per (document, package,
version) so overlapping refreshes only supersede genuinely re-requested
targets."
```

---

### Task 21: Memoize `.importlensignore` loading per workspace report (DF-8, promoted from backlog)

**Files:**
- Modify: `daemon/src/service.rs` (report path around `:1301` where `load_import_lens_ignore(active_path)` is called per analyzed file)
- Test: `daemon/tests/report.rs` (behavior pin)

**Verified:** Agent-verified against `service.rs:1301` (unconditional per-file `load_import_lens_ignore`) and cross-checked with the repo's own backlog: `docs/superpowers/plans/2026-07-03-perf-plan-d-small-and-watchlist.md` Task D3 / daemon-review DF-8 describe exactly this and were deliberately parked as optional. Promoted here because the report path is the one place the O(files x ancestor-depth) rescan compounds. This is the only backlog-promoted task in the plan; skipping it (user's call) breaks nothing else.

- [ ] **Step 1: Write the guard test** - a report over a workspace with a root `.importlensignore` excluding one package: the excluded import stays excluded for files in nested directories (pins that memoization respects per-directory resolution).
- [ ] **Step 2: Implement** - a per-report `HashMap<PathBuf, Arc<IgnoreRules>>` keyed by the file's parent directory (or the nearest ancestor chain node), threaded through the report's per-file closure; `load_import_lens_ignore` consults it before walking ancestors. Scope the memo to one report run - config edits between reports stay live.
- [ ] **Step 3: Run tests** - `cargo test -p import-lens-daemon --test report` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "perf(report): memoize .importlensignore resolution per report run

The workspace report reloaded and re-walked the .importlensignore ancestor
chain for every analyzed file - O(files x depth) filesystem hits per
report. Cache resolved rules per directory for the duration of one report
run; per-directory semantics and between-run freshness are unchanged.
Promoted from the DF-8 backlog entry."
```

---

# PART C - Dead Code Removal

*(All entries below were confirmed by grep during validation: each symbol/file appears only at its own definition plus tests - zero `extension/src` / `daemon/src` consumers.)*

### Task 22: Delete the unreachable decoration-group modules

**Files:**
- Delete: `extension/src/ui/inlineHintDecorations.ts`, `extension/src/ui/packageJsonDecorationSegments.ts`, and any test file exercising only them

**Verified:** Grep: `packageJsonDecorationSegments` imported by nothing in `src/`; `inlineHintDecorations` imported ONLY by that dead file. Live controllers import from `inlineHintDecorationTypes.ts` directly.

- [ ] **Step 1:** Re-confirm: `rg "inlineHintDecorations|packageJsonDecorationSegments" extension/src` -> only self-references.
- [ ] **Step 2:** Delete both files + their dedicated tests.
- [ ] **Step 3:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "refactor(ui): remove unreachable decoration-group modules

packageJsonDecorationSegments.ts had no importer and
inlineHintDecorations.ts was imported only by it - both are leftovers from
the migration to the decoration-layers model that the live controllers use
directly. Delete them and their tests."
```

---

### Task 23: Remove dead `formatImportSize` / `formatWarningSuffix` and the unused `runtime` parameter

**Files:**
- Modify: `extension/src/ui/format.ts` (delete `formatImportSize` `:130`, `formatWarningSuffix` `:45`; drop `runtime` from `formatImportSizePrimary` `:106-128`)
- Modify: `extension/src/ui/importHintParts.ts:72` (drop the argument)
- Modify: `extension/test/ui/format.test.ts` (remove dead-function cases)

**Verified:** Grep: `formatImportSize` referenced only in `format.ts` (definition + internal call) and its test; `importHintParts` uses `formatImportSizePrimary` + `importHintTagLabels`. Read `format.ts:106-128` via callers: `runtime` is never read in the body; `importHintParts.ts:72` threads it pointlessly. `formatWarningSuffix` duplicates `importHintTagLabels`' tag conditions - a drift hazard.

- [ ] **Step 1:** Delete the two functions; remove the parameter and its call-site arguments.
- [ ] **Step 2:** Retarget/remove the affected `format.test.ts` cases (keep `formatImportSizePrimary` + tag-label coverage).
- [ ] **Step 3:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "refactor(ui): drop the dead single-string import formatter

formatImportSize/formatWarningSuffix survived the migration to the
structured hint-parts model with no production caller, and they duplicate
the tag rules that importHintTagLabels now owns - a silent divergence
point for any future tag change. Remove them, their tests, and the unused
runtime parameter that formatImportSizePrimary never read."
```

---

### Task 24: Remove `compareImportItemsForResponse`

**Files:**
- Modify: `extension/src/ui/compareImportItems.ts:14`
- Modify: `extension/test/ui/compareImportItems.test.ts` (retarget to `compareImportItemsForResults`)

**Verified:** Grep: production `compareImports.ts` calls `compareImportItemsForResults`; the `BatchResponse`-shaped sibling exists only for its test.

- [ ] **Step 1:** Delete the function; retarget its test at the live sibling with equivalent fixtures.
- [ ] **Step 2:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(ui): remove the batch-response compare adapter

Compare Imports moved to analyzeSpecifiers; the BatchResponse-taking
adapter kept only its own test alive. Remove it and point the test at the
live results-based builder."
```

---

### Task 25: Reuse `cached_import_request_from_key` in the prewarm path

**Files:**
- Modify: `daemon/src/prefetch.rs:251-262` (inline struct build) to call `cached_import_request_from_key` (`:301-312`)
- Test: `daemon/tests/prefetch.rs` (existing coverage)

**Verified:** Agent grep: the exported helper is referenced only from tests; `run_recent_prewarm_job` re-implements the same six field assignments inline - drift hazard if `ImportRequest` grows.

- [ ] **Step 1:** Replace the inline build with the helper (decode once; keep the separate resolved-path derivation).
- [ ] **Step 2:** `cargo test -p import-lens-daemon --test prefetch` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(prefetch): build prewarm requests through the shared decoder

run_recent_prewarm_job re-assembled the ImportRequest field-by-field while
the exported cached_import_request_from_key did the same decoding for
tests only - two copies to keep in sync whenever the request shape grows.
Route the prewarm path through the shared helper."
```

---

### Task 26: Remove the dead Batch / streaming-batch transport surface

**Files:**
- Delete: `extension/src/analysis/batchPartial.ts`, `extension/src/analysis/status.ts`, `extension/test/analysis/batchPartial.test.ts`, `extension/test/analysis/status.test.ts`
- Modify: `extension/src/daemon/manager.ts` (`sendBatch`), `extension/src/daemon/transport.ts` (interface + coordinator `sendBatch`), `extension/src/daemon/nativeTransport.ts` (`sendBatch` + batch timeout constant), `extension/src/ipc/client.ts` (`sendBatch`, `#batchPending`, `isBatchResponse`, the batch branch of partial handling), `extension/src/ipc/protocol.ts` (`BatchRequest`/`BatchResponse` TS types if then unreferenced)
- Keep: ALL Rust protocol handlers (`handle_batch` etc.) - wire compatibility for the CLI/accuracy scripts and protocol v7 stability.

**Verified:** Grep: `sendBatch` has no production caller (live paths: `analyzeDocument`, `analyzeSpecifiers`); `applyStreamingBatchPartial`/`applyFinalBatchResults`/`markLoadingStatesUnavailable` referenced only by their tests. User approved removal (2026-07-03). CAUTION (validated): `isStreamingPartial` and the partial dispatch in `client.ts` are shared with `analyzePackageJson`/`refreshRegistryHints` streaming - remove only the batch-specific branches, not the shared guard.

**Open sub-decision (user):** the `requestFileSize` chain (`manager.ts:82` -> `transport.ts:158` -> `nativeTransport.ts` -> `client.ts`) is equally production-dead (docs DF-11; live path is `requestFileSizeDocument`). Default here: REMOVE it in the same commit for the same rationale - veto this line if you want it kept for the WASM transport plan.

- [ ] **Step 1:** Re-confirm callers: `rg "sendBatch|requestFileSize\b|applyStreamingBatchPartial|applyFinalBatchResults|markLoadingStatesUnavailable|isBatchResponse" extension/src` -> only the definitions listed above (`requestFileSizeDocument` is a different symbol - keep it).
- [ ] **Step 2:** Delete the files/tests; strip the methods, pending map, batch type-guard branch, and timeout constant across the four layers; remove the TS `BatchRequest`/`BatchResponse` types if nothing else references them.
- [ ] **Step 3:** `pnpm check && pnpm test:ts` -> Expected: PASS (no dangling references).
- [ ] **Step 4: Commit**

```bash
git commit -m "refactor(daemon): remove the dead batch transport surface

sendBatch and the streaming-batch merge helpers (batchPartial.ts,
status.ts) had no production caller - document analysis has used
analyzeDocument/analyzeSpecifiers since the daemon-first migration - and
carried a latent timeout bug (batch partials never reset the request
timeout). Remove the client method, transport wrappers, pending map, batch
type guards, their tests, and the file-size twin of the same dead surface.
The Rust batch handlers stay for protocol v7 wire compatibility."
```

---

# PART D - DRY Consolidations

### Task 27: Single compression byte/label selector

**Files:**
- Modify: `extension/src/ui/format.ts` (export `bytesForCompression` + `labelForCompression`); replace the copies in `extension/src/ui/tooltipMarkdown.ts:18-30`, `extension/src/ui/packageJsonLabels.ts:86-110`, `extension/src/analysis/fileSize.ts:5-24`

**Verified:** Grep confirmed all four copies (format.ts:17-35, fileSize.ts:6-24, packageJsonLabels.ts:90-110, tooltipMarkdown.ts:22-26) - same `gzip/zstd/else-brotli` switch + `{br,gz,zstd}` label map.

- [ ] **Step 1:** Export the two helpers from `format.ts`, typed against the structural `{ gzip_bytes; brotli_bytes; zstd_bytes }` shape both response types share.
- [ ] **Step 2:** Replace the three duplicate implementations with calls; behavior identical (existing tests stay green throughout).
- [ ] **Step 3:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "refactor(ui): centralize the compression byte and label selectors

The compression-to-bytes switch and the br/gz/zstd label map were
reimplemented in four modules (format, tooltipMarkdown, packageJsonLabels,
analysis/fileSize); adding a format or changing a label meant four edits
or divergent renders. Export one selector pair from format.ts and use it
everywhere."
```

---

### Task 28: Extract a shared decoration-controller base

**Files:**
- Create: `extension/src/ui/inlineHintDecorationController.ts`
- Modify: `extension/src/ui/decorations.ts`, `extension/src/ui/packageJsonDecorations.ts`

**Verified:** Agent-verified byte-identical `refreshVisibleEditors` / `refreshUri` / pool field / `dispose()` across both controllers (decorations.ts:35-47,72-75; packageJsonDecorations.ts:37-49,82-85); only `refreshEditor` differs. Existing controller tests pin behavior.

- [ ] **Step 1:** Abstract base class holding the decoration pool, store subscription, `refreshVisibleEditors`, `refreshUri`, `dispose`; abstract `refreshEditor(editor)`.
- [ ] **Step 2:** Re-parent both controllers; no behavior change.
- [ ] **Step 3:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git commit -m "refactor(ui): share the decoration-controller lifecycle

DecorationController and PackageJsonDecorationController carried
byte-identical editor-refresh, URI-matching, and disposal logic - a
two-place fix for any lifecycle bug. Hoist the shared lifecycle into an
abstract base with a single refreshEditor extension point."
```

---

### Task 29: Share the `SourceRange` -> `vscode.Range` converter

**Files:**
- Modify: `extension/src/ui/hoverRanges.ts` (add the exported helper), `extension/src/ui/hoverProvider.ts:9`, `extension/src/ui/budgetDiagnostics.ts:53`

**Verified:** Agent-verified duplicate converters (same construction, different names).

- [ ] **Step 1:** One exported `rangeFromSourceRange`; replace both locals.
- [ ] **Step 2:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(ui): share the protocol-range converter

hoverProvider and budgetDiagnostics each declared their own
SourceRange-to-vscode.Range conversion. Export one helper beside the other
range utilities and use it in both."
```

---

### Task 30: Collapse `server.rs` response/error plumbing

**Files:**
- Modify: `daemon/src/ipc/server.rs:699-786` (eight `*_response_from_join` fns) and the shared prologue of the `protocol_error_*` family (`:791+`)

**Verified:** Grep: eight structurally-identical join helpers (batch, exports, file_size, analyze_document, analyze_package_json, analyze_specifiers, file_size_document, complete_import_members) and nine+ `protocol_error_*` builders repeating the `version.min(PROTOCOL_VERSION)` / `request_id` / diagnostics prologue.

- [ ] **Step 1:** One generic replaces the eight:

```rust
async fn response_from_join<T, R>(
    handle: JoinHandle<T>,
    request: &R,
    on_error: impl FnOnce(&R, String) -> T,
) -> T {
    match handle.await {
        Ok(response) => response,
        Err(error) => on_error(request, join_error_message(error)),
    }
}
```

  Call sites become `response_from_join(handle, &request_for_error, protocol_error_batch_response).await`. Keep the typed `protocol_error_*` builders (they carry per-response shape) but factor their shared version/request_id/diagnostics prologue into a small helper where mechanical.
- [ ] **Step 2:** `cargo test -p import-lens-daemon --test server` -> Expected: PASS (existing coverage pins behavior).
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(ipc): collapse duplicated response-join plumbing

Eight request types each carried an identical join-or-protocol-error
wrapper, and every protocol_error_* builder restated the same
version/request-id/diagnostics prologue - roughly ninety lines of
mechanical duplication that every new request type extended. Replace the
wrappers with one generic response_from_join and share the error-response
prologue."
```

---

### Task 31: Extract the shared debounced-document scheduler

**Files:**
- Create: `extension/src/analysis/debouncedDocumentScheduler.ts`
- Modify: `extension/src/listener.ts:59-73,166-187`, `extension/src/guidance/packageJsonAnalysis.ts:80-94,279-299`

**Verified:** Agent-verified near-identical `#timers` map + `schedule` + `disposeDocument` + `dispose` in both controllers, already drifting slightly.

- [ ] **Step 1:** Extract `DebouncedDocumentScheduler` (timer map keyed by document, `schedule(document, run)` honoring `config.debounceMs`, `disposeDocument`, `dispose`); each controller keeps its own subscriptions and freshness tracker.
- [ ] **Step 2:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(analysis): share the debounced document scheduler

The document and package.json controllers each reimplemented the per-URI
debounce timer map with slightly different disposal behavior - the kind of
drift that turns into a leak in exactly one of them. Extract one scheduler
used by both."
```

---

### Task 32: Share pipeline helper predicates

**Files:**
- Modify: `daemon/src/pipeline/fallback.rs:147-167` + `daemon/src/pipeline/types_only.rs:127-147` (skip-directory predicate/list -> one source); `daemon/src/pipeline/bundle.rs:728-734` + `daemon/src/pipeline/cjs_scan.rs:464-469` (identifier predicates -> one source); `daemon/src/pipeline/cjs.rs:156-162` + `daemon/src/pipeline/file_size.rs:252-258` + the inline copy in `analyze.rs` (`diagnostic()` constructor -> one source)

**Verified:** Grep confirmed the fallback.rs skip-list (`node_modules`/`.git`/`coverage`/`target`...); agent verified the types_only twin and the identifier/diagnostic duplicates. The skip-list is correctness-relevant: today a new ignored directory requires two edits.

- [ ] **Step 1:** Hoist into a small `daemon/src/pipeline/util.rs` (or extend `replacements.rs` if the team prefers no new module): `should_skip_package_directory(name)`, `is_identifier_start/continue`, `diagnostic(stage, message, details)`.
- [ ] **Step 2:** `cargo test -p import-lens-daemon` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(pipeline): dedupe directory-skip, identifier, and diagnostic helpers

The package-scan skip list existed in two files (a correctness constant
that must never drift), and the identifier predicates and diagnostic
constructor were each copied across pipeline modules. Hoist all three into
one shared pipeline util."
```

---

### Task 33: Deduplicate the package.json request's initial timeout arm

**Files:**
- Modify: `extension/src/ipc/client.ts:192-197`

**Verified:** Read `client.ts:172-235`. `requestAnalyzePackageJson` defines `resetTimeout` (`:180-190`) then restates its body verbatim to arm the initial timer (`:192-197`); the sibling `requestRefreshRegistryHints` simply calls `resetTimeout()` (`:235`).

- [ ] **Step 1:** Replace lines 192-197 with `resetTimeout();` (existing `client.test.ts` timeout coverage pins behavior).
- [ ] **Step 2:** `pnpm check && pnpm test:ts` -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(ipc): arm the package.json request timeout via resetTimeout

requestAnalyzePackageJson duplicated the reset-timeout body to arm the
initial timer, unlike its registry-refresh sibling which calls
resetTimeout() - two copies of the timeout logic to keep in sync. Use the
function for the initial arm as well."
```

---

# PART E - Scripts / CLI

### Task 34: Anchor the CI gate's changed files to the repository root

**Files:**
- Modify: `cli/importlens.mjs:79-86` (resolution) + `:156-159` (`changedFiles`)
- Test: `scripts/test/importlens-cli.test.mjs`

**Verified:** Read both regions. `git diff --name-only ... HEAD --` emits repo-root-relative paths; the gate resolves them with `path.resolve(cwd, filePath)` - correct only when cwd IS the repo root. From a sub-package, files resolve to nonexistent paths (ENOENT aborts the whole check) or, worse, to same-named files under cwd. Note (validated): budgets legitimately come from CWD's `.importlensrc.json`/`package.json` - keep budgets cwd-scoped; only FILE resolution anchors to the git top-level.

- [ ] **Step 1: Write the failing test** - with the existing CLI test harness, run the gate from a subdirectory of a temp git repo whose changed file lives outside that subdirectory; assert the file is found and analyzed (no ENOENT, correct relative reporting).
- [ ] **Step 2: Run -> FAIL.**
- [ ] **Step 3:** In `changedFiles`, also run `git rev-parse --show-toplevel` (same cwd) and return `{ topLevel, files }`; resolve each file against `topLevel`; pass `topLevel` as the daemon `workspace_root`; keep budget discovery reading from `cwd`. Report violations relative to `cwd` as today.
- [ ] **Step 4: Run -> PASS** (`pnpm test:scripts`).
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(cli): resolve changed files against the git repository root

'git diff --name-only' prints repository-root-relative paths, but the
check gate resolved them against the invocation directory - running
'importlens check' from a sub-package aborted with ENOENT (or analyzed a
same-named file under the subdirectory). Resolve changed files against
'git rev-parse --show-toplevel' and hand that root to the daemon, while
budget discovery stays scoped to the invocation directory."
```

---

### Task 35: Launch `pnpm` through a shell on Windows in the oxc updater

**Files:**
- Modify: `scripts/update-oxc-stack.mjs:153-159` (`updateLockfiles`)
- Test: `scripts/test/update-oxc-stack.test.mjs`

**Verified:** Read `:95-159`. `updateLockfiles` calls `execFile("pnpm", ...)` with no shell - on Windows `pnpm` is `pnpm.CMD`, which `CreateProcess` cannot launch, so the call throws. Worse: source edits are written at `:107-112` BEFORE `updateLockfiles` at `:113`, so a real run on Windows leaves manifests bumped with stale lockfiles. `package-target.mjs`/`package-vsix.mjs` already special-case win32+pnpm with `shell: true`.

- [ ] **Step 1: Write the failing test** - `updateLockfiles` invoked with a recording exec stub must route the pnpm invocation through the win32 shell path (assert the same shape the packaging scripts use) when `process.platform === "win32"` is simulated via the injectable seam.
- [ ] **Step 2: Run -> FAIL.**
- [ ] **Step 3:** Mirror the packaging scripts' guarded spawn for the pnpm call (cargo is a real .exe - leave it on execFile).
- [ ] **Step 4: Run -> PASS** (`pnpm test:scripts`).
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(scripts): launch pnpm via a shell on Windows in the oxc updater

updateLockfiles invoked pnpm through execFile without a shell; on Windows
pnpm resolves to pnpm.CMD, which CreateProcess cannot start, so a real
'pnpm deps:update:oxc' crashed after the manifests were already written -
leaving bumped versions with stale lockfiles on the project's primary
platform. Use the same win32 shell guard the packaging scripts already
apply to pnpm."
```

---

### Task 36: Fail loudly when a selected daemon binary is missing during hashing

**Files:**
- Modify: `scripts/daemon-hashes.mjs:56-88` (`collectDaemonHashes` / `updateKnownDaemonHashes`), `scripts/generate-daemon-hashes.mjs` (surface the error, non-zero exit)
- Test: `scripts/test/daemon-hashes.test.mjs`

**Verified:** Read `:50-88`. For a selected target with no binary, `collectDaemonHashes` silently `continue`s while `updateKnownDaemonHashes` has already deleted that target's existing hash - so `pnpm hash:daemon` (defaults to ALL targets) on a partial local build silently strips other platforms' integrity hashes and exits 0. This also explains the committed 5-of-6 hash set (`win32-arm64` missing).

- [ ] **Step 1: Write the failing test** - `updateKnownDaemonHashes` with a selected target whose binary is absent must throw (and must not have removed the target's previous hash); present targets still refresh.
- [ ] **Step 2: Run -> FAIL** (currently returns success minus the entry).
- [ ] **Step 3:** Per selected target: if the binary exists, delete + recompute; if missing, throw `Error("daemon binary missing for <target> at <path>; build it or pass an explicit target list")`. `generate-daemon-hashes.mjs` exits non-zero on that error. Also settle the `win32-arm64` question here: check `.github/workflows` for whether the release pipeline builds+hashes it per-target; if it is a real release target ensure it is covered, otherwise remove it from `platformTargets`/`PlatformTarget` so an unsupported arch reads "unsupported platform" instead of "no trusted hash".
- [ ] **Step 4: Run -> PASS** (`pnpm test:scripts`).
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(scripts): refuse to drop integrity hashes for unbuilt daemon targets

Hash generation deleted every selected target's known hash up front and
silently skipped targets whose binary was absent - so running
'pnpm hash:daemon' with a partial local build stripped other platforms'
integrity hashes and still exited 0, weakening runtime binary
verification. Fail with a non-zero exit naming the missing binary instead,
and refresh only targets that are actually present."
```

---

### Task 37: Fix `assert-vsix-size`'s no-argument scan directory

**Files:**
- Modify: `scripts/assert-vsix-size.mjs:10-12`

**Verified:** Agent-verified: VSIXes are written under `builds/` (`targets.mjs:116`, `package-vsix.mjs:63`), but the no-arg default scans the repo root - bare `pnpm assert:vsix-size` always fails with "No VSIX files found" (masked because every current caller passes args).

- [ ] **Step 1:** Default the scan to `builds/` (keep explicit args taking precedence).
- [ ] **Step 2:** Manual check: with a `builds/*.vsix` present, `pnpm assert:vsix-size` passes; without, it errors as intended.
- [ ] **Step 3: Commit**

```bash
git commit -m "fix(scripts): scan builds/ by default in the VSIX size gate

VSIX files are always written under builds/, but the no-argument mode of
assert-vsix-size scanned only the repository root, so a bare
'pnpm assert:vsix-size' reported no VSIX files even when builds existed.
Default the scan to builds/ while keeping explicit paths authoritative."
```

---

### Task 38: Harden the accuracy-compare IPC client

**Files:**
- Modify: `scripts/accuracy-compare.mjs:261-300` (`daemonClient`)

**Verified:** Read `:261-300`. No `close` handler (daemon EOF strands pending promises -> `pnpm test:accuracy` hangs until job timeout), unguarded `decode` inside the data handler, no per-request timeout; correlation is positional (`pending.shift()`), acceptable for this ordered dev harness once close/timeout reject stragglers.

- [ ] **Step 1:** Add `socket.on("close", ...)` rejecting all pending, wrap `decode` in try/catch (reject + destroy on parse failure), add a per-request timeout mirroring `cli/importlens.mjs`.
- [ ] **Step 2:** Manual check: kill the daemon mid-run; the script exits with an error instead of hanging.
- [ ] **Step 3: Commit**

```bash
git commit -m "fix(scripts): fail fast when the accuracy daemon dies mid-run

The accuracy-compare client had no close handler and no request timeout,
so a daemon exit between request and response stranded the pending promise
and hung the whole run until the CI job timeout; a malformed frame threw
inside the data handler. Reject pending requests on close, guard frame
decoding, and time out individual requests like the CLI client does."
```

---

### Task 39: Make `replaceKnownVersions` context-aware

**Files:**
- Modify: `scripts/oxc-stack-helpers.mjs:78-81`
- Test: `scripts/test/update-oxc-stack.test.mjs`

**Verified:** Agent-verified: blunt chained `replaceAll` of the raw version literals across the SRS + two test files; rewrites unrelated occurrences and, if one version were a substring of the other, the first replacement would corrupt the second's needle.

- [ ] **Step 1: Write the failing test** - a fixture containing the oxc version literal in an unrelated sentence must survive the update unchanged; the pinned-version lines must update.
- [ ] **Step 2: Run -> FAIL.**
- [ ] **Step 3:** Anchor replacements to the known contexts (crate/package tokens adjacent to the version, mirroring `updateCargoToml`'s precise-regex approach), not the bare literal.
- [ ] **Step 4: Run -> PASS** (`pnpm test:scripts`).
- [ ] **Step 5: Commit**

```bash
git commit -m "fix(scripts): replace pinned oxc versions in context, not by bare literal

replaceKnownVersions ran a global replaceAll of the raw version strings
over the SRS and two test files, rewriting any unrelated occurrence of the
literal, and its two chained replacements could corrupt each other if one
version string ever contained the other. Anchor the replacements to the
pinned-version contexts, matching the precise approach Cargo.toml updates
already use."
```

---

### Task 40: Drop the redundant staged-manifest key

**Files:**
- Modify: `scripts/package-vsix-manifest.mjs:15-21`

**Verified:** Agent-verified: `dependencies: manifest.dependencies` is a no-op after `...manifest` (unlike the `undefined` assignments, which strip keys).

- [ ] **Step 1:** Delete the line.
- [ ] **Step 2:** `pnpm test:scripts` (manifest test) -> Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git commit -m "refactor(scripts): drop the no-op dependencies restatement

The staged VSIX manifest spread already carries dependencies; restating
'dependencies: manifest.dependencies' did nothing (unlike the undefined
assignments, which deliberately strip keys) and read as if it had an
effect. Remove it."
```

---

# Final

### Task 41: Rebuild daemon, refresh hashes, full verification

**Files:** `extension/src/daemon/knownHashes.generated.ts` (regenerated); build artifacts (ignored)

- [ ] **Step 1:** `cargo fmt --check` + `pnpm test:rust` -> all green.
- [ ] **Step 2:** `pnpm check` + `pnpm test` -> all green.
- [ ] **Step 3:** `pnpm package:win32-x64` -> succeeds; VSIX under the 20 MB gate (`assert:vsix-size`).
- [ ] **Step 4:** Review `git status --short` + the staged hash diff; commit:

```bash
git commit -m "chore(daemon): refresh the Windows daemon hash after hardening pass

Rebuild the win32-x64 daemon and refresh its embedded SHA-256 so the
extension's binary integrity gate (NFR-014a) accepts the binary carrying
the pipeline, document, IPC, cache, and registry fixes from this plan."
```

---

# PART F - Deliberately NOT Fixed (validated exclusions)

Recorded so future reviews do not re-litigate. Each was examined during the 2026-07-03 validation pass.

| Item | Why excluded |
|---|---|
| CJS bare `require("pkg")` kept external (`cjs.rs:136-138`) | SRS FR-024a specifies static scanning of literal RELATIVE requires only; following bare requires is a product/spec change, not a bug fix. Revisit as a feature with SRS update. |
| Recursive `load_module_from` stack depth (graph.rs) | DF-7: deliberately parked in the repo backlog; bounded by the 2000-module limit; no real package reproduces the risk. |
| `remove_shard_by_id` vs in-flight analyses on Windows (project.rs:329-359) | DF-9: parked; failure is transient and self-correcting, UI already reports it. |
| Compact cache keys (key.rs) / cross-workspace registry metadata | DF-10 / DF-12d: parked with documented triggers. |
| `compute_file_size` first-seen edge merge for mixed runtimes | DF-12c: accepted approximation, documented. |
| TOCTOU between binary hash check and spawn (nativeTransport) | Requires local write access to the extension dir; the hash gate's threat model is corrupted/partial installs, not a local attacker. Documenting the threat model is a docs nicety, not a code fix. |
| Reachability/rename repeated DFS memoization (bundle.rs/reachability.rs) | Win is graph-shape-dependent; adds cross-pass cache complexity. Revisit only if profiling shows barrel-heavy graphs hot after Task 15. |
| package.json single-line section-summary anchor collision | Cosmetic, requires a single-line dependencies object (rare); low confidence it renders wrongly at all. |
| `literal_dynamic_import_specifier` `value[1..len-1]` guard | Traced unreachable for parsed dynamic imports (span length >= 2 guaranteed post-parse-guard). Defensive edit only; skipped per "no fake fixes". |
| Compression round-trip test (compress.rs) | The wrappers are 5 lines/format over mature crates; a size>0 assertion plus the pipeline-level size tests already catch wiring mistakes. A round-trip adds decode deps for negligible protection. |
| `changedLinesForFile` temp-git-repo integration test | The exec wrapper becomes thin after Task 11 (one `rev-parse`, one `show`); the diff core is now a directly-tested pure function. An init-commit-edit repo fixture per test run is high-maintenance for the residual risk. |
| SRS section 13.4 items (cache webview, status-bar icon menu), WASM fallback, linux-armhf, telemetry | Explicitly future (v1.1) scope in the SRS; not incomplete work. |
| Rust `Batch`/`FileSize` protocol handlers | Kept intentionally: wire compatibility (protocol v7) and used by dev scripts; only the dead TS client surface is removed (Task 26). |

---

## Self-Review (run after the rewrite - all items checked)

- **Anti-hallucination:** every task carries a `Verified:` line naming the exact lines read and, where applicable, the hand-traced failure. Two agent claims were corrected during validation (test harness is node:test, not vitest; the `lang` mis-detection mechanism), one proposed fix was strengthened (Task 6 stale-clear branch), one was redesigned (Task 11 buffer diff replaces suppress-on-dirty, which would have gutted the while-typing delta feature), and one overstated perf claim was restated honestly (Task 18).
- **Spec coverage:** Tasks 8 and 9 update the SRS where behavior it describes changes; IPC/compression/graph invariants pinned in Global Constraints.
- **No placeholders:** confirmed-bug tasks carry real RED/GREEN code; harness-dependent steps say exactly which existing helper pattern to adapt and why.
- **Type/name consistency:** `strip_keyword`, `changedLinesBetween`, `response_from_join`, `bytesForCompression`/`labelForCompression`, `rangeFromSourceRange`, `DebouncedDocumentScheduler` - each defined once and referenced by that exact name in consuming steps.
- **Ordering:** Part A before B/C/D/E; Task 2 (Astro panic) before Task 3 (which may use it as the panic fixture); Task 22-26 removals before Part D refactors that touch neighboring files; Task 41 last.
- **Commit discipline:** one commit per task; messages state the user-visible symptom, the mechanism, and the fix rationale.

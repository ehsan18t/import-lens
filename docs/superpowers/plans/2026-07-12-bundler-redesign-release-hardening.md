# Bundler Redesign — Release Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every open finding from the post-cutover verification of the Rolldown engine, so `bundler-redesign` can ship without serving a wrong size, a false diagnostic, or a dark regression guard.

**Architecture:** The Rolldown cutover is architecturally complete; this plan does not revisit it. The work is (a) three live reproductions that convert statically-confirmed findings into executable failing tests, (b) the correctness fixes that stop a user seeing wrong output, (c) the regression guards that keep them fixed, (d) the performance the shipped miss path gives away, (e) the re-baseline, the two open design decisions, and the release mechanics.

**Tech Stack:** Rust daemon (`daemon/`), Rolldown `=1.1.5` on OXC `=0.139.0`, `oxc_resolver =11.23.0`; TypeScript extension host (`extension/`); pnpm; lefthook; GitHub Actions.

**Source spec:** [`docs/superpowers/specs/2026-07-11-bundler-redesign-verification-findings.md`](../specs/2026-07-11-bundler-redesign-verification-findings.md). Finding IDs (W4, R2, N2, …) refer to that document and are the authority for *why* each task exists.

**Every open finding in the spec has a task here.** Nothing is deferred to a future milestone. Where a finding resolves to a *decision* rather than a code change (R1, I9, W5), the decision itself is a task with a recorded outcome.

## Ordering rationale

Tasks are ordered by **"can a user see wrong output today?"**, not by the spec's section order.

**Phase 1 is the release blockers**, ranked by blast radius × severity:

| # | Finding | Who sees it | Why here |
|---|---|---|---|
| 1 | **N2** | **Every user, every build** | Every raw size is inflated ~2% and every module breakdown is contaminated. Broadest possible reach, and the fix is one option field. It goes first because it is cheap, broad, and **invalidates the Task 0.3 benchmark** if landed later. (An earlier draft claimed it also invalidates byte-asserting tests — that is **false**: every byte assertion in `daemon/tests` is relational and N2-invariant. See Task 1 Step 5.) |
| 2 | **W4** | TS users importing a type without the keyword | The most *severe* single failure: valid, compiling TypeScript yields a hard error, size 0, and a failed file aggregate. Deterministic, reproduced live against `commander`. |
| 3 | **I15** | Astro/mixed-runtime files | A silently wrong aggregate — the build succeeds, so nothing warns. |
| 4 | **R2** | Anyone who saves a file mid-analysis | A wrong size served as fresh, and *sticky* — it never self-heals. Narrower trigger than the above, but it does not recover. |
| 5 | **R5** | First-party monorepo manifest edits | Same shape as R2, narrower trigger. |
| 6 | **I11/I10/I13/I8** | Anyone reading a diagnostic | False *messages*, not wrong *numbers* — I13 currently tells the user the size is conservative when it is actually undercounted. |

**Phase 2 is the guards**, immediately after — a fix with no gate is a fix that regresses. N1 comes before R3 because R3's CI job depends on the fixture preparer being reliable.

**Phase 3 is performance.** No measured regression exists yet (that is what Task 0.3 establishes), so nothing here blocks release.

**Phase 4 is the re-baseline, the decisions, and the release.** The re-baseline **must** come after every measurement-affecting change (Phase 1 *and* Phase 3), which is why it is last — but note this is *only* the re-baseline. **The N2 code fix is Task 1**; an earlier draft of this plan conflated the two and buried a release blocker in Phase 4.

## Global Constraints

- **Branch:** all work lands on `bundler-redesign`. Never commit to `main`.
- **Commits:** one commit per *logically-coherent change*, NOT one per task or per step. Tasks that share a commit say so. This overrides the plan-template default (`CLAUDE.md` → Git Expectations).
- **Line endings:** LF only. **Package manager:** `pnpm` only.
- **Compiler-stack pins are exact and untouchable here:** `rolldown =1.1.5`, all OXC crates `=0.139.0`, `oxc_resolver =11.23.0`. **This plan adds no new crate dependency.** (This rules out the `oxc_str::Ident` route in Task 2 — see Task 2 Step 3.) Do not assert any *other* dependency's version in a test.
- **Testing policy:** Logic / Drift / Property / Guard only. No Echo tests — never write a test whose expected value you typed by hand out of the file under test (`CLAUDE.md` → Testing Policy).
- **SRS:** if behavior diverges from `docs/ImportLens-SRS.md`, update the SRS in the same task.
- **Verification:**
  ```powershell
  pnpm check
  pnpm test
  cargo fmt --check
  pnpm package:win32-x64
  ```
- **`ANALYZER_REVISION` is bumped exactly once, in Task 14**, after every measurement-affecting change has landed. Do not bump it per-task.

### Verified API facts (do not re-derive; checked against the vendored crates)

An earlier draft asserted several APIs from memory and was wrong. These are the corrected forms:

| Thing | Truth |
|---|---|
| `Scoping::symbol_id_for_span` | **DOES NOT EXIST.** Build a `Span → SymbolId` map from `Scoping::symbol_ids()` (`scoping.rs:336`) + `Scoping::symbol_span(id)` (`scoping.rs:344`). |
| `Scoping::get_root_binding` / `find_binding` | Take `oxc_str::Ident`. **`oxc_str` is not a daemon dep** — using these means adding a pinned compiler-stack crate. Forbidden. Use the span route. |
| `Semantic::scoping()` | Exists → `&Scoping` (`oxc_semantic/src/lib.rs:137`). |
| `Scoping::get_resolved_references(id)` | Exists → `impl DoubleEndedIterator<Item = &Reference>` (`scoping.rs:583`). |
| Type-only predicate | Real, copied from OXC's own `delete_typescript_bindings` (`scoping.rs:1028-1031`): `(flags.is_type() && !flags.is_value()) \|\| flags.is_value_as_type()`. `Reference::flags()` → `ReferenceFlags` (`oxc_syntax/src/reference.rs:312`). |
| `Semantic` in `daemon/src/document/` | **None is built today.** `imports.rs:39` parses and uses `parsed.module_record` only. Add: `SemanticBuilder::new().build(&parsed.program).semantic`. |
| Region TS-ness | `semantic.source_type().is_typescript()`, or `source_type_for_region()` (`script_regions.rs:51`). |
| `HookLoadReturn` | `Result<Option<HookLoadOutput>>`. **`HookLoadOutput.code` is `ArcStr`, not `String`** — use `code: source.into()`. |
| Returning source from `load` | **Does** suppress Rolldown's own disk read (`rolldown/src/utils/load_source.rs:23-50`: `is_read_from_disk = false`). Module type is still inferred from the extension, so a `.ts` file is still transformed — meaning the bytes we hand in are the *raw* bytes, which is exactly what we want to hash. |
| `.lock_unpoisoned()` | **Not a method.** A private free fn in `analysis_flight.rs:168`. In `plugin.rs` the pattern is `.lock().expect("…")` (`plugin.rs:44,57,65,197`). |
| `cache/key.rs` helpers | All exist, all `pub`: `content_hash` (`:16`), `read_time_len_mtime` (`:422`), `file_fingerprint_from_read_time` (`:442`, canonical-path debug assert at `:457-463`), `file_fingerprint_reading_hash` (`:408`, returns `Option`). `file_fingerprint_from_read_time` has **zero callers today**. |
| `BundlerOptions.experimental` | `Option<ExperimentalOptions>` (`rolldown_common/.../mod.rs:180`); `attach_debug_info: Option<AttachDebugInfo>` is public; `AttachDebugInfo::None` is real. Import via `rolldown::{AttachDebugInfo, ExperimentalOptions}` (glob re-export), **not** `rolldown_common::` root. |
| Unset `attach_debug_info` | Normalizes to `Simple` (`rolldown/src/utils/prepare_build_context.rs:285-287`). Debug comments are **on** today. |
| `RenderedModule::rendered_length()` | Sums `content().len()` over **every** source in the module's source vec (`rolldown_common/src/types/rendered_module.rs:36`) — and the region comments *are* sources. This is why N2 contaminates contributions, not just the raw total. |
| `ImportRuntime` | Derives `Hash`/`Eq` but **not `Ord`** (`ipc/protocol.rs:19-25`). `BTreeMap<ImportRuntime, _>` will not compile — add `PartialOrd, Ord` (no serialized-format impact) or use `HashMap` with deterministic iteration. |
| `FileSizeComputation` | Has **no `contributions` field** (`file_size.rs:16-25`). Contribution assertions go through `ImportResult`. |
| `OxcDiagnostic` | Implements `Display` (`oxc_diagnostics/src/lib.rs:227`), printing **only the message** (span/label dropped — intended). |
| `AnalysisError` | `stage: &'static str`, private fields, module-local → `error.stage.to_owned()` works in-module. |
| Two `ImportDiagnostic` types | `ipc::protocol::ImportDiagnostic` has `details`; **`engine::ImportDiagnostic` does not**. `adapter.rs`'s glob diagnostic uses the engine one. |

---

## Phase 0 — Prove the findings (no fix code)

Three findings are confirmed by code reading only. Phase 0 converts each into an **executable failing test**.

**Rule for a Phase-0 test that unexpectedly passes:** do **not** conclude the finding evaporated. W4 and N2 were reproduced live against real packages; I15 and R2 were confirmed statically by a second validation. A green repro test is far more likely to mean *the test does not drive the real code path*. So: if a Phase-0 test passes, **prove the test reaches production code** (break the production path deliberately and confirm the test goes red). Only if the test provably exercises the real path **and** still passes may the finding be struck — which requires editing the spec and saying so in the commit body.

---

### Task 0.1: Reproduce I15 — mixed-runtime file sizing returns a wrong aggregate

**Why:** `compute_file_size` takes the *first* resolved import's runtime and applies it to every entry ([file_size.rs:66-98](../../../daemon/src/pipeline/file_size.rs#L66)). An Astro file emits Server imports from frontmatter and Client imports from `<script>` blocks ([script_regions.rs:129-155](../../../daemon/src/document/script_regions.rs#L129)). Root entries resolve per-request correctly, but *transitive* resolution runs under the single mapped runtime, and Server vs Client differ materially (`alias_fields: ["browser"]` vs `[]`; browser vs node conditions). The mis-conditioned build **succeeds**, so no fallback fires.

**Files:** Test: `daemon/tests/file_size_runtime.rs` (create)

**Interfaces:** Consumes `compute_file_size(&AnalysisContext, &[ImportRequest]) -> FileSizeComputation` (no `contributions` field). **Produces the regression gate for Task 3.**

- [ ] **Step 1: Author the missing test helpers — most of them do not exist**

Read [`daemon/tests/common/mod.rs`](../../../daemon/tests/common/mod.rs) first. It exports only `temp_workspace`, `fixture_workspace`, `assert_parseable`, `assert_semantic_valid`, `assert_no_dangling_il_bindings`.

Of the helpers this plan's test code calls, **only `fixture_workspace` exists.** These must be authored:

| Helper | Status | Note |
|---|---|---|
| `fixture_workspace` | **exists** (`common/mod.rs:28`) | use it |
| `analysis_context` | **author** | |
| `named_request` | **author** | ⚠ A *private* `named_request(package, named)` exists in `daemon/src/pipeline/file_size_cache.rs:365` with **two args and no `ImportRuntime`**. Do not grep-and-reuse it — the plan's calls take three args. |
| `analyze_imports` | **exists in the product** — `document::analyze_imports(filename, source) -> Result<Vec<DetectedImport>, String>` ([document/mod.rs:14](../../../daemon/src/document/mod.rs#L14)). There is **no** `detect_imports` / `named_specifiers`. |
| `bundle_fixture` | **author** (or follow `candidate_matrix.rs`'s local `bundle_case` pattern) |
| `probe_cached_result`, `set_after_build_hook` | **author** (Task 0.2, in-crate) |

Add them to `common/mod.rs` where shared. An earlier draft of this plan said "add no new helper module" while calling seven helpers that do not exist.

- [ ] **Step 2: Build a fixture where runtime actually changes resolution.** Two **disjoint** packages (no shared modules → the expected aggregate is an exact sum):

```text
dual/package.json   { "name":"dual","version":"1.0.0","type":"module",
                      "main":"./server.js","browser":"./browser.js",
                      "exports":{".":{"browser":"./browser.js","node":"./server.js"}} }
dual/server.js      re-exports a LARGE module (~40 KB of exported consts)
dual/browser.js     exports one tiny const

plain/package.json  { "name":"plain","version":"1.0.0","type":"module","main":"./index.js" }
plain/index.js      exports one small function
```

- [ ] **Step 3: Write the failing test — assert EXACT equality.** A `>=` assertion is satisfied *by the bug* (the Server-conditioned `dual` is larger, so `mixed >= client_only` passes while the number is wrong):

```rust
#[test]
fn combined_file_size_sizes_each_entry_under_its_own_runtime() {
    let workspace = fixture_workspace("il-fs-runtime");
    write_dual_runtime_package(&workspace);
    write_plain_package(&workspace);
    let context = analysis_context(&workspace);

    let dual_client = compute_file_size(&context, &[named_request("dual", &["value"], ImportRuntime::Client)]);
    let dual_server = compute_file_size(&context, &[named_request("dual", &["value"], ImportRuntime::Server)]);
    let plain_server = compute_file_size(&context, &[named_request("plain", &["thing"], ImportRuntime::Server)]);

    // If the fixture does not make the runtimes resolve differently, this test
    // proves nothing. Fail loudly rather than pass vacuously.
    assert!(
        dual_server.raw_bytes > dual_client.raw_bytes * 2,
        "fixture is broken: server={} client={} — runtime must change resolution",
        dual_server.raw_bytes, dual_client.raw_bytes,
    );

    // CLIENT import first, SERVER import second. The packages share no modules,
    // so the correct aggregate is the exact sum of each entry sized under ITS OWN runtime.
    let mixed = compute_file_size(&context, &[
        named_request("dual", &["value"], ImportRuntime::Client),
        named_request("plain", &["thing"], ImportRuntime::Server),
    ]);

    assert_eq!(mixed.error, None, "mixed-runtime sizing must not fail");
    assert_eq!(
        mixed.raw_bytes,
        dual_client.raw_bytes + plain_server.raw_bytes,
        "mixed aggregate must size `dual` under Client and `plain` under Server; got {}, \
         expected {} + {}. Sizing every entry under the first import's runtime yields the \
         Server-conditioned `dual` instead (spec I15).",
        mixed.raw_bytes, dual_client.raw_bytes, plain_server.raw_bytes,
    );
}
```

- [ ] **Step 4: Run and record the three numbers.**

```powershell
cargo test -p import-lens-daemon --locked --test file_size_runtime -- --nocapture
```

Expected: **FAIL**. **STOP AND REPORT.**

---

### Task 0.2: Reproduce R2 — a stale size is served as fresh, and stays that way

**Why:** `dependency_fingerprints` re-reads every loaded path *after* the build ([service.rs:2380-2402](../../../daemon/src/service.rs#L2380)). A first-party file edited during the analysis window is stored with **new** bytes against an **old** size — and the entry is *sticky*, because every later probe compares new-to-new and answers `Fresh`.

**Files:** an **in-crate unit test** in `daemon/src/service.rs`'s `#[cfg(test)] mod tests` — **not** an integration test. **Produces the regression gate for Task 4.**

**Why in-crate, and why a seam is required.** The R2 window is *inside* one call: it opens when Rolldown reads a module and closes when the post-build `dependency_fingerprints` pass re-reads it ([service.rs:2380-2402](../../../daemon/src/service.rs#L2380)). A test that writes the new bytes **after** `analyze_with_cache` returns does **not** reproduce anything — the fingerprint already captured `hash(v1)`, the probe correctly sees Stale, and the test passes green on today's buggy code. (An earlier draft of this plan made exactly that mistake and asserted it would fail.) There is no way to hit the window from an integration test without winning a thread race, so **inject a deterministic seam**. `#[cfg(test)]` works here because a unit test in `daemon/src/` compiles *with* the lib — unlike `daemon/tests/`, which links the lib built without `cfg(test)` (this is the same constraint that defeats the `cfg(test)` limit override in Task 9).

- [ ] **Step 1: Add a test-only seam between the build and the fingerprint pass**

In `service.rs`, immediately before `dependency_fingerprints(...)`:

```rust
#[cfg(test)]
static AFTER_BUILD_BEFORE_FINGERPRINT: Mutex<Option<Box<dyn Fn() + Send>>> = /* … */;

#[cfg(test)]
if let Some(hook) = AFTER_BUILD_BEFORE_FINGERPRINT.lock().unwrap().as_ref() {
    hook();
}
```

This is the *only* production-file change Phase 0 makes, it is `cfg(test)`-gated, and it is what makes R2 reproducible at all. Keep it after the fix — it is what lets the Task 4 regression guard stay honest.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn a_size_measured_from_v1_is_never_served_as_fresh_once_the_dep_is_v2() {
    let workspace = fixture_workspace("il-r2-race");
    let dep = workspace.join("src/dep.ts");
    std::fs::write(&dep, b"export const v = 1;\n").expect("v1");

    // Rewrite the dep INSIDE the analysis window: after Rolldown read v1 and
    // measured it, before the post-build fingerprint pass reads the file again.
    set_after_build_hook({
        let dep = dep.clone();
        move || {
            std::fs::write(&dep, b"export const v = 1; export const w = 2222222222;\n")
                .expect("v2");
        }
    });

    let measured = analyze_with_cache(/* first-party entry, cold */);

    // The stored size describes v1. Under the bug the stored fingerprint describes v2,
    // so every later probe compares v2-on-disk against the v2 hash and answers Fresh —
    // serving the v1 size forever. The loop IS the stickiness assertion.
    for attempt in 0..3 {
        let served = probe_cached_result(&workspace);
        assert!(
            served.map_or(true, |entry| entry.raw_bytes != measured.raw_bytes),
            "probe {attempt}: a size measured from v1 is being served as Fresh while \
             dep.ts is v2; the fingerprint must describe the bytes that were MEASURED, \
             not the bytes on disk afterwards (spec R2)",
        );
    }
}
```

- [ ] **Step 3: Run.** Expected **FAIL** — the v1 size is served, repeatedly. If it passes, the hook is not firing where you think it is; verify the seam before concluding anything. **STOP AND REPORT.**

---

### Task 0.3: Measure the shipped cold path, old vs new

**Why:** *Every* performance claim in the spec is an unmeasured mechanism, and "best performant" is one of the two goals the redesign is judged against. §10.7 timed `RolldownEngine::bundle()` in isolation on a `num_cpus`-wide runtime; production runs it on a 2-thread runtime (I4) behind a fingerprint re-read (R2), a second full build (R4), and a canonicalize pass (I1). The recorded numbers do not describe production.

**Files:** Create `docs/superpowers/specs/2026-07-12-shipped-path-benchmark.md`.

- [ ] **Step 1: Benchmark the shipped path on this branch** — through `analyze_with_cache`, cold cache, release build. Three shapes: **cold single named import** (p50/p95), **20-import cold batch** (wall clock), **20-import all-hit batch** (wall clock — R7's surface). Record core count; R7 and I4 are core-count-sensitive.
- [ ] **Step 2: Benchmark the same three shapes at the merge base** (`git worktree add ../il-mergebase f4460fa`). Same fixtures, same machine, three runs, median.
- [ ] **Step 3: Record** raw numbers, exact commands, machine, one paragraph of interpretation. Do not round away a regression.

**Phase 1 will move this baseline** (Task 1 shrinks every chunk ~2%; Task 4 removes a full disk pass). **Task 14 re-takes it**; Phase 3 measures against *that*.

**STOP AND REPORT.** If the shipped cold path already beats the old engine end-to-end, Phase 3 drops from "reclaim a regression" to "optional headroom" and should be re-ordered by measured cost.

- [ ] **Step 4: Commit Phase 0** — one commit ("prove the findings"). Body states which reproduced.

---

## Phase 1 — Release blockers

Ordered by blast radius × severity. See the Ordering rationale table above.

---

### Task 1: N2 — stop counting Rolldown's debug comments as package cost

**Why this is first:** it is the **broadest** defect in the spec — every raw size, on every build, for every user — and it is a one-field fix. It also **unblocks everything downstream**: until it lands, every byte-asserting test and every benchmark in this plan is measured against contaminated output.

Rolldown normalizes an unset `attach_debug_info` to `Simple` (`prepare_build_context.rs:285-287`), and its renderer wraps **every non-empty rendered module** in `//#region <debug_id>` / `//#endregion`. `raw_bytes` is the chunk length, so those bundler-owned bytes are billed to the user's package. And `RenderedModule::rendered_length()` sums `content().len()` over **every** source in the module's source vec — and the region comments *are* sources — so **the module breakdown is contaminated too**.

Reproduced: `css-tree/parse` carried 6,389 debug bytes in a 326,844-byte raw chunk (~1.95%), with 123 contributions summing to 326,214 — a 630-byte gap, far smaller than the 6,389 debug bytes, proving the wrappers sit inside the contributions.

**This task does NOT re-baseline and does NOT bump `ANALYZER_REVISION`** — those are Task 14, after every other measurement-affecting change.

**Files:** `daemon/src/engine/adapter.rs:88-106`; tests in `daemon/tests/candidate_matrix.rs`.

- [ ] **Step 1: Write the failing Guard test**

```rust
#[tokio::test]
async fn matrix_49_no_debug_region_comments_in_production_chunk() {
    let artifact = bundle_fixture(/* multi-module fixture */).await;
    assert!(
        !artifact.code.contains("//#region"),
        "production chunk must not contain Rolldown debug region comments; they are \
         billed to the user as package cost and contaminate module contributions (N2)"
    );
    assert!(!artifact.code.contains("//#endregion"));
}
```

- [ ] **Step 2: Run it to verify it fails.** Expected: FAIL — the chunk contains `//#region`.

- [ ] **Step 3: Disable debug attachment explicitly.** Import via `rolldown::{AttachDebugInfo, ExperimentalOptions}` (the glob re-export), **not** `rolldown_common::` root:

```rust
// Rolldown normalizes an UNSET attach_debug_info to `Simple`, which wraps every
// rendered module in //#region/#endregion comments. Those bytes land in `raw_bytes`
// AND inside rendered-length module contributions, billing bundler metadata to the
// user as package cost (§8.1/§8.2, spec N2).
experimental: Some(ExperimentalOptions {
    attach_debug_info: Some(AttachDebugInfo::None),
    ..ExperimentalOptions::default()
}),
```

The comment states the one thing the code cannot show: that the *default* is not off.

- [ ] **Step 4: Add the cross-root determinism Property test.** The existing determinism check repeats requests under one root, so it cannot see path-dependent measurement. Bundle an identical fixture under two roots of different lengths and assert **every** size field matches. **`FileSizeComputation` has no `contributions` field** — assert contributions through `ImportResult` (`internal_contributions` / `module_breakdown`).

```rust
#[test]
fn size_fields_are_independent_of_workspace_root_path_length() { /* raw, minified, brotli, contributions */ }
```

This is a Property test: it fails for any future change that lets an absolute path leak into measured output.

- [ ] **Step 5: Run the full suite.** **No existing test needs its expected value updated** — a sweep of `daemon/tests` confirms every byte assertion is *relational* and N2-invariant (`raw_bytes > 0`, `dynamic.raw_bytes == namespace.raw_bytes`, `file_size.raw_bytes < summed_raw`, `contribution.rendered_bytes > 0`); there are **zero byte literals**, and `candidate_matrix.rs` does not reference `raw_bytes` at all. An earlier draft told the executor to re-baseline rows that do not exist — do not go looking for them. Do confirm **minified/gzip/brotli/zstd are unchanged** (the minifier already stripped the comments — which is exactly why the accuracy oracle never caught this). If a compressed figure moves, stop: something else changed.

**Safe to make this change:** `is_attach_debug_info_enabled()` has exactly two call sites in Rolldown 1.1.5, both in `render_ecma_module.rs:26,55` (the two comment sources). Everything else (`chunk_ext.rs:57`, `chunk_optimizer.rs:1114`, `ecma_generator.rs:261`) gates on `is_attach_debug_info_full()`, already false under today's `Simple`. So `None` changes the comments and nothing else — no chunking, ordering, or sourcemap effect.

- [ ] **Step 6: Commit.**

---

### Task 2: W4 — stop turning valid TypeScript type-position imports into hard zero-size errors

**Why:** `import { ParseOptions } from "commander"` — a type used only in a type position, written without the `type` keyword — is valid, compiling TypeScript. The detector only filters imports OXC marks explicitly `is_type` ([imports.rs:96-100](../../../daemon/src/document/imports.rs#L96)), so it reaches the engine as a runtime `Named` request; Rolldown correctly reports the runtime export missing; [analyze.rs:173](../../../daemon/src/pipeline/analyze.rs#L173) turns that into a hard error with **all size fields zero**, and the file's aggregate fails. Reproduced live against `commander@12.1.0`.

**The fix is at the detector.** When TypeScript erases the import, its true runtime cost is **zero** — so neither the old full-package number nor the new `0`-with-an-error was right.

**Scope guard — do NOT elide unused imports.** An earlier draft proposed eliding any import with zero references in a `.ts` file. That is **wrong**: under `verbatimModuleSyntax` / `isolatedModules` (the modern bundler-targeted default) TypeScript **preserves** a value import even when unused, and it has real runtime cost. Eliding it is a silent under-count — the exact failure the counter-tests below exist to prevent.

**Files:** `daemon/src/document/imports.rs` (`imports_from_region` ~`:32-75`; `imports_from_static_imports` `:87-130`); tests in `daemon/tests/document_analysis.rs`.

**⚠ THE TRAP THAT MAKES THIS FIX WORSE THAN THE BUG.** `imports_from_static_imports` has a **second loop** at [imports.rs:131-160](../../../daemon/src/document/imports.rs#L131) that walks `module_record.requested_modules` and, for any `(specifier, statement_span)` key **not already in `binding_imports`**, pushes a group with **`has_namespace: true`**. The key is inserted only when the first loop creates a group (`:123`).

So if you simply `.filter()` the `import_entries` iterator and `Kind` was the statement's only binding, the key is never inserted — and the second loop **re-adds the statement as a Namespace import of the entire package**. The fix would turn *"0 bytes + hard error"* into *"the whole package's bytes"*: **strictly worse than the bug.**

And the naive test would not catch it: a Namespace `DetectedImport` has `named: []`, so a `named`-only assertion sees `Kind` absent and passes **green while the bug ships**. The tests below assert the **group is gone entirely**, not merely that `named` lacks the binding.

That second loop is also what legitimately produces bare side-effect imports (`import "pkg"`, which has no `import_entries` at all) — so it cannot simply be gated off. Track the elided statements and skip **those keys only**.

- [ ] **Step 1: Write the failing tests (four)**

The real API is `document::analyze_imports(filename, source) -> Result<Vec<DetectedImport>, String>` ([document/mod.rs:14](../../../daemon/src/document/mod.rs#L14)), used that way in [document_analysis.rs:19](../../../daemon/tests/document_analysis.rs#L19). There is **no** `detect_imports` or `named_specifiers` helper — author them locally in the test file or call `analyze_imports` directly.

```rust
#[test]
fn type_position_only_named_import_is_not_a_runtime_import() {
    let source = r#"
import { Kind } from "pkg";
import { run } from "pkg";
const k: Kind = { a: 1 };
run(k);
"#;
    let imports = analyze_imports("a.ts", source).expect("analyze");

    // The `Kind` STATEMENT must be gone entirely — not merely stripped of its named
    // binding. If it survives as a namespace group, we size the whole package.
    assert!(
        !imports.iter().any(|i| i.named.iter().any(|n| n == "Kind")),
        "type-position-only binding must be elided",
    );
    assert_eq!(
        imports.iter().filter(|i| i.specifier == "pkg").count(),
        1,
        "the elided statement must not reappear as a namespace import of the whole \
         package (imports.rs:150-157); got {imports:?}",
    );
    let survivor = imports.iter().find(|i| i.specifier == "pkg").expect("value import");
    assert!(!survivor.has_namespace, "survivor must not be a namespace import");
    assert!(survivor.named.iter().any(|n| n == "run"), "value binding must survive");
}

#[test]
fn bare_side_effect_import_still_produces_a_group() {
    // The same loop that causes the trap above legitimately handles `import "pkg"`.
    // Suppressing elided statements must not suppress these.
    let imports = analyze_imports("a.ts", r#"import "pkg";"#).expect("analyze");
    assert_eq!(imports.len(), 1, "bare side-effect import must survive, got {imports:?}");
}

#[test]
fn binding_used_as_both_type_and_value_stays_a_runtime_import() {
    // A class is a type AND a value. Eliding it would silently UNDER-count.
    let source = r#"
import { Thing } from "pkg";
const t: Thing = new Thing();
"#;
    let named = named_specifiers(detect_imports(source, "ts"));
    assert!(named.contains(&"Thing"), "a binding used as a value must NOT be elided, got {named:?}");
}

#[test]
fn unused_import_is_still_a_runtime_import() {
    // Under verbatimModuleSyntax/isolatedModules TypeScript PRESERVES this and it has
    // real runtime cost. Eliding it is an under-count.
    let source = r#"
import { unused } from "pkg";
export const x = 1;
"#;
    let named = named_specifiers(detect_imports(source, "ts"));
    assert!(named.contains(&"unused"), "an unused VALUE import must not be elided, got {named:?}");
}
```

Tests 2 and 3 pass already (nothing is elided today) — that is correct; they are regression guards against the fix going too far.

- [ ] **Step 2: Run.** Test 1 FAILS (`Kind` present); 2 and 3 pass.

- [ ] **Step 3: Build a `Semantic` *only when needed*, and elide type-only bindings**

`daemon/src/document/` builds no `Semantic` today. `&parsed.program` and `&parsed.module_record` are both shared borrows of `parsed` and `Program<'a>` is covariant, so the borrow compiles — that OXC footgun does not bite here.

**But do not build it unconditionally.** `analyze_imports` runs per file in the workspace report, which drains over hundreds of files ([service.rs:491-497](../../../daemon/src/service.rs#L491)), and `SemanticBuilder` is typically 30–60% of parse cost. Gate it — `source_type` is already local at `imports.rs:38`:

```rust
// Only TypeScript has type positions, and only a region with at least one
// non-type named import can have anything to elide. A .js region pays nothing.
let type_only_spans = if source_type.is_typescript()
    && parsed.module_record.import_entries.iter().any(|entry| !entry.is_type)
{
    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    type_only_binding_spans(&semantic)
} else {
    HashSet::new()
};
```

`Scoping` has **no** `symbol_id_for_span`. Build the span map yourself:

```rust
/// A named import whose local binding is referenced only in type positions is erased
/// by TypeScript before any bundler sees it, so its runtime cost is zero (spec W4).
/// `import type` / `{ type X }` are already filtered by `is_type`; this covers the
/// legacy-elision form that omits the keyword.
///
/// A binding with NO references is NOT elided: under verbatimModuleSyntax a value
/// import is preserved even when unused, and eliding it under-counts.
fn type_only_binding_spans(semantic: &Semantic<'_>) -> HashSet<Span> {
    let scoping = semantic.scoping();
    scoping
        .symbol_ids()
        .filter(|symbol_id| {
            let mut references = scoping.get_resolved_references(*symbol_id).peekable();
            references.peek().is_some()   // no references → not type-only; see doc comment
                && references.all(|reference| {
                    let flags = reference.flags();
                    (flags.is_type() && !flags.is_value()) || flags.is_value_as_type()
                })
        })
        .map(|symbol_id| scoping.symbol_span(symbol_id))
        .collect()
}
```

**The span mapping is verified correct** (do not re-derive): `oxc_semantic`'s `impl Binder for ImportSpecifier` declares the symbol at `ident.span` where `ident` is `specifier.local`; `oxc_parser`'s module_record builds `ImportEntry.local_name = NameSpan::new(specifier.local.name, specifier.local.span)` — the **same** span. For `import { A as B }`, `specifier.local` is `B`, so both sides key on `B`. `Span` has `Hash`/`Eq`, so `HashSet<Span>` compiles.

Now filter the first loop **and suppress the elided statements in the second one** — this is the trap above:

```rust
let mut elided_statements = HashSet::<(String, u32, u32)>::new();

for entry in module_record.import_entries.iter().filter(|entry| !entry.is_type) {
    let specifier = entry.module_request.name.as_str();
    if !is_runtime_package_specifier(specifier) {
        continue;
    }
    let key = (specifier.to_owned(), entry.statement_span.start, entry.statement_span.end);

    if type_only_spans.contains(&entry.local_name.span) {
        // Remember the statement so the requested_modules loop below does NOT
        // resurrect it as a whole-package namespace import (imports.rs:150-157).
        elided_statements.insert(key);
        continue;
    }
    // … existing group creation, which inserts `key` into `binding_imports` …
}
```

and in the `requested_modules` loop, skip a key that was elided **and** never produced a surviving group:

```rust
if binding_imports.contains_key(&key) || elided_statements.contains(&key) {
    continue;
}
```

A statement with a mix of type-only and value bindings inserts into `binding_imports` via its surviving binding, so `contains_key` already covers it; `elided_statements` only matters when **every** binding of the statement was elided. A bare `import "pkg"` never enters the first loop at all, so it is untouched and still produces its group.

**Guard rails:** (1) TS-family documents only — including the TS regions of `.astro`/`.vue`/`.svelte`, which `source_type_for_region()` already classifies. (2) Never elide a binding with any value reference. (3) If a symbol cannot be resolved, do not elide. (4) A fully-elided statement produces **no lens on that line at all** — not a "0 B" lens. That matches today's `import type` behavior, so it is defensible, but state it in the SRS since W4's framing ("its true runtime cost is zero") implies a zero-byte lens.

- [ ] **Step 4: Run.** All three pass; pre-existing Astro/Vue region tests still pass.

- [ ] **Step 5: Verify against the live reproduction.** In a scratch workspace with `commander@12.1.0`: `import { ParseOptions } from "commander"` used only as a type no longer yields `missing_export` + zero sizes; a genuine typo used as a **value** still does.

- [ ] **Step 6: Record the decision on W4's aggregate half.** A genuine missing export still produces zero-everything **and fails the whole file's aggregate**. This task does not change [analyze.rs:173](../../../daemon/src/pipeline/analyze.rs#L173). Recommended: a typo in one import should not blank the file's size — make `compute_file_size` treat a failed import as a zero-cost entry with a diagnostic. **Implement in Task 3** (which already rewrites that function); record the decision here.

- [ ] **Step 7: Update the SRS** — type-position-only bindings are elided from runtime sizing in TypeScript documents, matching compiler elision semantics.

- [ ] **Step 8: Commit.**

---

### Task 3: I15 + I14 + I6a + W4's aggregate — rewrite `compute_file_size` once

**All four edit the same function.** An earlier draft split them across Phase 1 and Phase 3, guaranteeing a conflict. They land together.

**Why (I15):** proved by Task 0.1. Root entries resolve per-request correctly, but transitive resolution runs under one mapped runtime, so a Client `<script>` package's subgraph resolves with node conditions — undercounted, and the build **succeeds**, so nothing warns.

**Files:** `daemon/src/pipeline/file_size.rs:58-193`; `daemon/src/ipc/protocol.rs:19-25` (derive `Ord`); tests in `daemon/tests/file_size_runtime.rs`.

- [ ] **Step 1: I15 — group entries by runtime, one build per group**

`ImportRuntime` derives `Hash`/`Eq` but **not `Ord`**, so `BTreeMap<ImportRuntime, _>` will not compile. Add `PartialOrd, Ord` (no serialized-format impact) or use a `HashMap` with deterministic iteration — determinism matters for Task 1's cross-root property test.

```rust
// Server and Client entries resolve their transitive graphs under different conditions
// (browser alias fields, node vs browser export conditions), so one build per runtime is
// the only way each entry gets its real cost (spec I15).
let mut by_runtime: BTreeMap<ImportRuntime, Vec<BundleEntry>> = BTreeMap::new();
```

**`ImportRuntime` has THREE variants** — `Server`, `Client`, and **`Component`** (the `#[default]`, `protocol.rs:19-25`). Handle three groups, not two. Task 0.1's fixture exercises only two; that is fine for the repro, but the implementation must not assume the set is binary.

Build once per group; sum `raw_bytes` and `minified_bytes`; concatenate the per-group minified strings before compressing. **Preserve within-runtime dedup** — grouping keeps shared-module linking *within* each runtime, which is where it was ever true.

**Two things to get right about the compressed figures:**
- Concatenation is **safe**: `compress_all` ([pipeline/compress.rs:12-23](../../../daemon/src/pipeline/compress.rs#L12)) only compresses bytes — nothing re-parses, so colliding top-level declarations across independently-minified chunks are harmless.
- But compressing `minified_a + minified_b` yields a **smaller** number than two separate bundles would (repeated identifiers and runtime helpers dedupe inside the compressor's window), while `raw_bytes`/`minified_bytes` are honest sums. So gzip/brotli/zstd become a **lower bound**, not a sum. That is a defensible choice — say so in the SRS rather than claiming totals are "comparable to the single-build case."
- **Specify the join** (`\n` separator, or none). It moves `minified_bytes` by N−1 bytes, and a determinism test will notice.

**Single-runtime case (the common one):** one group → one build, byte-identical to today. No behavior change, no extra build. Confirmed against `file_size.rs:96-119`.

**Two honest caveats (the plan's claims, not the spec's):**
- Summing across groups means a package imported under **both** runtimes in one file is counted **twice**. Arguably correct (Server and Client code never share a chunk in the shipped product, so both copies really exist), but it is a behavior change. Add a test for this shape and state the choice in the SRS.
- The build-failure fallback must now be **per group** — one group failing must not discard the other's real numbers. (**W7**, the no-dedup fallback over-count, lives here too; grouping does not fix W7, but do not make it worse.)

- [ ] **Step 2: W4's aggregate half** (decision recorded in Task 2 Step 6). Treat a failed entry as a zero-cost entry carrying its diagnostic; keep sizing the rest.

- [ ] **Step 3: I14 — record per-entry loaded paths; do NOT skip**

`record_loaded_paths` writes the union of **all** entries' loaded paths under **every** entry key ([file_size.rs:121-123](../../../daemon/src/pipeline/file_size.rs#L121)), so editing package B invalidates another document's L1 signature for package A. It fails safe, but it is churn.

**Record per-entry subsets. Do not take the "skip `record_loaded_paths`" option** — the index is the only consumer feeding `first_party_module_token`, and skipping it **weakens L1 freshness for first-party imports** (the fallback then only stats the entry — see `file_size_cache.rs:34-37`).

- [ ] **Step 4: I6a — fast-path the single-import file.** A 1-import file currently does a redundant combined build after the per-import analysis already built the same package.

**There is no per-import artifact lying around to "reuse".** `compute_file_size` ([file_size.rs:58](../../../daemon/src/pipeline/file_size.rs#L58)) builds its own; `analyze_resolved_import` is called only on the fallback path (`:164`). The real fix is to route the single-import case through **`analyze_with_cache`** (i.e. the L2 entry, which may already be warm) and lift its size fields. If that is not viable, drop this step rather than fake it. (`I6a` is the plan's own label for the first half of spec **I6**; the remainder is Task 13.)

- [ ] **Step 5: Run.** Task 0.1's exact-equality test PASSES. Add the both-runtimes-same-package test from Step 1.

- [ ] **Step 6: Update the SRS** (FR-024a): entries are grouped by runtime and built once per group; a package imported under both runtimes is counted once per runtime.

- [ ] **Step 7: Commit.**

---

### Task 4: R2 (+ R6 + I1) — capture fingerprints at read time, and stat before reading

**Why (R2):** every cold miss re-reads and re-hashes the whole loaded graph right after Rolldown already read it; and because the hash is taken *after* measurement, a file edited during the analysis window is stored with **new** bytes against an **old** size, and the entry is **sticky**. The mechanism that closed this window is still present and is now **dead code**: `file_fingerprint_from_read_time` ([cache/key.rs:442](../../../daemon/src/cache/key.rs#L442)) has zero callers.

**Why R6 belongs here:** R6 says the 20 MiB per-module limit fires only *after* Rolldown reads and OXC parses the file. This task makes the plugin's `load` hook read real files **itself** — which, without a guard, moves the unguarded full-file read into *our own code* and makes R6 strictly worse. The `fs::metadata` stat is nearly free at exactly this point. **R6 is a step of this task, not a footnote.**

**Files:** `daemon/src/engine/plugin.rs` (`load` `:145-153`; `BuildState` `:29-34`); `daemon/src/engine/mod.rs` (`BundleArtifact` `:66-74`); `daemon/src/engine/adapter.rs:196-203`; `daemon/src/pipeline/analyze.rs:50-52`; `daemon/src/service.rs:2380-2402`; tests in `daemon/tests/freshness_read_time.rs`.

**Interfaces:** `FingerprintSource::ReadTime(Vec<FileFingerprint>)` replaces `LoadedPaths(Vec<PathBuf>)`. `BundleArtifact` gains `read_time_fingerprints: Vec<FileFingerprint>`. `BuildState` gains `read_time_fingerprints: Mutex<Vec<FileFingerprint>>`.

- [ ] **Step 1: Confirm Task 0.2's test is red for the right reason.**

- [ ] **Step 2: Decide I1's shape FIRST (it determines this step's code)**

Step 6 offers a choice between **(a)** a canonical-path memo used inside the hook and **(b)** canonicalizing on the caller thread after permit release. **Make that decision now** — an earlier draft hard-coded (a) in this step's sketch while presenting it as open in Step 6, so an executor choosing (b) would have built the fingerprints in the wrong place. Recommended: **(a)**, a *bounded* memo (an unbounded process-lifetime `HashMap<PathBuf, PathBuf>` is a leak; bound it or key it to the cache generation).

- [ ] **Step 3: Stat, then read OFF the async worker, in the `load` hook (R6 + R2 together)**

`load` is called for **every** id, including the virtual entry and Rolldown runtime helpers — gate on "id is an existing absolute path" before touching disk.

**Do NOT call `std::fs::read` directly in this `async fn`.** The engine runtime has **2 worker threads** ([boundary.rs:33-43](../../../daemon/src/engine/boundary.rs#L33)), and the read this replaces runs on Tokio's *blocking pool*, not a worker — Rolldown wraps it in `spawn_blocking` (`rolldown/src/utils/load_source.rs:87-90`). A blocking read inside the hook moves every module's disk read onto 2 async workers and **serializes the entire module-graph load two-wide** — plausibly a larger regression than the re-read pass this task removes. Use `tokio::fs` / `spawn_blocking`; the hook is already `async`, so this is free.

```rust
// Real file. Stat BEFORE reading: the 20 MiB module limit exists to bound memory,
// and reading first blows the bound we are enforcing (§7.3, R6).
let metadata = tokio::fs::metadata(&path).await?;
if metadata.len() as usize > MAX_MODULE_SOURCE_BYTES {
    // MUST go through the plugin's own breach API — `classify_failure`
    // (adapter.rs:242-249) calls `state.take_breach()` BEFORE classifying, so only a
    // recorded breach surfaces as `module_graph_limit`. A bare io::Error here would
    // land as stage `resolve` (UnloadableDependencyError, adapter.rs:294).
    return Err(self.breach(&format!(
        "module source exceeds {MAX_MODULE_SOURCE_BYTES} bytes: {}", path.display()
    )));
}

let bytes = tokio::fs::read(&path).await?;

// Binary modules (.wasm, dataurl/binary module types) are NOT valid UTF-8. Rolldown
// handles them as StrOrBytes::Bytes (load_source.rs:78-96). Hand them back to
// Rolldown rather than failing the build; Step 5 back-fills their fingerprints.
let Ok(source) = String::from_utf8(bytes.clone()) else {
    return Ok(None);
};

let (len, modified_millis) = read_time_len_mtime(&path);   // cache/key.rs:422
let hash = content_hash(&bytes);                            // cache/key.rs:16
let canonical = self.canonical_memo(&path);                 // from Step 2
self.state
    .read_time_fingerprints
    .lock()
    .expect("read-time fingerprints should not be poisoned")
    .push(file_fingerprint_from_read_time(&canonical, len, modified_millis, hash));

Ok(Some(HookLoadOutput {
    code: source.into(),   // ArcStr, NOT String
    module_type: None,     // let Rolldown infer from the extension
    ..Default::default()
}))
```

**Verified in scope already:** `MAX_MODULE_SOURCE_BYTES` (`plugin.rs:25`), `HookLoadOutput::default()` + `module_type` (already used at `plugin.rs:147-150`). `ImportLensPlugin::breach()` is at `plugin.rs:105-108`; `BuildState::record_breach` at ~`:62`. There is **no** `record_limit_breach` free function — an earlier draft invented it.

**Three load-bearing constraints:**
1. **`code` is `ArcStr`** — `source.into()`, not `String`.
2. `file_fingerprint_from_read_time` **debug-asserts its path is already canonical** ([cache/key.rs:457-463](../../../daemon/src/cache/key.rs#L457)).
3. The hash must be of the **raw disk bytes**. We hash before handing them over, and Rolldown still infers module type from the extension and transforms `.ts` itself — so the bytes we hash are pre-transform by construction. **Never hash `module_info.code`**; for `.ts` that is post-transform output.

- [ ] **Step 4: Reconcile the two limit checks (R6, second half)**

After Step 3 there are **two** per-module size checks with **different units**: the new `metadata.len()` (raw disk bytes) and the existing `module_info.code.len()` in `module_parsed` ([plugin.rs:171-179](../../../daemon/src/engine/plugin.rs#L171)) — which is *post-transform* source, and disagrees for `.ts` and for any file with a BOM. Two sources of truth for one limit is how R6 half-fixes itself.

**Decide and state:** make raw disk bytes the authority and delete the `module_parsed` per-module check, **or** keep both and document why. Either way, re-check that `matrix_33`'s fixture is still on the correct side of 20 MiB, and note that `total_source_bytes` ([plugin.rs:186](../../../daemon/src/engine/plugin.rs#L186)) still accumulates post-parse — Task 9's env override lands on that accumulator, so do not leave it shadowed.

- [ ] **Step 5: Thread the fingerprints out — WITHOUT dropping the manifest, the sort, or the dedup**

`translate` drains `BuildState::read_time_fingerprints` into `BundleArtifact`. Three things live in the code being replaced, and an earlier draft silently deleted all three:

1. **The root manifest.** [analyze.rs:292-293](../../../daemon/src/pipeline/analyze.rs#L292) appends `package_root.join("package.json")` to the loaded-path list. A `package.json` is **not a loaded module**, so it will never appear in `read_time_fingerprints`. Taking the read-time list alone **un-fingerprints the root manifest** — a straight regression, and it guts the thing Task 5 then tries to extend.
2. **`sort` + `dedup`** ([service.rs:2398-2399](../../../daemon/src/service.rs#L2398)). Dedup is not decorative: two ids can canonicalize to the same real path (symlinked workspace deps). The `Mutex<Vec<_>>` pushes arrive in nondeterministic concurrent order, so both are required.
3. **Back-fill for modules the hook did not read** (binary/non-UTF8, where Step 3 returned `Ok(None)`). `module_parsed` still records **every** module into `BuildState::loaded_paths` ([plugin.rs:194-202](../../../daemon/src/engine/plugin.rs#L194)), so diff the two sets and back-fill the remainder with `file_fingerprint_reading_hash`.

```rust
let mut fingerprints = match source {
    Some(FingerprintSource::ReadTime(read_time)) => {
        let mut fingerprints = read_time.clone();
        // Back-fill any loaded path the load hook did not read (binary modules).
        let hashed: HashSet<&str> = fingerprints.iter().map(|f| f.path.as_str()).collect();
        fingerprints.extend(
            artifact.loaded_paths.iter()
                .filter(|path| !hashed.contains(normalize_identity_path(path).as_str()))
                .filter_map(file_fingerprint_reading_hash),
        );
        fingerprints
    }
    // Static-fallback path has no graph, so it still reads. Correct — there is nothing
    // that was measured for it to be inconsistent with.
    None => vec![resolved.package_root.join("package.json"), resolved.entry_path.clone()]
        .into_iter()
        .filter_map(file_fingerprint_reading_hash)
        .collect(),
};

// The root manifest is not a module and never reaches the load hook. It is an INPUT to
// resolution, not a source of measured bytes, so hashing it post-build does not
// reintroduce R2's read-after-measure race (see Task 5's note).
fingerprints.extend(file_fingerprint_reading_hash(package_root.join("package.json")));

fingerprints.sort_by(/* path */);
fingerprints.dedup_by(/* path */);
```

The whole-graph `fs::read` pass disappears; what remains is one small hash per manifest plus a back-fill for the rare binary module.

**On cache-key stability — a hypothesis worth killing before someone raises it:** the nondeterministic push order does **not** destabilize cache keys. `CacheIdentity` ([cache/key.rs:39-50](../../../daemon/src/cache/key.rs#L39)) contains **no fingerprints** — it is analyzer version, specifier, package name/version/root, entry path, runtime, import kind, named exports. Fingerprints live in the cache *value* and are consumed order-independently by `check_fingerprints`. The sort is for dedup correctness and reproducible stored entries, not for key stability.

- [ ] **Step 6: Run.** Task 0.2's test PASSES; `freshness_core` does not regress.

- [ ] **Step 7: Re-home the two deleted freshness guards (R8, second half)**

The merge base's `fingerprints_capture_read_time_len_not_post_analysis_stat` and `module_graph_carries_content_hash_for_loaded_modules` were deleted because they named now-deleted symbols — but their *assertions* are about freshness, not the old engine. Re-home both as **Guard** tests:

```rust
#[test]
fn fingerprints_capture_read_time_len_not_post_analysis_stat() { /* … */ }

#[test]
fn ts_dependency_is_hashed_from_raw_disk_bytes_not_transformed_output() {
    // Fails if anyone ever hashes module_info.code — that is post-transform.
}
```

- [ ] **Step 8: Finish I1 — make the canonicalize single-source**

Step 2 chose the memo. Now remove the *second* canonicalize: **delete the one in `sorted_loaded_paths`** ([plugin.rs:41-53](../../../daemon/src/engine/plugin.rs#L41)) — canonicalizing twice is exactly what I1 is about.

**Consistency trap:** if `sorted_loaded_paths` stops canonicalizing, `module_parsed`'s recorded paths ([plugin.rs:200](../../../daemon/src/engine/plugin.rs#L200)) must go through the **same memo**. Otherwise `loaded_paths` becomes raw Rolldown ids while fingerprints are canonical, and `dependency_paths` / `first_party_module_token` ([file_size_cache.rs:214-226](../../../daemon/src/pipeline/file_size_cache.rs#L214)) start stat'ing non-canonical paths.

- [ ] **Step 9: Measure the latency effect — it can go either way**

Do **not** assume this task is a speedup. It removes an O(graph-bytes) post-build read pass, but it also moves every module's read from Rolldown's blocking pool into our hook. Even done correctly via `spawn_blocking` (Step 3), the read now happens inside the build permit rather than wherever Rolldown scheduled it. Re-run Task 0.3's **cold single-import** and **20-import cold batch** shapes and compare. If the net is a regression, say so and reconsider — the correctness fix stands on its own, but the plan must not claim a speedup it did not measure.

- [ ] **Step 10: Commit.** R2 + R6 + I1 + the re-homed R8 guards are one coherent change to the read path.

**Note a cross-task file collision:** this task extends `BundleArtifact` and edits `adapter.rs:196-203`. **Task 13's I6** also edits `adapter.rs:197` (`try_unwrap` the chunk `Arc`) and **deletes `BundleArtifact.exported_names`**. Different phases, so no conflict in practice — but this task's commit is the one under independent review, so flag it there.

---

### Task 5: R5 — fingerprint first-party manifests

**Why:** only the *root* `package.json` is added to the fingerprint set ([analyze.rs:290-293](../../../daemon/src/pipeline/analyze.rs#L290)). The plugin records only parsed source *modules*, so a first-party workspace dependency's `package.json` — consulted during resolution and side-effect classification — is never fingerprinted. Editing its `exports`/`type`/`sideEffects` changes the resolved graph while no fingerprinted path moves.

Narrower than it looks: a first-party dep's *source* files **are** fingerprinted, so editing its code is caught. What is missed is editing its *manifest*.

**Known interaction with Task 4 — acknowledge it, do not paper over it.** A `package.json` is not a loaded module, so it never passes through the `load` hook and has **no read-time fingerprint**. Manifests must therefore be hashed after the build via `file_fingerprint_reading_hash` — the read-after-measure shape Task 4 just removed for modules. That is acceptable *only* because a manifest is not the thing that was measured (it is an input to resolution, not a source of measured bytes), and the window is one small file rather than the whole graph. **State this in a code comment** so the next reader does not "fix" it back.

**Files:** `daemon/src/pipeline/analyze.rs:285-295`; test in `daemon/tests/freshness_read_time.rs`.

- [ ] **Step 1: Failing test.** Two first-party workspace packages; consumer imports the dep; analyze; edit **only** the dep's `package.json` (flip `sideEffects`); assert the entry is invalidated.
- [ ] **Step 2: Implement.** For each distinct **first-party** directory in the loaded set, walk up to the nearest `package.json` and add it. Skip anything under `node_modules` — the install-generation gate covers those.
- [ ] **Step 3: Run, commit.**

---

### Task 6: I11 + I10 + I13 + I8 — make the diagnostics tell the truth

One coherent commit: everything here is about what the user is *told*. These are false **messages**, not wrong **numbers** — which is why they rank below Tasks 1–5.

**Files:** `daemon/src/pipeline/analyze.rs` (`:340-372`, `:544-548`); `daemon/src/pipeline/minify.rs:22,38`; `daemon/src/document/imports.rs:48`; `daemon/src/engine/adapter.rs:180-193`; `daemon/src/pipeline/resolver.rs:681-689`; tests in `daemon/tests/candidate_matrix.rs`.

- [ ] **Step 1: I11 — preserve the failure stage.** `engine_error` carries the stage into `AnalysisError.stage`, and `engine_fallback_diagnostic` then throws it away, hard-coding `stage: "engine_fallback"`. **Every** fallback-eligible failure — `parse`, `resolve`, `link`, `generate`, `output_shape`, `module_graph_limit`, OXC `minify` — surfaces under one label, violating three rows of §12's failure table.

```rust
ImportDiagnostic {                       // ipc::protocol::ImportDiagnostic (has `details`)
    stage: error.stage.to_owned(),       // &'static str → String
    message: format!("{}; used static fallback sizing", error.message),
    details: error.details.clone(),
}
```

- [ ] **Step 2: I11 — the Property test, enumerated from the source of truth.** Do **not** write `for stage in ["parse", "resolve", …]` — a literal list typed out of `analyze.rs` is exactly the banned Echo. Enumerate exhaustively from the engine's failure type, so **adding a new failure variant without plumbing its stage fails to compile or fails the test**. If the stages are `&'static str` rather than an enum, introduce an enum (or a `const` slice owned by the engine) so the test has a real source of truth to quantify over — that refactor is part of this step.

- [ ] **Step 3: I11 — delete the stale confidence text.** [analyze.rs:544-548](../../../daemon/src/pipeline/analyze.rs#L544) says *"Package side effects require full-graph sizing instead of named-export tree shaking."* That describes the **deleted** engine — the Rolldown path builds the same named-selection virtual entry regardless of `sideEffects`. Reword to what is true.

- [ ] **Step 4: I10 — stop Debug-formatting OXC diagnostics into user messages.** `minify.rs:22`/`:38` do `format!("{error:?}")` and that string reaches the tooltip; §5.1 bans unstable debug representations. `OxcDiagnostic: Display` — use `format!("{error}")` (prints message only; span/label dropped, which is the intent). **The same `{error:?}` pattern also exists at [imports.rs:48](../../../daemon/src/document/imports.rs#L48)** — fix it too (the spec missed this one).

- [ ] **Step 5: I13 — say the size may be *undercounted*.** Current text — *"matched paths unavailable / confidence conservative"* — reads as an *over*-estimate. The truth is the opposite: on Windows, Rolldown 1.1.5 never matches glob `sideEffects` (backslashed relative paths — see the `#[ignore]` reasons on matrix rows 42/43), so effectful files are **over-shaken** and the size is too **small**. Also fix the scope: the diagnostic is **not** platform-conditioned today (it fires everywhere) and fires **only** for the array form. On Windows, state "size may be undercounted: bundler cannot match glob `sideEffects`", and drop confidence to **Low** for glob/string `sideEffects` on Windows. Note this diagnostic is built in `adapter.rs` and uses **`engine::ImportDiagnostic`, which has no `details` field**.

- [ ] **Step 6: I8 — normalize string-form `sideEffects`.** `Some(Value::String(_))` falls into `SideEffectsMode::Unknown` ([resolver.rs:681-689](../../../daemon/src/pipeline/resolver.rs#L681)), so `"sideEffects": "./x.js"` gets no glob diagnostic at all while suffering the identical Windows undercount. §7.4 names string form as a first-class case. Normalize string and array into the same conservative reporting/confidence metadata — **without** locally deciding which files match. Do **not** route the string through the existing custom matcher: Task 15 (I9) may delete it, and adding a caller makes it harder to remove.

- [ ] **Step 7: Note the W2 interaction** — this changes a user-visible confidence level (W2 documents the current Medium for array-`sideEffects`; Step 5 drops it to Low on Windows). Say so in the commit body.

- [ ] **Step 8: Update the SRS** — confidence levels and diagnostic wording are user-visible behavior.

- [ ] **Step 9: Commit.**

---

### Phase 1 gate

```powershell
pnpm check; pnpm test; cargo fmt --check
```

- [ ] **Re-take the benchmark.** Phase 1 moved it: Task 1 shrank every chunk ~2%, and Task 4 changed where module reads happen. Re-run Task 0.3's three shapes and append to `docs/superpowers/specs/2026-07-12-shipped-path-benchmark.md`. **This is the baseline Phase 3 measures against** — it lives here, not in Task 14, because Phase 3 needs it *before* Task 14 runs. (Task 14 re-takes it once more at the end, for the §10.7 record.)

Note this also makes Task 0.3's STOP-AND-REPORT verdict provisional: it was taken on pre-N2, pre-Task-4 numbers by construction. Re-decide Phase 3's scope from *this* baseline.

**Independent review checkpoint.** Phase 1 touches the cache format, the measurement path, the plugin's disk-read path, and a user-visible behavior change (W4). Per `CLAUDE.md`, dispatch a fresh reviewer over the staged Phase 1 diff. Treat findings as hypotheses: reproduce each, fix what confirms, decline the rest with a one-line reason.

---

## Phase 2 — Regression guards

A fix with no gate is a fix that regresses. These come immediately after the blockers.

---

### Task 7: N1 — make fixture preparation reproducible

**Why it is before Task 8:** Task 8's CI job runs `prepare-candidate-fixtures.mjs`. If the preparer is unreliable, the gate is unreliable.

`prepare-candidate-fixtures.mjs` defaults to a **fixed** temp dir, `mkdir -p`s it, and runs `pnpm install --frozen-lockfile` — but never recreates or integrity-checks the target. An interrupted prior install leaves a `node_modules` pnpm considers current; the script exits 0 with packages missing (observed: `lodash-es`, `react`, `uuid` absent while pnpm reported the install current).

- [ ] Prepare into a unique directory, **or** remove and recreate `node_modules` before the frozen install and verify the seven package entry files exist before printing the path.
- [ ] Commit.

---

### Task 8: R3 — run the real-package correctness gates in CI

**Why:** all 7 rows in `daemon/tests/candidate_packages.rs` are `#[ignore]` + fixture-gated, and nothing automated installs the fixtures or passes `--ignored`. The spec's named gates — "the four css-tree danglers reach zero", "`loaded_paths` includes tree-shaken dependencies" — exist only in a suite that never runs. **The whole redesign's justification is a correctness win that no automated gate protects.**

The supposed mitigation is weaker than it looks: CI's accuracy check defaults to a **75%** tolerance (`IMPORT_LENS_ACCURACY_TOLERANCE ?? "0.75"`) against observed deltas of 2.6–13.0%. A dangler regression would have to move brotli size by >75% to trip it.

- [ ] **Step 1:** Add a `validate.yml` job that runs `node scripts/prepare-candidate-fixtures.mjs` (committed lockfile, registry access), exports `IMPORT_LENS_FIXTURES_WORKSPACE`, and runs `cargo test -p import-lens-daemon --locked --test candidate_packages -- --ignored`. Default it **on** for `ci.yml`.
- [ ] **Step 2: Tighten the accuracy tolerance — with a justified number.** It must bracket the observed 2.6–13.0% range with enough headroom that a *legitimate* compiler-stack bump does not turn CI red for a non-bug. Set it from the observed worst case plus a stated margin (e.g. `0.25` — worst observed 13.0%, doubled and rounded), **record the derivation in a comment**, and keep the env override. Do not pick a number without writing down why.
- [ ] **Step 3: Prove the job can fail.** Temporarily break a dangler assertion and confirm the job goes red. **Do not skip this** — a green gate that cannot fail is the exact defect being fixed.
- [ ] **Step 4: Commit.**

---

### Task 9: R8 — make the total-source-limit branch runnable

`matrix_34_total_source_limit` is the **only** coverage of the `MAX_GRAPH_SOURCE_BYTES` accumulator and is `#[ignore]`d (writes >100 MiB). Rows 32/33 cover the count and per-module branches, so the limit is only partially live (§10.4 requires all graph limits observable). (R8's other half — the two deleted freshness guards — is re-homed in Task 4 Step 5.)

**The `cfg(test)` route does not work** — `candidate_matrix.rs` is an *integration* test and links the daemon lib compiled **without** `cfg(test)`, so a `#[cfg(test)]` constant has no effect on it. **The cargo-feature route also fails** (feature unification across the integration test's lib build). **Mandate an env-var override** read by `daemon/src/engine/limits.rs`, so the test can shrink `MAX_GRAPH_SOURCE_BYTES` and run the branch by default without a 100 MiB fixture.

**Mechanical trap:** `MAX_GRAPH_SOURCE_BYTES` is currently used in **inline format args** (`plugin.rs:175,189` — `format!("{MAX_GRAPH_SOURCE_BYTES}")`). Turning it into a `LazyLock`/fn breaks those call sites; they need explicit arguments.

**Interaction with Task 4:** Task 4 Step 4 decides the fate of the *per-module* check. `total_source_bytes` ([plugin.rs:186](../../../daemon/src/engine/plugin.rs#L186)) — the accumulator this task covers — still sums post-parse `module_info.code.len()`. Confirm that is still the intended authority before wiring the override to it.

- [ ] Implement, un-`#[ignore]` row 34, run, commit.

---

## Phase 3 — Performance

**Nothing here blocks release** — no measured regression exists (that is what Task 0.3, re-taken at the Phase 1 gate, establishes). **Read the Phase-1-gate baseline before starting.** If the shipped cold path already beats the old engine end-to-end, re-order this phase by measured cost and say so.

---

### Task 10: R4 — stop rebuilding the whole package once per named import

For every non-side-effectful `Named` import, `truly_treeshakeable` runs a second `bundle_sync(BundleSelection::Full)` + a second full minify and uses only its `len()` ([analyze.rs:248-278](../../../daemon/src/pipeline/analyze.rs#L248)). The cache key includes the named set, so single-flight cannot coalesce them. For **N** named variants of one entry/runtime the shipped path does **2N complete Rolldown builds**. The old engine cached the graph once and memoized one full-package comparison in a `OnceLock` on it — so this understates the regression vs. the old engine, not just vs. the new baseline.

Scope (verify before relying on it): gated on `!side_effects`, and `side_effects` is true for `Missing | Unknown | True | is_array()` — so it fires for packages explicitly declaring `"sideEffects": false`, i.e. the popular tree-shakeable set (lodash-es, date-fns, zod). Large blast radius, not literally every named import.

**Fix:** memoize the full-package minified length per `(entry_path, runtime)` behind the same freshness fingerprints, or single-flight it on package identity.

---

### Task 11: R7 + I2 — stop serving cache hits two-wide

`handle_batch`, `handle_batch_streaming`, `handle_file_size`, **and the workspace-report builder** wrap the whole `analyze_with_cache` call — *including the cache-hit path* — in `drain_ordered`, capped at `ENGINE_PERMITS = 2`. §9 only requires *builds* be limited to 2. The report path is the biggest surface and the spec barely mentions it: [service.rs:491-497](../../../daemon/src/service.rs#L491) drains over **files**, so file reading, parsing, and import detection all run 2-wide.

**The pattern already exists in-repo:** `analyze_package_json` classifies hits pool-wide (`par_iter`, [service.rs:1296](../../../daemon/src/service.rs#L1296)) and drains only misses 2-wide. Port it to all four handlers. Fold in **I2** by draining with `permits + k` workers so post-build work (minify/compress/fingerprint/insert — which runs *after* the permit is released) stops occupying the workers that would submit the next build. I2's real shape: it is *this drain* failing to refill released permits, not the permits sitting globally idle (other daemon work can take them).

**Assert no numeric claim.** Measure before and after.

---

### Task 12: I4 — benchmark and set the engine runtime width

The engine runtime is built with `worker_threads(ENGINE_PERMITS)` = **2** ([boundary.rs:33-43](../../../daemon/src/engine/boundary.rs#L33)), capping Rolldown's *internal* parallelism — while `candidate_performance.rs` measured on a default `num_cpus` runtime. **The §10.7 performance record therefore does not describe production**, which also puts a caveat on Section B4's "~2× faster per build". Permits bound memory; runtime width need not equal permits.

- [ ] Raise runtime workers to `min(num_cpus, 8)` keeping `ENGINE_PERMITS = 2`, and **re-measure**. Keep only if it wins; record the numbers either way. This is a benchmark, not an assumption.

---

### Task 13: I5 + I3 + I6 remainder + I7 + I12 + I17

- [ ] **I5 — prewarm's default-export probe.** `exposes_default_export` runs a full `enumerate_exports_sync` engine build **per dependency**, serially, before real prewarm starts. Replace with a single-file OXC parse for `export default` (conservative `true` on re-export stars), or drop the probe and let the Default job fail once.
- [ ] **I3 — priority lane.** Background prewarm, SWR revalidation, and default-export probes share the same 2 FIFO permits as interactive misses ([boundary.rs:24](../../../daemon/src/engine/boundary.rs#L24)). Split an interactive lane from a background lane (background capped at 1). **Do this last** — its benefit largely evaporates once R4 (Task 10) and I5 stop flooding the queue. Re-measure first; drop it if the queue is no longer contended.
- [ ] **I6 remainder.** (a) Bounded export-list cache for `enumerate_exports`, keyed on entry stat token + runtime — it is an **uncached full build per completion request** today. The spec's original rationale was **wrong**: an ImportSize build's `exported_names` are the virtual entry's positional aliases (`__il_entry_0_export_0`), not real export names, so there is **nothing free to reuse**. (b) `BundleArtifact.exported_names` has **no production reader** — delete or narrow it. (c) `try_unwrap` the chunk `Arc` instead of the full-chunk `String` clone at [adapter.rs:197](../../../daemon/src/engine/adapter.rs#L197).
- [ ] **I7 — enumeration drops success warnings.** Return `(Vec<String>, Vec<ImportDiagnostic>)`. **Requires amending §5** of the design doc (the trait has no diagnostics on the `Ok` side, while §8.4 wants them) — do the amendment in the same commit. Mitigating: Rolldown reports missing/ambiguous exports as *errors*, which already reach the user; only true warnings are lost.
- [ ] **I12 — builtin list.** Add `trace_events`/`wasi` in bare and `node:` forms; model `node:test`, `node:test/reporters`, `node:sqlite`, `node:sea` as prefix-only externals.
- [ ] **I17a** — contribution paths use Rolldown's raw id while `loaded_paths` are canonicalized; normalize the display spelling.
- [ ] **I17b** — the SWR revalidation `spawn_blocking` task ([ipc/server.rs:900-923](../../../daemon/src/ipc/server.rs#L900)) is detached and can write after the shutdown flush. Join or cancel it on shutdown.

---

## Phase 4 — Re-baseline, decisions, release

---

### Task 14: Re-baseline every measurement and bump `ANALYZER_REVISION`

**Why last:** every measurement-affecting change must have landed first. Task 1 changed raw sizes and contributions; Task 2 changed which imports are sized; Task 3 changed aggregates; Phase 3 changed timings. Re-baselining earlier would produce numbers that go stale within a task.

- [ ] **Step 1: Re-take the benchmark** (Task 0.3's three shapes) for the §10.7 record. Phase 3 already measured against the **Phase-1-gate** baseline; this run captures the final post-Phase-3 numbers.
- [ ] **Step 2: Re-baseline §10.7** in `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` — raw package figures **and** module-contribution figures **and** the performance record. Note in the commit body that the previously-recorded raw numbers were inflated ~2% by debug comments.
- [ ] **Step 3: Bump `ANALYZER_REVISION` — once, here.**

```rust
pub const ANALYZER_REVISION: &str = "rolldown2";
```

Every measured value changed across Phase 1. One bump on an unreleased branch covers them all. **The L1 file-size cache is signature+TTL based and is NOT gated by `ANALYZER_REVISION`** — confirm it invalidates on its own signature, or invalidate it explicitly.

- [ ] **Step 4: Commit.**

---

### Task 15: I9 — resolve the §15 cutover-deletion gate (a decision, with a recorded outcome)

**Why this is not optional:** §15 lists the "package-side-effect matcher/override" among required cutover deletions, and the matcher still lives in [resolver.rs:44-67, 739-825](../../../daemon/src/pipeline/resolver.rs#L44). The spec is explicit: *"the Section F claim that all §15 cutover-deletion bullets are done is **false** unless the matcher is removed or the design is amended."* Section F's row reads **partial**. Leaving this as a deferral is how a stated release gate lapses.

Runtime severity is genuinely low — the matcher never reaches Rolldown, so retention and measured size are untouched; it only drives the static-fallback `side_effects` flag and the matched-path diagnostic. But the gate is either closed or explicitly amended.

- [ ] **(a) Delete it** — reduce the static-fallback diagnostic to the conservative message and find a replacement source for that path's `side_effects` flag. Blocker: §7.4's "matched paths available through public metadata" is **not** satisfied by Rolldown 1.1.5, so the fallback loses fidelity.
- [ ] **(b) Amend the design** — add an explicit divergence to §7.4/§14.6/§15 accepting a reporting-only, retention-neutral matcher in the product resolver, and update Section F's row to **done (with recorded divergence)**.

Recommend **(b)**. Either way `docs/` changes and Section F is updated. **Record the decision in the commit body.**

---

### Task 16: W5 — decide the named-CJS typo regression

The old `cjs_scan` emitted *"named CommonJS export(s) not found"*; it is gone, because Rolldown's interop exposes a CJS entry's exports as `["default"]` only, leaving no name set to validate against. W5 is the **only** Section-E row marked `needs-decision` with **no** §10.7/SRS sanction.

**The connection the spec does not draw:** this is the same mechanism that makes CJS packages **immune to W4**. The synthetic namespace that costs us CJS typo detection is exactly what stops Rolldown raising `missing_export` on CJS named imports. The two rows are one trade — say so wherever this is documented.

- [ ] **Decide and record:** accept the lost lint (documenting it in the SRS as a known limitation, noting the W4 trade), or restore a conservative named-CJS check (a single-file OXC scan of the CJS entry's `exports.*` assignments, warning only — never affecting size). Recommend accepting: the old warning came from a regex-grade scan, and re-adding a bespoke scanner reintroduces exactly the hand-rolled analysis the cutover removed.

---

### Task 17: R1 — the panic profile decision

`[profile.release] panic = "abort"` ([Cargo.toml:13](../../../Cargo.toml#L13), with `opt-level="z"`, `lto`, `strip`). Under `abort`, the daemon's isolation layers cannot recover a request: the `catch_unwind` wrappers ([service.rs:445](../../../daemon/src/service.rs#L445), [:536](../../../daemon/src/service.rs#L536)), the `spawn_blocking`→`JoinError` mapping, single-flight leader-panic recovery (a `Drop` guard in `analysis_flight.rs`), and semaphore RAII all require unwinding.

**The trap worth naming:** Cargo ignores `panic=abort` for **test** targets, so the suite exercises all that recovery code under unwind and passes — proving behavior the shipped binary does not have.

Not a confirmed redesign regression (it predates the branch; no panic reproduction was found; unwind would not recover stack overflow, OOM, or an explicit abort; the extension already restarts a crashed daemon with backoff and goes unavailable after 3 crashes in 60s). But it is an open decision.

- [ ] **Decide and record:** flip to `panic = "unwind"` and **measure** the binary-size cost against the VSIX cap (`scripts/assert-vsix-size.mjs`, 20 MB — `strip` removes symbols but **not** unwind tables), or keep `abort` and annotate the four unwind-only recovery sites as dead-under-abort. **Do not ship them silently claiming isolation.** Folds in **I16** (the `dependency_paths` lock `.expect`s on poison where `analysis_flight` recovers): under `abort` a lock can never *become* poisoned, so align it only if unwind is chosen.

---

### Task 18: Release mechanics

- [ ] **Six-target packaging** — `pnpm package:win32-x64` and the other targets.
- [ ] **Daemon-hash refresh** — `extension/src/daemon/knownHashes.generated.ts`. The committed hash predates the cutover and cannot match a Rolldown-era binary.
- [ ] **VSIX size check** — 20 MB cap; headroom was ample pre-cutover but is **unverified** for a binary that now links Rolldown.
- [ ] **Update Section F** of the findings spec: every row now has an outcome.

---

## Self-review

- **Coverage — every spec finding has a home.** N2→T1. W4→T2 (+T3 aggregate). I15→T3. I14→T3. I6a→T3. R2→T4. R6→**T4 Step 2**. I1→T4 Step 6. R8→T4 Step 5 (guards) + T9 (row 34). R5→T5. I11→T6. I10→T6. I13→T6. I8→T6. N1→T7. R3→T8. R4→T10. R7→T11. I2→T11. I4→T12. I5/I3/I6-remainder/I7/I12/I17→T13. I9→T15. W5→T16. R1→T17. I16→T17. W1/W2/W3/W6→sanctioned, no action (W2's confidence change noted in T6 Step 7). W7→T3 Step 1 (do not worsen). Section F rows→T14/T15/T17/T18.
- **Ordering:** Phase 1 is strictly the user-visible-wrong-output set, ranked by blast radius × severity (N2 → W4 → I15 → R2 → R5 → diagnostics). The N2 *fix* is Task 1; only the *re-baseline* is deferred to Task 14. Guards (Phase 2) follow immediately, with N1 before R3 because R3's CI job depends on it. Perf (Phase 3) blocks nothing.
- **Single-owner invariants:** T3 is the only rewrite of `compute_file_size`. T4 is the only change to the `load` hook. T14 owns the only `ANALYZER_REVISION` bump and the only **§10.7 doc** re-baseline (the *benchmark* is re-taken at the Phase-1 gate, which is what Phase 3 measures against). T4 and T13 both touch `adapter.rs`/`BundleArtifact` in different phases — declared in T4.
- **Cross-references checked:** Task 0.1 → gate for **Task 3**. Task 0.2 → gate for **Task 4**. (An earlier draft had these off by two.)
- **`I6a` is this plan's label**, not the spec's — it is the 1-import-redundant-build half of spec **I6**; the remainder is T13.

### Defects fixed in the third revision (from independent validation)

Recorded so they are not silently reintroduced:

1. **W4 would have been worse than the bug.** Filtering `import_entries` alone lets [imports.rs:150-157](../../../daemon/src/document/imports.rs#L150) resurrect the statement as a **whole-package namespace import**, and a `named`-only assertion would have passed green while it shipped. Fixed with `elided_statements` suppression + a group-count assertion + a bare-side-effect-import guard test.
2. **The R2 repro could not fail.** It wrote v2 *after* the analysis returned, outside the race window. Fixed with a `cfg(test)` seam between the build and the fingerprint pass, and moved in-crate (integration tests cannot see `cfg(test)`).
3. **Task 4 would have serialized every module read two-wide.** `std::fs::read` inside the `async fn load` moves reads off Rolldown's blocking pool onto the 2-thread engine runtime. Fixed with `tokio::fs`, and Step 9 now treats the latency outcome as a measurement that can fail rather than an assumed win.
4. **Task 4 dropped the root manifest, the sort, and the dedup** from the fingerprint set, and would have hard-failed on binary modules. All four restored/handled.
5. **The N2-first rationale was false** (no byte-literal tests exist), and Task 1 told the executor to re-baseline matrix rows that do not exist.
6. **Seven of eight test helpers did not exist**, while Step 1 said "add no new helper module."
7. **`ImportRuntime` has a third variant (`Component`)** — the I15 grouping is not binary.
8. **Fabricated APIs:** `record_limit_breach`, `canonical_path_memo`, `detect_imports`, `named_specifiers`. Real ones named.

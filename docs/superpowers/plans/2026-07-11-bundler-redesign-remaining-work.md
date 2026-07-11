# Bundler Redesign — Remaining Work Master Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repository override:** per CLAUDE.md, commit by LOGICAL CHANGE, not per task — the commit boundaries are marked explicitly below and win over any per-task commit habit. Implement inline (hybrid-execution default); dispatch an independent read-only reviewer over the staged diff at every commit marked **[review]** (they all touch the release path or measured output), verify each finding against the code, fix confirmed ones, decline the rest with one-line reasons.

**Goal:** Finish the bundler redesign end to end: qualify the Rolldown 1.1.5 candidate engine, integrate it behind the stable engine contract, cut over atomically, delete the custom semantic bundler and its tests, and re-baseline the release — per `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` (the spec; user-approved 2026-07-10).

**Architecture:** A feature-gated candidate engine (`rolldown-candidate`) wraps the published Rolldown Rust crate behind Import Lens-owned request/artifact/failure types (spec §5). A native plugin resolves pre-resolved entry targets and enforces product limits; Rolldown owns all linking/tree-shaking semantics. After the §10 gates pass, the engine becomes production behind an async two-permit boundary, and the old engine (bundle.rs/reachability.rs/cjs.rs/manual graph) is deleted atomically with an `ANALYZER_REVISION` bump.

**Tech Stack:** Rust (tokio, feature-gated rolldown =1.1.5), OXC 0.139.0 for validation/minification, Node test scripts, pinned real-package fixtures.

## Context snapshot (read this first after a memory compaction)

- **Branch:** `bundler-redesign` (never commit to `main`). Landed: `a77a02e` (spec + compiler-stack plan, squashed by the user into one `docs(daemon)` commit — prefer that shape for docs) and `0c26830` (compiler-stack workflow: exact `=` pins, optional `rolldown = "=1.1.5"` behind non-default feature `rolldown-candidate`, `pnpm deps:update:compiler`, generated `scripts/compiler-stack.fingerprint.json` + drift tests, `deps:update:safe` restoration, `--locked` on every non-update cargo entry point, skill renamed to `compiler-stack-upgrade`, SRS dependency policy updated).
- **Production behavior is UNCHANGED so far.** The shipped daemon never enables `rolldown-candidate`; the custom bundler (`daemon/src/pipeline/{bundle,reachability,cjs,graph}.rs`) is still the engine. `ANALYZER_REVISION` is `"graph2"` at `daemon/src/cache/key.rs:24` — do NOT bump it until Part D.
- **Verified facts** (2026-07-10/11): rolldown 1.1.5 + oxc 0.139.0 + oxc_resolver 11.23.0 resolve as one graph (the committed fingerprint lists 52 reachable oxc*/rolldown* packages); the candidate feature compiles on win32-x64; `Bundler`/`BundlerBuilder`/`BundlerOptions`/async `generate()`/`module_parsed` hook/`HookSideEffects`/`OutputChunk.exports`/`RenderedModule::rendered_length()`/`treeshake`/`preserve_entry_signatures`/`external`/`resolve.{condition_names,main_fields}` all exist in 1.1.5; there is NO `inline_dynamic_imports` option — the knob is `code_splitting` (`CodeSplittingMode`); `generate()` takes no per-call options, so raw+minified requires OXC minification of the one unminified chunk (as designed, spec §8.1). All four spec §2.2 defects still reproduce on the current engine at `f4460fa`; css-tree/`parse` has 4 remaining dangling bindings.
- **Key commands:** `pnpm check`, `pnpm test`, `cargo fmt --check`, `pnpm test:scripts`, `pnpm test:accuracy` (fixtures enforced with `IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1`), `pnpm package:win32-x64`, candidate builds via `cargo <cmd> -p import-lens-daemon --locked --features rolldown-candidate`.
- **SRS performance gates** (spec §10.6 = SRS NFR-002/003/004/005): cold single import p95 ≤ 500 ms, cache hit < 50 ms, startup < 500 ms, idle RSS < 100 MB, 20-import batch < 400 MB peak. VSIX cap 20 MB is a packaging gate only (current VSIXes 3.0–3.5 MB — huge headroom).
- **Memory:** update `bundler-redesign-work-state.md` (auto-memory) and tick checkboxes here as parts land. The spec's §10.7/§15 define gate outcome and definition of done.
- **Rolldown crate sources** (ground truth for the Rust API; docs.rs also builds): `C:\Users\Ehsan\.cargo\registry\src\index.crates.io-*\{rolldown-1.1.5,rolldown_common-1.1.5,rolldown_plugin-1.1.5,rolldown_error-1.1.5}\`. Never import private modules; public API only (spec §3.2/§10.4).

## Part map

| part | delivers | spec | precondition |
| --- | --- | --- | --- |
| B | candidate engine + construct matrix + real-package qualification + measurement harness + gate verdict | §5–§10 | none (start now) |
| C | guarded production integration behind the async boundary | §9, §11 Phase 2 | every §10 gate passed, spec marked accepted |
| D | atomic cutover: Rolldown only engine, old engine + tests deleted, ANALYZER_REVISION bump, README/SRS/skill truthful | §11 Phase 3 | C stable |
| E | release re-baseline: accuracy + truly_treeshakeable baselines, six targets, hashes, memory updates, spec marked complete | §11 Phase 4, §15 | D landed |

---

## Part B — Candidate engine and qualification (Phase 1, second half)

### Task B0: Pin down the exact Rolldown 1.1.5 API surface

**Files:** none committed — findings flow into B1–B3 code.

- [x] **Step 1:** Locate the vendored sources: `ls ~/.cargo/registry/src/index.crates.io-*/ | grep -E "rolldown|rolldown_common|rolldown_plugin"` (Git Bash). Optionally build local docs: `cargo doc -p rolldown --no-deps --features rolldown-candidate --locked`.
- [x] **Step 2:** From `rolldown-1.1.5/src` and `rolldown_common-1.1.5/src`, record the exact shapes of: `BundlerBuilder` (`with_options`, `with_plugins`, `build`), `Bundler::{scan,generate,write,close}` signatures and `BundleOutput` (chunks, warnings, errors); `BundlerOptions` fields for `input` (entry list type), `cwd`, `external` (`IsExternal`), `format`, `treeshake` (`TreeshakeOptions`), `preserve_entry_signatures` (`PreserveEntrySignatures` — pick the strict variant), `code_splitting` (`CodeSplittingMode` — pick the variant that inlines dynamic imports into one chunk; verify by reading how the chunk graph consumes it), `resolve` (`ResolveOptions.condition_names/main_fields/extensions/symlinks`), `minify` (leave off).
- [x] **Step 3:** From `rolldown_plugin-1.1.5/src`, record the plugin trait (name, `resolve_id`, `load`, `module_parsed` hook signatures and their `Hook*Output` types), the plugin context resolver (`ctx.resolve(...)` with self-skip), and `HookSideEffects` (never returned for real modules — spec §7.4).
- [x] **Step 4:** Confirm how output chunks expose `exports`, `modules` (path → `RenderedModule`), and `rendered_length()`; confirm the error/warning types are convertible to strings without Debug-formatting internals (spec §5.1 forbids Rolldown types in diagnostics).

### Task B1: Candidate module skeleton and the engine contract

**Files:**
- Create: `daemon/src/candidate/mod.rs`, `daemon/src/candidate/entry.rs`, `daemon/src/candidate/engine.rs`, `daemon/src/candidate/plugin.rs`
- Modify: `daemon/src/lib.rs` (add `#[cfg(feature = "rolldown-candidate")] pub mod candidate;`)

**Interfaces (produces — later tasks and Part C rely on these exact names):**

```rust
// daemon/src/candidate/mod.rs — Import Lens-owned; NO rolldown type may leak
// through this surface (spec §4.2, §5).
use std::path::PathBuf;

pub use crate::pipeline::resolver::SideEffectsMode; // reuse the existing enum

#[derive(Debug, Clone)]
pub struct BundleRequest {
    pub entries: Vec<BundleEntry>,
    pub runtime: ImportRuntime,
    pub purpose: BundlePurpose,
}

#[derive(Debug, Clone)]
pub struct BundleEntry {
    pub entry_path: PathBuf,
    pub package_root: PathBuf,
    pub selection: BundleSelection,
    pub reported_side_effects: SideEffectsMode,
}

#[derive(Debug, Clone)]
pub enum BundleSelection {
    Named(Vec<String>),
    Default,
    Namespace,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundlePurpose {
    ImportSize,
    FileSize,
    FullPackageComparison,
    ExportEnumeration,
}

#[derive(Debug, Clone)]
pub struct ModuleContribution {
    pub path: PathBuf,
    pub rendered_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct ImportDiagnostic {
    pub stage: String,
    pub message: String,
}

#[derive(Debug)]
pub struct BundleArtifact {
    pub code: String,
    pub loaded_paths: Vec<PathBuf>,
    pub contributions: Vec<ModuleContribution>,
    pub exported_names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub matched_side_effect_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct BundleFailure {
    pub stage: String, // "resolve" | "parse" | "link" | "generate" | "output_shape" | "module_graph_limit" | "missing_export" | "ambiguous_export"
    pub message: String,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub loaded_paths: Vec<PathBuf>,
}
```

`ImportRuntime` already exists in the pipeline (grep `ImportRuntime` under `daemon/src` and reuse; it carries the component/client/server condition context). Reuse limit constants from `crate::pipeline::graph::{MAX_GRAPH_MODULES, MAX_MODULE_SOURCE_BYTES, MAX_GRAPH_SOURCE_BYTES}` — in Part D those constants MOVE into `candidate` (rename module then) rather than being re-declared.

- [x] **Step 1:** Write `mod.rs` with the types above; `engine.rs`/`plugin.rs`/`entry.rs` as empty shells compiling under the feature.
- [x] **Step 2:** `cargo check -p import-lens-daemon --locked --features rolldown-candidate` — clean; `cargo check -p import-lens-daemon --locked` — clean (default build must not see the module).

### Task B2: Virtual entry generation (pure, fully unit-tested)

**Files:** `daemon/src/candidate/entry.rs`; unit tests in the same file (`#[cfg(test)]`).

Rules (spec §6.2): every requested surface gets a unique `__il_entry_<i>_...` alias so strict entry signatures keep it alive; names and specifiers are serialized with `serde_json::to_string` (a JSON string literal is a valid JS string literal — never interpolate raw); synthetic specifiers are `import-lens:target/<index>`; namespace/full use the escaping-namespace form because `export * from` drops the default export.

```rust
use serde_json::to_string as js_string;

pub const VIRTUAL_ENTRY_ID: &str = "import-lens:entry";
pub const TARGET_PREFIX: &str = "import-lens:target/";

pub fn synthetic_target(index: usize) -> String {
    format!("{TARGET_PREFIX}{index}")
}

pub fn virtual_entry_source(entries: &[super::BundleEntry]) -> String {
    let mut source = String::new();
    for (index, entry) in entries.iter().enumerate() {
        let specifier = js_string(&synthetic_target(index)).expect("specifier serializes");
        match &entry.selection {
            super::BundleSelection::Named(names) => {
                for (name_index, name) in names.iter().enumerate() {
                    let exported = js_string(name).expect("export name serializes");
                    source.push_str(&format!(
                        "export {{ {exported} as __il_entry_{index}_export_{name_index} }} from {specifier};\n"
                    ));
                }
            }
            super::BundleSelection::Default => {
                source.push_str(&format!(
                    "export {{ default as __il_entry_{index}_default }} from {specifier};\n"
                ));
            }
            super::BundleSelection::Namespace | super::BundleSelection::Full => {
                source.push_str(&format!(
                    "import * as __il_entry_{index}_namespace from {specifier};\n\
                     export {{ __il_entry_{index}_namespace }};\n"
                ));
            }
        }
    }
    source
}
```

Note the string-literal export form: `export { "a-b" as alias } from ...` — `js_string` already emits the quotes, so `Named(vec!["a-b"])` renders `export { "a-b" as __il_entry_0_export_0 } from "import-lens:target/0";` exactly as the spec shows. Plain identifiers also render quoted (`export { "parse" as ... }`) — valid ES2022 module syntax; the matrix asserts Rolldown accepts it (if 1.1.5's parser rejects quoted *identifier* exports anywhere, emit unquoted when the name is a valid identifier — decide from the B4 run, keep the quoted path for non-identifier names).

- [x] **Step 1:** Write the failing unit tests: named (incl. `"a-b"` string-literal name and two names → distinct aliases), default, namespace, full, multi-entry (indexes 0/1, shared nothing), and an adversarial name (`__il_entry_0_export_0` as the *requested* name must not collide — aliases are positional so it cannot).
- [x] **Step 2:** Implement (code above), `cargo test -p import-lens-daemon --locked --features rolldown-candidate candidate::entry` — green.

### Task B3: Engine adapter and native plugin

**Files:** `daemon/src/candidate/engine.rs`, `daemon/src/candidate/plugin.rs`.

**Interfaces (produces):**

```rust
pub struct RolldownEngine; // stateless; one Bundler per build (no reuse across requests)

impl RolldownEngine {
    pub async fn bundle(&self, request: BundleRequest) -> Result<BundleArtifact, BundleFailure>;
    pub async fn enumerate_exports(&self, entry_path: PathBuf, runtime: ImportRuntime)
        -> Result<Vec<String>, BundleFailure>;
}
```

Behavioral requirements (each cites the spec section; the exact rolldown calls come from B0):

1. **Options** (§7.1): ESM format; strict entry signatures; source maps off; code splitting disabled (dynamic imports inline — B4 has the construct proving it); minify off; single virtual entry `VIRTUAL_ENTRY_ID`; `resolve.condition_names`/`main_fields` mirrored from the existing direct-resolver configuration — read `daemon/src/pipeline/resolver.rs` and reuse its condition/main-field lists per `ImportRuntime` variant (do not re-derive them; extract a shared helper if needed so the two cannot drift); builtins/unresolved peers external with structured diagnostics.
2. **Plugin** (§7.2/§7.3): `resolve_id` answers `VIRTUAL_ENTRY_ID` and maps `import-lens:target/<i>` to `entries[i].entry_path` (pre-resolved absolute path — never re-resolve the bare package); `load` serves the generated entry source; all other resolution delegates to the plugin context resolver with self-skip. Limits: reject a real module > `MAX_MODULE_SOURCE_BYTES` at load; count unique internal modules in `module_parsed` (exclude virtual + external) against `MAX_GRAPH_MODULES`; accumulate source bytes against `MAX_GRAPH_SOURCE_BYTES`. Limit state per build via `AtomicUsize`s owned by the plugin instance; breach → typed `module_graph_limit` failure, never a partial graph. Record every resolved/loaded REAL path into a `Mutex<HashSet<PathBuf>>` for `loaded_paths` (canonicalized, sorted, deduplicated at translation).
3. **No semantic overrides** (§7.4): no `HookSideEffects` for real modules, no glob matching, no AST inspection, no interop or renaming. `matched_side_effect_paths` comes from public metadata where available, else stays empty plus a conservative confidence diagnostic.
4. **Output translation** (§5.1/§8): exactly one chunk and no extra assets or → `output_shape` failure; `code` = the chunk's unminified source; `exported_names` = chunk export list; `contributions` = rendered real modules only (exclude virtual entry, rolldown runtime modules, externals, zero-length), `rendered_bytes` = `rendered_length()`; diagnostics stringified without rolldown types. Missing/ambiguous requested export surfaces as a typed failure (`missing_export`/`ambiguous_export`) with zero-size semantics — never a guessed binding (§12).
5. **Export enumeration** (§8.4): same engine, real entry as strict entry, read the chunk export list; forbid any call into `module_exported_names`/`module_provides_export`.

- [x] **Step 1:** Implement options mapping + plugin with the B0-recorded signatures; keep every rolldown import inside `engine.rs`/`plugin.rs`.
- [x] **Step 2:** Smoke integration test (temp workspace, one `export const x = 1` module, Named["x"]) proving: build succeeds, one chunk, `x` exported, loaded_paths contains the file. Run under `--features rolldown-candidate`.
- [x] **Step 3:** `cargo clippy --workspace --all-targets --locked --features rolldown-candidate` (note: workspace-level clippy won't enable the feature by default — run `cargo clippy -p import-lens-daemon --all-targets --locked --features rolldown-candidate` explicitly) and `cargo fmt`.

**Commit 1 [review]:** `feat(daemon): add feature-gated rolldown candidate engine` (B1+B2+B3; body: contract, plugin responsibilities, what is intentionally NOT implemented — no production path touched).

### Task B4: Construct matrix (the committed qualification test)

**Files:** `daemon/tests/candidate_matrix.rs` (whole file `#![cfg(feature = "rolldown-candidate")]`), reusing `daemon/tests/common.rs` temp-workspace helpers and the `assert_no_dangling_il_bindings`-style OXC validation from `daemon/tests/bundle.rs` (copy the helper — bundle.rs is deleted in Part D, the matrix must not import from it).

Harness shape (complete; `run` builds one Named request unless the row says otherwise):

```rust
#![cfg(feature = "rolldown-candidate")]
use import_lens_daemon::candidate::{
    BundleEntry, BundlePurpose, BundleRequest, BundleSelection, RolldownEngine,
};

async fn run(root: &Path, entry: &str, selection: BundleSelection)
    -> Result<BundleArtifact, BundleFailure> {
    RolldownEngine
        .bundle(BundleRequest {
            entries: vec![BundleEntry {
                entry_path: root.join(entry),
                package_root: root.to_path_buf(),
                selection,
                reported_side_effects: SideEffectsMode::Unknown, // reuse existing variant names — grep before writing
            }],
            runtime: ImportRuntime::default_for_tests(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
}
// every Ok artifact additionally passes: parseable, semantic-valid,
// zero unresolved identifiers prefixed `__il_entry_` (port the semantic
// unresolved-references walker from daemon/tests/bundle.rs).
```

Required rows (spec §10.2 — every category; fixture sources are single-line JS, `→` is the assertion):

| # | construct | fixture (entry first) | expected |
| --- | --- | --- | --- |
| 1 | local named export | `export const parse = 1;` → Named["parse"] | code contains binding; exported_names has the alias |
| 2 | local default + alias | `const v=1; export default v;` → Default | default alias exported |
| 3 | imported-then-exported | `import {a} from './a.js'; export {a};` / `export const a=1;` | a.js rendered |
| 4 | direct named re-export | `export {leaf} from './leaf.js';` | leaf rendered, entry glue only |
| 5 | single `export *` | `export * from './leaf.js';` → Named["leaf"] | resolves through star |
| 6 | chained `export *` | entry→mid→leaf stars | leaf rendered once |
| 7 | ambiguous star providers | `export * from './a.js'; export * from './b.js';` both export `x` → Named["x"] | typed failure `ambiguous_export` (or diagnostic + failure — never both providers silently) |
| 8 | `export * as ns` | `export * as ns from './leaf.js';` → Named["ns"] | valid namespace in output |
| 9 | forwarded namespace | `export * from './mid.js';` / mid: `export * as ns from './leaf.js';` → Named["ns"] | resolves; no dangling |
| 10 | namespace static read | `import * as ns from './leaf.js'; export const r = ns.a;` | only `a` retained from leaf |
| 11 | namespace computed read | `... ns[key] ...` | whole leaf retained, valid |
| 12 | namespace optional read | `ns?.a` | valid output |
| 13 | escaping namespace | `export const grab = () => ns;` | materialized namespace, no dangling |
| 14 | empty namespace target | `import * as ns from './empty.js'; export const grab = () => ns;` / empty.js `""` | valid output (the §2.2 case the old engine fails) |
| 15 | string-literal names | `const v=1; export { v as "a-b" };` → Named["a-b"] | resolves |
| 16 | side-effect-only import | `import './fx.js'; export const x=1;` / fx.js `globalThis.__p=1;` | fx retained |
| 17 | pure unused declaration | entry also has `const dead = 1;` | dead code absent from chunk |
| 18 | effectful unused non-export | `sideEffect(foo)` top-level, foo imported | statement + import retained |
| 19 | exported effectful initializer, unrequested | `export const unused = compute(dep);` → request other export | initializer + dep retained (§2.2 case) |
| 20 | binding-less top-level statement | `import {f} from './f.js'; f();` | f.js retained |
| 21 | cycle | a↔b mutual imports | builds, both rendered once |
| 22 | shared diamond | entry→a,b; a,b→shared | shared rendered ONCE (contributions prove dedup) |
| 23 | external import | `import fs from "node:fs"; export const x = fs;` | builtin external, no failure, diagnostic notes external |
| 24 | external named re-export | `export { thing } from "unresolvable-pkg";` | boundary preserved in output OR typed diagnostic — NOT an empty chunk (§2.2 case) |
| 25 | external star re-export | `export * from "unresolvable-pkg";` | same policy as 24 |
| 26 | CJS interop | leaf.cjs `module.exports = { fn(){} };` imported from ESM | fn reachable, valid output |
| 27 | CJS export shapes | `exports.named =`, `module.exports.x =`, conditional exports | enumeration returns names rolldown resolves |
| 28 | TS/TSX/JSX/JSON/.mts/.cts inputs | one tiny fixture each | transformed, single chunk, parseable |
| 29 | `__il_` collision | source declares `const __il_entry_0_export_0 = 5; export const x = __il_entry_0_export_0;` | deconflicted, no dangling |
| 30 | missing export | Named["nope"] | typed `missing_export` failure, no guessed binding |
| 31 | parse failure | syntactically invalid module | typed failure, stage parse/link |
| 32 | module-count limit | generate `MAX_GRAPH_MODULES + 1` tiny modules (script-generated chain) | `module_graph_limit` failure, deterministic |
| 33 | single-module size limit | one module > 20 MiB (write programmatically, gitignored temp) | `module_graph_limit` |
| 34 | total source limit | modules summing > 100 MiB — SKIP by default (`#[ignore]`) with a comment: covered by 33's code path; run explicitly before the gate verdict |
| 35 | transitive dynamic import | dep does `import('./lazy.js')` | ONE chunk, lazy inlined — proves the code-splitting knob (§6.2/§7.1) |
| 36 | dynamic/full request | Full selection over a package dir with default+named | complete surface measured |
| 37 | multi-package request | two entries, shared transitive dep | shared dep contributions counted once (§6.3) |
| 38–44 | `sideEffects` false / true / missing / invalid / string / array / nearest-transitive-package | package.json fixtures per §7.4 | retention matches Rolldown's native interpretation; `matched_side_effect_paths` populated only from public metadata; no hook override exists to assert against |

- [x] **Step 1:** Write the harness + rows 1–15; run; fix adapter translation issues they surface.
- [x] **Step 2:** Rows 16–29.
- [x] **Step 3:** Rows 30–44 (failure/limit/side-effect rows).
- [x] **Step 4:** Full run green: `cargo test -p import-lens-daemon --locked --features rolldown-candidate --test candidate_matrix`.

### Task B5: Real-package qualification fixtures

**Files:**
- Modify: `scripts/accuracy-fixtures/package.json` (+ regenerate its committed `pnpm-lock.yaml`)
- Create: `daemon/tests/candidate_packages.rs` (`#![cfg(feature = "rolldown-candidate")]`, `#[ignore]`-gated like the performance suite so `pnpm test` stays hermetic; run explicitly during qualification)

- [x] **Step 1:** Add `lodash-es`, `zod`, `react`, `uuid` at the latest stable versions resolved at execution time (exact pins, like the existing three), `pnpm install --lockfile-only` inside the fixtures dir, commit the lockfile. Keep `css-tree` 3.2.1, `date-fns` 4.1.0, `lodash` 4.17.21.
- [x] **Step 2:** Test cases per spec §10.3/§10.4 over the installed fixtures (fixture install is a setup step; the test performs no network): for each (package, representative export — `css-tree/parse`, `date-fns/format`, `lodash-es/debounce`, `lodash/debounce`, `zod/z`, `react/useState`, `uuid/v4`): candidate build succeeds; output parses + passes OXC semantic validation; zero unresolved `__il_entry_*` AND zero unresolved identifiers overall for css-tree/date-fns (the dangling cases reach zero, §10.4); `loaded_paths` ⊇ rendered module paths and includes at least one tree-shaken (non-rendered) path for date-fns; contributions rendered-only; repeated identical runs byte-identical (determinism gate).
- [x] **Step 3:** Record raw/minified sizes vs the current engine and vs esbuild (reuse `scripts/accuracy-compare.mjs` fixtures/oracle where practical) — informational for §10.6's comparison set.

**Commit 2 [review]:** `test(daemon): add rolldown qualification matrix and package fixtures` (B4+B5).

### Task B6: Measurement harness, gates, and the verdict

**Files:**
- Create: `daemon/tests/candidate_performance.rs` (`#![cfg(feature = "rolldown-candidate")]`, `#[ignore]`, mirroring `daemon/tests/performance.rs` patterns and its `IMPORT_LENS_PERF_MULTIPLIER` tolerance env)
- Modify (results): `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` §10.7 + status header

- [x] **Step 1:** Measure per spec §10.6 in `--release` with 5 warm-ups + ≥30 runs: cold single import p95 (target fixture: css-tree/parse), 20-import batch latency, peak RSS (reuse whatever `performance.rs` uses for memory; on Windows fall back to `tasklist`-style probing only inside the ignored test), startup, cache-hit path untouched (assert unchanged code path). Record candidate binary size per target (informational).
- [x] **Step 2:** Determinism: run the full matrix + package suite twice; byte-compare size fields, loaded paths, contributions, failure stages.
- [x] **Step 3:** Six-target compile: `pnpm package:win32-x64` must stay green (default build unaffected); for the candidate graph run `cargo check -p import-lens-daemon --locked --features rolldown-candidate --target <triple>` for the six triples from `scripts/targets.mjs` (zig/xwin cross-checks via the existing docker builder if native check is impossible — compile proof only, no packaging of candidate binaries; they are never published, spec §10).
- [x] **Step 4:** Verdict per §10.7. PASS → update the spec: measured results table in §10.7, status header → **accepted**; update `bundler-redesign-work-state.md` memory. FAIL on an absolute gate → ONE bounded optimization pass (public options, adapter allocations, concurrency limit), re-measure; still failing → mark rejected in the spec and STOP (production unchanged; do NOT resurrect the fixpoint proposal — spec §10.7).

**Commit 3 [review]:** `test(daemon): add candidate measurement harness` + **Commit 4:** `docs(daemon): record rolldown qualification results` (or one commit if the harness lands with the results — prefer fewer).

---

## Part C — Guarded production integration (Phase 2; only after B's gates pass)

### Task C1: Promote the dependency and wire the engine

**Files:** `daemon/Cargo.toml` (rolldown loses `optional = true`; the `rolldown-candidate` feature is removed — update `scripts/compiler-stack-helpers.mjs` `validateCurrentStack`, `scripts/test/compiler-stack-coordination.test.mjs` rolldown-line/feature assertions, and `scripts/compiler-stack-fingerprint.mjs` to drop `--features rolldown-candidate`, all in this same change), `daemon/src/candidate/` renamed `daemon/src/engine/` (no longer candidate), integration seams in `daemon/src/pipeline/analyze.rs`, `daemon/src/pipeline/file_size.rs`, `daemon/src/service.rs`, `daemon/src/prefetch.rs`.

Requirements (spec §9, §11 Phase 2):

1. Misses run as async work behind a daemon-wide `tokio::sync::Semaphore` with **2 permits**; Rolldown builds are NEVER invoked from the outer global-Rayon `par_iter` loops (`service.rs:493/613/647/685/1160-1169/1307/1390/2020`, `prefetch.rs:258/309` — re-grep `par_iter` at execution time, line numbers drift). Cache hits bypass the semaphore and never construct a bundler.
2. Batch/file-size results preserve input ordering when misses complete out of order; streaming keeps completion order + existing indexes.
3. Blocking work (cache, fingerprints, OXC minify, compression) stays off Tokio I/O threads (`spawn_blocking` or existing worker pools — follow current service patterns).
4. Every size-producing path uses the engine: individual analysis, full-package comparison (`truly_treeshakeable`), export enumeration, prewarm, combined file sizing (one `BundleRequest` with all entries — never concatenate per-package bundles, §6.3).
5. Conservative static fallback and structured error behavior preserved per the §12 table; dependency fingerprints switch to `loaded_paths` (+ manifests); cache lifecycle/IPC/schema untouched (§9).
6. Old engine stays compiled and used ONLY by temporary differential tests (`daemon/tests/differential_engines.rs`: same fixture set through both engines; assert candidate output valid and record deltas — expectations are qualitative, not byte-equality; the values are EXPECTED to move).
7. NO `ANALYZER_REVISION` bump yet — production still runs the old engine until D? **No** — Phase 2 integrates Rolldown as the runtime engine for misses… **Decision recorded in the spec (§11):** Phase 2 wires the engine but production selection flips in Phase 3 atomically with the revision bump. Keep a single boolean seam (`const USE_ROLLDOWN_ENGINE: bool = false` or cfg) so Phase 3's flip is one line + deletion; the differential tests exercise the wired path.

- [x] Steps: read the four integration seams; implement the boundary; port each size-producing path; add ordering + semaphore tests (start 3 concurrent misses, assert ≤2 rolldown builds in flight via a test-only counter); full gate.

**Commit 5 [review]:** `feat(daemon): integrate the rolldown engine behind the async execution boundary` (body must state production output is still the old engine and why).

---

## Part D — Atomic cutover and deletion (Phase 3)

One commit. Everything below lands together; independent review is mandatory.

**Delete (code):** `daemon/src/pipeline/bundle.rs`, `daemon/src/pipeline/reachability.rs`, `daemon/src/pipeline/cjs.rs`, the manual module-graph construction in `daemon/src/pipeline/graph.rs` (relocate small non-bundling helpers first — `module_exported_names` dies with export enumeration's old path; the document pipeline under `daemon/src/document/` is untouched), the marker passes in `minify.rs`/`analyze.rs`/`file_size.rs`, generated binding fabrication, namespace-object construction, the package-side-effect matcher/override, bundling-only graph records, and the Phase-2 selection seam (Rolldown becomes the only engine).

**Delete (tests):** `daemon/tests/bundle.rs`, bundling coverage in `daemon/tests/graph.rs`, `daemon/tests/differential_engines.rs`, and every test asserting custom linking/tree-shaking internals. After cutover no test re-verifies Rolldown-owned semantics outside the §10.2 matrix (which asserts OUR contract).

**Modify:**
- `daemon/src/cache/key.rs:24` — `ANALYZER_REVISION: "graph2"` → `"rolldown1"` (atomic with the flip; caches repopulate, no schema change).
- `daemon/Cargo.toml` — remove direct `oxc_ast`, `oxc_ast_visit`, `oxc_transformer` (they remain transitive); keep allocator/parser/span/syntax (document pipeline) + resolver + allocator/parser/semantic/minifier/codegen/span (validate+minify path) per spec §4.2.
- `scripts/compiler-stack.config.mjs` — shrink `oxcCrates` to the retained set; regenerate the fingerprint; the coordination/updater tests follow automatically (they derive from config).
- `.claude/skills/compiler-stack-upgrade/SKILL.md` — rewrite the "five surface facts" (they describe marker/bundle internals that no longer exist) and the OXC-usage file list.
- `tsdown.config.ts` — remove the stale `"oxc-parser"` from `neverBundle` (the Guard test asserting no direct dependency stays).
- `README.md` — lines 9/27/152/173: describe Rolldown (built on OXC) as the linking/tree-shaking engine; direct OXC as document parser, root resolver, validator, final minifier; remove "custom reachability analysis" and the OXC-only footer; keep privacy/local-only/caching/compression claims.
- `docs/ImportLens-SRS.md` — the architecture rewrite (this is the moment production behavior changes, so CLAUDE.md's same-task rule fires): grep-driven sweep for `custom reachability|module graph walker|reference-closure|virtual module graph` plus §4.2 (bundler decision — record the Rolldown adoption + gate results), §9.2 component descriptions, C-003 final state, Appendix C technology-watch Rolldown row → "Adopted". Sizes/`truly_treeshakeable` movement documented as intended (spec §14.4).
- `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` — status → cutover complete (Phase 3 done).

- [ ] Steps: relocate helpers → flip selection → delete inventory → shrink deps/config/fingerprint → docs sweep → full gate (`pnpm check && pnpm test && cargo fmt --check`) → `pnpm test:accuracy` with enforcement (numbers WILL move — trace each fixture delta to a construct, spec §14.4) → **Commit 6 [review]:** `feat(daemon)!: make rolldown the only semantic bundler` (breaking `!` because measured outputs change; body covers the revision bump and deletion inventory).

---

## Part E — Release re-baseline (Phase 4) and definition of done

- [ ] Re-baseline accuracy + `truly_treeshakeable` against the new engine (`IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1 pnpm test:accuracy`); update stored baselines/thresholds wherever `scripts/accuracy-compare.mjs` keeps them.
- [ ] `pnpm package:win32-x64` (daemon changed → repackage + `knownHashes.generated.ts` refresh is REQUIRED per CLAUDE.md); then the remaining five targets via the docker builder (`pnpm docker:build` / `package:all:zig` + xwin) and `pnpm assert:vsix-size` (NFR-007 — expect ample headroom; record the actual delta).
- [ ] Run the §15 definition-of-done checklist in the spec top to bottom; fix anything unchecked.
- [ ] Update memory: `bundler-redesign-work-state.md` → complete; `dependency-version-policy.md` unaffected; note the retained-crate shrink.
- [ ] **Commit 7:** `build(release): re-baseline accuracy and daemon hashes for the rolldown engine` (plus any doc straggler).

## Verification quick-reference

| when | command |
| --- | --- |
| candidate compile | `cargo check -p import-lens-daemon --locked --features rolldown-candidate` |
| matrix | `cargo test -p import-lens-daemon --locked --features rolldown-candidate --test candidate_matrix` |
| package qualification | same + `--test candidate_packages -- --ignored` |
| perf gates | `cargo test -p import-lens-daemon --release --locked --features rolldown-candidate --test candidate_performance -- --ignored --nocapture` |
| full gate (every commit) | `pnpm check && pnpm test && cargo fmt --check` |
| accuracy (B5/D/E) | `IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1 pnpm test:accuracy` |
| release path (E) | `pnpm package:win32-x64 && pnpm assert:vsix-size` |

## Standing rules that bind every part

- Never commit to `main`; stay on `bundler-redesign` (or successors branched from it).
- One commit per logically-coherent change (markers above); commit bodies are required and feed the changelog.
- Tests must be Logic/Drift/Property/Guard — no Echoes; never assert a dependency version outside the compiler stack.
- No hand-edited compiler-stack pins or fingerprint — only `pnpm deps:update:compiler`.
- A Rolldown version bump at ANY point re-runs the §10 gates (skill: `compiler-stack-upgrade`).
- If a needed capability requires private Rolldown internals — stop and reject per §10.7; do not fork or vendor.
- LF endings; pnpm only; keep Windows packaging green before other targets.

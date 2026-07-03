# Perf Plan B — Graph & Bundle Compute (DF-2, DF-6, DF-7, DF-12-bundle)

> **STATUS: plan ready; execute task-by-task, one commit per task.** First of four grouped follow-up plans from the deferred backlog in `2026-07-03-daemon-review-fixes.md` (Part C). Sequence: **B (this) → A (resolver+fingerprint) → C (persistence) → D (small/conditional).**
>
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans (or subagent-driven-development). Steps use `- [ ]` checkboxes.

**Goal:** Stop re-parsing and re-running semantic analysis on every module for every bundle request (DF-2, the big win), analyze plain JSX shipped in `.js` package files (DF-6), and pick off two low-risk graph/bundle robustness+micro items (DF-7, DF-12-bundle) — each as its own commit.

**Architecture:** All changes live in the daemon's static-analysis pipeline (`pipeline/graph.rs`, `pipeline/bundle.rs`, `pipeline/minify.rs`) plus `pipeline/analyze.rs` for one fallback path. No IPC/protocol surface changes — the extension sees identical `ImportResult` bytes; DF-6 only changes *which* packages analyze successfully vs. fall back. The module graph is already cached (`GRAPH_CACHE`, bounded to 32 entries after T18); DF-2 moves per-request work onto that cached structure.

**Tech stack:** Rust 2024, oxc 0.138 (parser/semantic/transformer/codegen).

## Global constraints

- **Bundle output must stay byte-for-byte identical** for DF-2. The existing `daemon/tests/bundle.rs` suite asserts exact generated source and exact `__il_*` binding names — it is the regression harness. If any of its assertions change, the DF-2 refactor is wrong; do not "update the expectation" to make it pass.
- No change to `ipc/protocol.rs` wire types (extension pins `protocolVersion = 7`).
- Each task ends with `cargo test -p import-lens-daemon` green and `cargo clippy -p import-lens-daemon --all-targets` introducing no new warnings.
- One commit per task, conventional-commit messages with a body explaining the user-visible effect (or "no behavior change" for pure refactors).
- Perf tasks add `#[ignore]`'d timing tests to `daemon/tests/performance.rs` following its existing `threshold_ms()` / `Instant` pattern — they document the win, they are not correctness gates.

---

## Verification notes (checked against current code before writing)

Confirmed by reading the current tree (post T1–T21):

- **DF-2 is not just valid, it is a near-duplicate pass.** `bundle.rs::semantic_rename_replacements` ([bundle.rs:597](../../daemon/src/pipeline/bundle.rs)) re-parses `module.source` with `SourceType::mjs()` and runs `SemanticBuilder::new().with_build_nodes(true)`, then iterates `scoping.iter_bindings_in(scoping.root_scope_id())` → `symbol_name` / `symbol_span` / `symbol_references` → `reference_span`, plus `shorthand_identifier_spans`. `graph.rs::binding_dependencies` ([graph.rs:1044](../../daemon/src/pipeline/graph.rs)) runs the **identical** `SemanticBuilder::new().with_build_nodes(true)` on the **same** prepared source (`parse_module` parses the prepared source with `source_type_for_prepared_module()` = `mjs()`; `module.source` *is* that prepared source), iterating the same bindings→references. So every bundle request recomputes spans that graph-build already computed. Merging is natural.
  - **Nuance 1:** `binding_dependencies` early-returns before building semantic when `statement_binding_ranges` is empty. The shared pass must run whenever there is anything to rename (exports/local bindings/imports), not only when there are top-level binding statements.
  - **Nuance 2:** `binding_dependencies` records only reference spans; DF-2 additionally needs each symbol's declaration span (`scoping.symbol_span`).
  - **Nuance 3 (behavior):** `semantic_rename_replacements` currently returns `Err` (failing the whole bundle) on `semantic.diagnostics.has_errors()`, whereas `binding_dependencies` returns empty. `SemanticBuilder::new()` without `with_check_syntax_error` rarely emits diagnostics, but moving the pass to graph build means a would-be rename-time semantic error no longer fails the bundle. This is acceptable (arguably better — fewer spurious fallbacks) but is a real, test-covered behavior decision, called out in B3.
  - Spans are offset-based within `module.source`; `ModuleRecord` is `Clone` and `file_size.rs::merge_graph_modules` clones records and only reassigns `module.id` (not source), so stored spans stay valid across the combined-graph path. Verified no interaction.
- **DF-6 verified.** `module_needs_transform` ([graph.rs:619](../../daemon/src/pipeline/graph.rs)) covers `ts|tsx|mts|cts|jsx` — **not** `js`. A `.js` module takes the no-transform branch, is parsed by `parse_module` as `mjs()` (no JSX), and JSX content produces a parse error that fails the whole graph → LOW-confidence static-entry fallback (`analyze.rs`). Retrying as JSX is safe. **Payoff scoped:** oxc does not parse Flow, so `.js` files carrying Flow types (React Native *core*) still fail and fall back (no regression); DF-6 targets packages shipping *plain JSX in `.js`*.
- **DF-7 deferred to Plan D (watch-list) — not in this plan.** `load_module_from` ([graph.rs:~360](../../daemon/src/pipeline/graph.rs)) recurses per edge; depth is bounded by the longest acyclic import chain (cycles cut by `path_to_id`), capped at `MAX_GRAPH_MODULES = 2000`. No real package reproduces a stack overflow, so rewriting a correct, hot recursion for a theoretical gain carries more regression risk than value right now. Parked in Plan D's watch-list; revisit only if a real deep-chain package actually triggers an overflow.
- **DF-12 bundle bits verified.** `sanitize_identifier` ([bundle.rs:815](../../daemon/src/pipeline/bundle.rs)) maps invalid bytes to `_` byte-by-byte, so two names differing only in same-position non-ASCII bytes collide — but only *within one module* (the `__il_m{id}_` prefix scopes it). Astronomically rare. The `minify.rs` Transformer pass over already-plain-JS bundles is *possibly* removable but "measure first"; kept as an investigation, not a committed change.

---

### Task B1: Add a DF-2 measurement to `tests/performance.rs`

Establish a before/after number so the DF-2 win is evidenced, not asserted. This is a self-contained synthetic bench (does not depend on `packages.zip` contents).

**Files:**
- Modify: `daemon/tests/performance.rs` (append)

- [ ] **Step 1: Add an ignored timing test** that forces repeated re-bundles of a multi-module package by using distinct named-import subsets (distinct cache keys → forced miss → re-bundle each time), so the per-request bundle cost — including the `semantic_rename_replacements` pass DF-2 removes — dominates.

```rust
#[test]
#[ignore = "release-only performance smoke run by pnpm test:performance"]
fn multi_module_rebundle_stays_under_release_threshold() {
    use std::fs;
    let workspace = common::temp_workspace("import-lens-perf-bundle");
    let pkg = workspace.join("node_modules").join("multi-lib");
    fs::create_dir_all(&pkg).expect("pkg dir");
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"multi-lib","version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    // One barrel entry re-exporting 40 leaf modules, each a helper referencing a local.
    let mut index = String::new();
    for i in 0..40 {
        fs::write(
            pkg.join(format!("leaf{i}.js")),
            format!("const base{i} = {i};\nexport const fn{i} = () => base{i} + 1;\n"),
        )
        .expect("leaf");
        index.push_str(&format!("export {{ fn{i} }} from './leaf{i}.js';\n"));
    }
    fs::write(pkg.join("index.js"), index).expect("index");

    let service = ImportLensService::new(None, false);
    let document = workspace.join("src").join("app.ts");
    let start = Instant::now();
    // 40 distinct single-export requests → 40 distinct cache keys → 40 re-bundles.
    for i in 0..40 {
        let request = BatchRequest {
            version: PROTOCOL_VERSION,
            request_id: i,
            workspace_root: workspace.to_string_lossy().to_string(),
            active_document_path: document.to_string_lossy().to_string(),
            imports: vec![ImportRequest {
                specifier: "multi-lib".to_owned(),
                package_name: "multi-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec![format!("fn{i}")],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            }],
            streaming: false,
        };
        let response = service.handle_batch(request);
        assert_eq!(response.imports[0].error, None, "{:?}", response.imports[0]);
    }
    let elapsed_ms = start.elapsed().as_millis();

    fs::remove_dir_all(&workspace).expect("cleanup");
    eprintln!("multi_module_rebundle: {elapsed_ms}ms for 40 re-bundles");
    assert!(
        elapsed_ms <= threshold_ms(4000),
        "multi-module re-bundle exceeded threshold: {elapsed_ms}ms"
    );
}
```

- [ ] **Step 2: Record the baseline.** Run `IMPORT_LENS_PERF_MULTIPLIER=1 cargo test -p import-lens-daemon --test performance --release -- --ignored multi_module_rebundle --nocapture` and note the printed ms in the commit body. (Re-run after B3 to quote the improvement.)

- [ ] **Step 3: Commit**

```bash
git add daemon/tests/performance.rs
git commit -m "test(perf): add multi-module re-bundle timing baseline" -m "Forces 40 distinct-key re-bundles of a 40-module barrel package so the per-request parse+semantic pass (removed in the next commits) dominates the measurement. Baseline: <X>ms at multiplier 1."
```

---

### Task B2: Compute root-scope symbol spans + shorthand spans at graph build (DF-2, data)

Additive only — store the data on `ModuleRecord`; the bundle still uses its own pass until B3. Reviewer gate: nothing about bundle output changes here.

**Files:**
- Modify: `daemon/src/pipeline/graph.rs` (`ModuleRecord`, `ParsedModule`, `parse_module`, `binding_dependencies` → shared analysis; move the shorthand collector in from `bundle.rs`)
- Modify: `daemon/src/pipeline/bundle.rs` (make `ShorthandIdentifierCollector` + `collect_binding_pattern_spans` + `span_bounds` `pub(crate)` or move them to `graph.rs`)
- Test: `daemon/tests/graph.rs` (append)

**Interfaces (produced, consumed in B3):**
- `pub struct RootSymbolSpans { pub name: String, pub decl: (usize, usize), pub references: Vec<(usize, usize)> }`
- `ModuleRecord` gains `pub root_symbol_spans: Vec<RootSymbolSpans>` and `pub shorthand_spans: Vec<(usize, usize)>`.
- A single graph-build helper `fn root_scope_analysis(program: &Program) -> RootScopeAnalysis` returning `{ dependencies: Vec<BindingDependencyRecord>, symbol_spans: Vec<RootSymbolSpans> }` so the semantic pass runs once.

- [ ] **Step 1: Write the failing test** (append to `daemon/tests/graph.rs`)

```rust
#[test]
fn module_record_carries_root_symbol_and_shorthand_spans() {
    let (root, _entry, graph) = graph_from_sources([(
        "entry.js",
        "const helper = 1;\nconst obj = { helper };\nexport const value = helper + obj.helper;",
    )]);

    let entry = graph
        .modules
        .iter()
        .find(|module| module.path.ends_with("entry.js"))
        .expect("entry module");

    // `helper` is a root binding with a declaration span and at least one reference.
    let helper = entry
        .root_symbol_spans
        .iter()
        .find(|symbol| symbol.name == "helper")
        .expect("helper symbol spans");
    assert!(helper.decl.0 < helper.decl.1);
    assert!(!helper.references.is_empty());

    // `{ helper }` shorthand records the value-identifier span.
    assert!(
        !entry.shorthand_spans.is_empty(),
        "shorthand object property should be recorded"
    );

    fs::remove_dir_all(root).expect("cleanup");
}
```

Add `root_symbol_spans` / `shorthand_spans` (and any new import) to the graph test file as needed.

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --test graph module_record_carries_root_symbol` → FAIL (fields don't exist).

- [ ] **Step 3: Move the shorthand collector to `graph.rs`.** Cut `ShorthandIdentifierCollector`, `shorthand_identifier_spans`, `collect_binding_pattern_spans`, `span_bounds`, and the `oxc_ast_visit`/`ObjectProperty`/`BindingProperty`/`AssignmentTargetPropertyIdentifier` imports from `bundle.rs` into `graph.rs`. (bundle.rs keeps using them via `graph::shorthand_identifier_spans` only until B3 removes the caller.)

- [ ] **Step 4: Add the shared root-scope analysis in `graph.rs`.** Refactor `binding_dependencies` into `root_scope_analysis(program) -> RootScopeAnalysis` that builds the semantic model once and produces **both** the dependency records (unchanged logic) **and** `Vec<RootSymbolSpans>` (`symbol_span` as `decl`, `symbol_references`→`reference_span` collected per symbol). Run the semantic build whenever the program has any root bindings — i.e. do the empty-check on "no bindings at all", not only "no binding statements" (Nuance 1). Populate `ParsedModule.binding_dependencies` from `.dependencies` and add `root_symbol_spans` + `shorthand_spans` (from `shorthand_identifier_spans(&parsed.program)`).

- [ ] **Step 5: Thread the fields through** `ParsedModule` → the `ModuleRecord { … }` construction in `load_module_from`.

- [ ] **Step 6: Run** `cargo test -p import-lens-daemon` → all pass (new test green; bundle output untouched because B3 hasn't switched the consumer). `cargo clippy` clean.

- [ ] **Step 7: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/src/pipeline/bundle.rs daemon/tests/graph.rs
git commit -m "refactor(graph): record root-scope symbol and shorthand spans at build time" -m "The bundle rewriter re-derives root-scope symbol spans, reference spans and object-shorthand spans on every request by re-parsing and re-running semantic analysis, even though the module graph that owns the source is cached. Extend the semantic pass that already runs for binding dependencies to emit that span data too and store it on ModuleRecord. No behavior change yet; the bundle still uses its own pass until the next commit."
```

---

### Task B3: Consume stored spans in the bundle; delete the re-parse (DF-2, the win)

**Files:**
- Modify: `daemon/src/pipeline/bundle.rs` (`semantic_rename_replacements`)
- Test: byte-exact `daemon/tests/bundle.rs` (the guard — must stay green unchanged) + re-run B1's bench

- [ ] **Step 1: Rewrite `semantic_rename_replacements`** to read `module.root_symbol_spans` and `module.shorthand_spans` instead of parsing. The body becomes: for each `RootSymbolSpans` whose `name` is in `renames`, `push_semantic_rename` for `decl` and each `references` entry, keeping the existing `shorthand_spans` lookup, `protected_replacements` overlap filter, and `seen_spans` dedup exactly as-is. Delete the `Parser`/`SemanticBuilder` calls and the two error branches. Keep the `renames.is_empty()` early return.

Skeleton:

```rust
fn semantic_rename_replacements(
    module: &ModuleRecord,
    renames: &HashMap<String, String>,
    protected_replacements: &[Replacement],
) -> Result<Vec<Replacement>, String> {
    if renames.is_empty() {
        return Ok(Vec::new());
    }

    let shorthand_spans: HashSet<(usize, usize)> =
        module.shorthand_spans.iter().copied().collect();
    let mut replacements = Vec::new();
    let mut seen_spans = HashSet::new();

    for symbol in &module.root_symbol_spans {
        let Some(new_name) = renames.get(&symbol.name) else {
            continue;
        };
        for &(start, end) in std::iter::once(&symbol.decl).chain(symbol.references.iter()) {
            push_semantic_rename(
                &module.source,
                start,
                end,
                new_name,
                &shorthand_spans,
                protected_replacements,
                &mut seen_spans,
                &mut replacements,
            )?;
        }
    }

    Ok(replacements)
}
```

Adjust `push_semantic_rename` to take `start: usize, end: usize` (it currently takes an oxc `Span`); the callers now pass stored `(usize, usize)` pairs. Its internals (`source.get(start..end)`, shorthand `{original}: {new}`) are unchanged.

- [ ] **Step 2: Remove now-dead code** — the `SourceType`/`Parser`/`SemanticBuilder`/`Allocator` imports in `bundle.rs` if nothing else there uses them (check `rewrite_module`/`transform_export_statement` first). `graph::shorthand_identifier_spans` is now only called at graph build.

- [ ] **Step 3: Run the byte-exact bundle suite** — `cargo test -p import-lens-daemon --test bundle`. Every assertion must pass **unchanged**. If a `__il_*` name or generated-source assertion differs, stop: the stored spans are not equivalent to the live pass (investigate coordinate space / which symbols are captured) — do not edit the expectation.

- [ ] **Step 4: Behavior-decision test (Nuance 3).** Add one bundle test proving a module the old path would have rejected at semantic time now bundles from stored spans (or, if you choose to preserve the hard-fail, that it still errors). Document the chosen semantics in the commit body. Recommended: accept the bundle (fewer spurious fallbacks).

- [ ] **Step 5: Full suite + re-run bench.** `cargo test -p import-lens-daemon` green; `IMPORT_LENS_PERF_MULTIPLIER=1 cargo test -p import-lens-daemon --test performance --release -- --ignored multi_module_rebundle --nocapture` — quote before/after in the commit body.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/pipeline/bundle.rs
git commit -m "perf(bundle): reuse graph-time symbol spans instead of re-parsing per request" -m "semantic_rename_replacements re-parsed and re-ran semantic analysis on every included module for every bundle request; the spans it needs are now recorded once at graph build (cached) and read directly. Removes a full parse + semantic pass per module per cache-miss/file-size request. Bench: <before>ms -> <after>ms for 40 re-bundles. Bundle output is byte-for-byte unchanged (tests/bundle.rs)."
```

---

### Task B4: Analyze plain JSX shipped in `.js` package modules (DF-6)

**Files:**
- Modify: `daemon/src/pipeline/graph.rs` (`prepare_module_source`, transform helper)
- Test: `daemon/tests/graph.rs` (append)

- [ ] **Step 1: Write the failing tests** (append to `daemon/tests/graph.rs`)

```rust
#[test]
fn graph_transforms_plain_jsx_shipped_in_js_modules() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { Widget } from './widget.js';\nexport const value = Widget;",
    );
    write_source(
        &root,
        "widget.js",
        "export const Widget = () => <div className=\"x\">hi</div>;",
    );

    let graph = build_module_graph(&root.join("entry.js"))
        .expect("plain JSX in .js should build");

    let widget = graph
        .modules
        .iter()
        .find(|module| module.path.ends_with("widget.js"))
        .expect("widget module");
    fs::remove_dir_all(root).expect("cleanup");
    // Prepared source is transformed to JSX-free output for the minifier.
    assert!(!widget.source.contains("<div"), "{}", widget.source);
}

#[test]
fn graph_still_fails_gracefully_on_flow_typed_js() {
    // oxc cannot parse Flow; such a module must error (safe fallback), not panic.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { x } from './flow.js';\nexport const value = x;",
    );
    write_source(&root, "flow.js", "export const x: number = 1;");

    let result = build_module_graph(&root.join("entry.js"));

    fs::remove_dir_all(root).expect("cleanup");
    assert!(result.is_err(), "Flow-typed .js should fail, not panic");
}
```

- [ ] **Step 2: Run** `cargo test -p import-lens-daemon --test graph graph_transforms_plain_jsx` → FAIL (parse error on `<div`).

- [ ] **Step 3: Route JSX-`.js` through the transform.** In `prepare_module_source`, for the JS-like branch (currently the untransformed `Ok(PreparedModuleSource { source, validate_semantics: false })`): attempt a plain parse; if it fails, retry parsing with a JSX-enabled source type and, on success, run the existing transform path with that JSX source type so the prepared output is JSX-free. Concretely, add a helper `transform_module_source_with(path, source, source_type)` (extract the body of `transform_module_source`, which currently derives the type via `SourceType::from_path`), and call it with `SourceType::jsx()` for the JSX-`.js` retry. Only attempt the retry for JS-like modules that currently take the no-transform branch; leave `.ts`/`.tsx`/`.mts`/`.cts`/`.jsx`/JSON paths exactly as they are. A retry that still fails (e.g. Flow) returns the original parse error → graph fails → safe fallback (unchanged behavior).

- [ ] **Step 4: Run** `cargo test -p import-lens-daemon` → both new tests green, all existing graph/bundle/analyze tests unchanged. Clippy clean.

- [ ] **Step 5: Commit**

```bash
git add daemon/src/pipeline/graph.rs daemon/tests/graph.rs
git commit -m "fix(graph): analyze plain JSX shipped in .js package modules" -m "A package whose .js entry ships untranspiled JSX (common outside strict-ESM libraries) failed to parse in the module graph and fell back to low-confidence static-entry sizing. Retry JS-like modules that fail a plain parse as JSX and transform them so the bundler/minifier still see JSX-free source. Flow-typed .js remains unsupported by oxc and continues to fall back safely (no panic), so React Native core is out of scope."
```

---

### Task B5 (OPTIONAL — opportunistic): DF-12 bundle/minify micro-items

> Marginal; do only if touching this code anyway. Two independent sub-items, each its own commit if done. (DF-7 was deferred to Plan D — see verification notes.)

**B5a — `sanitize_identifier` collision guard.** Distinct module-local names differing only in same-position non-ASCII bytes sanitize to the same `__il_m{id}_` binding. Fix only with evidence:

- [ ] Write a bundle test with two exports in one module differing only in a non-ASCII byte (construct a true collision like `café`/`cafÉ` as two exported locals) and assert the generated bindings differ. If it does not reproduce a real collision, **drop this item** (do not add speculative hashing). If it does, append a short content hash to the sanitized suffix only when the sanitized form lost information (a non-ASCII byte was replaced).

**B5b — minify Transformer pass.** `minify_source_inner` runs a `Transformer` over already-transformed, plain-ESM bundle source.

- [ ] Investigate whether the pass is a no-op for the bundler's output (which is always JSX/TS-free prepared source). Add a temporary timing probe; if removing it keeps every `tests/bundle.rs` and `tests/analyze.rs` assertion byte-identical AND measurably helps, remove it. If there is any output difference or no measurable gain, **keep it and record the finding** — do not remove on spec.

---

## Execution order & exit

B1 (baseline) → B2 (data) → B3 (the win; re-quote the bench) → B4 (DF-6). Stop there unless you want the optional tail; **B5 is explicitly optional** and if skipped leaves DF-12-bundle open in the backlog. DF-7 is deferred to Plan D. On completion, Plan A (resolver reuse + fingerprint gate) is next and reuses `tests/performance.rs` the same way.

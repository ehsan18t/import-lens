# Rolldown Qualification and Bundler Replacement Design

Status: **cutover complete** — §11 Phase 3 landed on 2026-07-11 (`233c25d`): Rolldown is
the only semantic bundler, the custom engine (`bundle.rs`/`reachability.rs`/`cjs.rs`/manual
`graph.rs`) and its tests are deleted, `ANALYZER_REVISION` moved to `rolldown1` — and
subsequently to **`rolldown2`** on 2026-07-12, when the post-cutover correctness fixes recorded
in §10.7 moved real numbers — direct
`oxc_ast`/`oxc_ast_visit`/`oxc_transformer` dependencies are removed, and the
README/SRS/skill describe the shipped architecture. Phase 4: the accuracy re-baseline is
green (2026-07-11, enforced fixtures; deltas 2.6–13% vs the esbuild oracle) and the §15
checklist holds, except that packaging, the daemon-hash refresh, and the VSIX size check
are deferred by owner direction (2026-07-11) and MUST run before any release because the
daemon binary changed. Previously: **accepted** — the §10 qualification gates passed on
2026-07-11 with measured results recorded in §10.7; proposed 2026-07-10 after a
validation pass against crates.io (rolldown 1.1.5, oxc 0.139.0, oxc_resolver 11.23.0),
the published Rolldown 1.1.5 API surface, and repo HEAD. This document replaces the
custom reference-closure/fixpoint proposal after verifying that Rolldown publishes an
embeddable Rust crate.

**Release amendments (2026-07-12, I16–I23).** A design interview against the independent release
review settled four decisions this document had never stated, and they overturn parts of it. Read the
amendments in §5.1 (I19), §6.3 (I20), §7.4 (I17, I18), §8.2 (I21), §8.4 (I22), §10.6 (I23) and §12
(I16) — **I20 supersedes an accepted consequence of I15, and I16 reverses §12's fallback
mechanism**, so the unamended text of those sections is no longer binding. The governing decisions
are recorded as ADRs, the vocabulary in the root `CONTEXT.md`, and the ordered work in
`2026-07-12-bundler-redesign-release-plan.md`:

- [ADR-0001](../../adr/0001-measure-a-neutral-build.md) — measure a neutral build (not production,
  not development): our accuracy oracle is esbuild, which injects no `NODE_ENV` define either, so
  neutrality is what keeps the two comparable.
- [ADR-0002](../../adr/0002-upstream-owns-everything-it-can-answer.md) — Rolldown and OXC answer
  everything they can; hand-written logic is the last resort.
- [ADR-0003](../../adr/0003-no-size-without-a-build.md) — if Rolldown did not build it, we report no
  size.
- [ADR-0004](../../adr/0004-import-lens-measures-imports-not-bundles.md) — Import Lens measures
  imports, not bundles.
- [ADR-0005](../../adr/0005-a-runtime-is-an-artifact-boundary.md) — a runtime is an artifact
  boundary.

## 1. Decision summary

Import Lens should stop implementing JavaScript module linking and tree-shaking semantics
itself if the published Rolldown Rust crate passes the correctness, supported-public-API,
performance, memory, and stability gates in this document.

The target split of responsibility is:

| Responsibility | Owner |
| --- | --- |
| Parse imports from the user's document | existing OXC document pipeline |
| Resolve the requested package and build cache identity before bundling | existing direct `oxc_resolver` pipeline |
| Resolve, load, and link the transitive ESM/CJS graph | Rolldown |
| Bind imports, exports, re-exports, namespaces, and live references | Rolldown |
| Interpret package `sideEffects` and decide statement/module retention | Rolldown |
| Deconflict symbols and emit one linked ESM chunk | Rolldown |
| Produce both raw and minified measurements | Import Lens orchestration over Rolldown output and `oxc_minifier` |
| Compute gzip, Brotli, and zstd sizes | existing compression pipeline |
| Cache identity, freshness, persistence, diagnostics, and IPC | Import Lens |

OXC remains the compiler toolchain. It exposes the parser, resolver, semantic analysis,
minifier, and code generator used directly by the daemon, but it does not expose a standalone
cross-module bundler. OXC's own overview describes a suite of compiler tools and identifies
Rolldown as the bundler built on those tools:

- <https://oxc.rs/docs/guide/what-is-oxc>
- <https://rolldown.rs/development-guide/repo-structure>

Rolldown 1.1.5 publishes `Bundler`, `BundlerBuilder`, `BundlerOptions`, native plugin hooks,
in-memory `generate`, output chunks with export lists, rendered-module metadata
(`RenderedModule::rendered_length`), and configurable tree-shaking — all verified against the
published 1.1.5 crate sources. Its own manifest declares compatible OXC ranges (`^0.139.0`),
and its `rolldown_resolver` workspace crate declares the `oxc_resolver` range (`^11.23.0`);
nothing in the published stack exact-pins the versions selected by Import Lens. Cargo
resolution verifies that Rolldown 1.1.5 is compatible with the qualification stack selected
in §4 — the same stack the daemon already ships today — and Import Lens locks that complete
resolved stack itself.

This is not an unconditional dependency decision. Rolldown's official Rust-crate policy says
that its Rust API does not follow semver, does not receive a documentation guarantee, and
Rust-only issues are not maintained by the core team:

- <https://rolldown.rs/apis/rust-crates>

Import Lens therefore adopts Rolldown only behind exact direct version pins, a locked and
verified compiler-stack graph, one narrow internal adapter, and repeatable qualification
gates. The adapter absorbs Rust API churn; Rolldown owns JavaScript bundling semantics.

## 2. Why the current engine must be replaced

Import Lens reports the size an import costs. The current ESM path manually builds a module
graph, computes export reachability, expands binding dependencies, rewrites modules, creates
namespace objects, deconflicts names, concatenates sources, and then invokes OXC's minifier.

That means the product owns the most failure-prone part of a bundler even though it already
depends on the compiler stack used by a production bundler.

### 2.1 Original evidence and the July 2026 fix campaign

A scratch closure check against real packages, measured before commit `7774469`, found
emitted reads of generated `__il_` bindings that no included module declared:

| package, requested export | undeclared generated identifiers (pre-campaign) |
| --- | --- |
| `css-tree`, `parse` | 15 |
| `date-fns`, `format` | 1 (`__il_m93_enUS`) |

The `date-fns` result under-reported by 33.2% against the reference bundle because one
required binding was absent. The test suite passed at the time because it asserted individual
rewriter cases, not the end-to-end closure of emitted references.

Eleven defects were initially traced to disagreements between three independent decisions:

1. `reachability.rs` decides which exported symbols are reachable.
2. `bundle.rs` decides which modules, declarations, and imports to include.
3. `bundle.rs` separately decides which statements to emit and how to rename their bindings.

Each decision hand-enumerates local exports, named re-exports, star re-exports, namespace
re-exports, namespace imports, side-effect statements, and declarations. Nothing forces the
three enumerations to agree.

A five-commit fix campaign has since landed on `main` (`7774469` through `f4460fa`):
re-export chain walking, side-effect statement retention, shared export enumeration,
namespace object materialization, and namespace member inlining. The bundle tests gained an
end-to-end `assert_no_dangling_il_bindings` closure helper. As of `f4460fa`:

- `date-fns`/`format` no longer dangles, and its deviation against esbuild fell from 33.2%
  to 8.7%;
- `css-tree`/`parse` still emits 4 undeclared bindings (down from 15), none of them
  namespace bindings, after a campaign aimed at exactly this fixture.

The campaign is itself evidence for replacement: each fix surfaced a further defect its
predecessor had masked (`0670690` records "Fixing that surfaced a third defect"; `3242208`
was found only by independent review of the preceding commit), and the four remaining
css-tree danglers belong to shapes no local test yet names.

### 2.2 Additional validation findings

Direct probes found more semantic failures after the original list was written. All four
were re-probed against HEAD (`f4460fa`) on 2026-07-10, after the fix campaign, and all four
still reproduce:

| construct | behavior at `f4460fa` | required behavior |
| --- | --- | --- |
| escaping namespace of an empty module | reads a generated namespace binding with no declaration, even after namespace materialization landed | emit a valid empty namespace or eliminate the read |
| unused exported effectful initializer | deletes `compute(dep)` and its dependency | retain the initializer and everything it reads |
| ambiguous `export *` providers | retains both providers without reporting ambiguity | produce a resolution error/diagnostic |
| explicit re-export from an external module | emits an empty (zero-byte) bundle | preserve the external re-export/import boundary |

The 36 bundle tests and 24 graph tests at `f4460fa` pass while all four failures remain. This
proves that adding more local cases to the same architecture is insufficient.

### 2.3 Structural root cause

The current engine contains fallbacks that convert internal disagreement into believable but
incorrect output:

```rust
// Inclusion cannot determine which imports matter, so it keeps them all.
let include_all_static_imports =
    next_keep_all || !module_has_reachable_export(module, reachable);

// Emission cannot resolve a binding, so it invents the name it expected.
.unwrap_or_else(|| module_binding_name(target_id, &binding.imported_name));
```

The first silently over-counts. The second can emit a reference that has no declaration and
silently under-count. Neither behavior is acceptable for a tool whose primary output is a
number.

`oxc_minifier` cannot repair this. The current bundler pins its retained exports before
minification, so the manual retention decision is already the answer. The minifier only
optimizes within the source it receives.

## 3. Goals and non-goals

### 3.1 Goals

- Remove Import Lens's ownership of cross-module linking and tree-shaking semantics.
- Correctly support ESM, CJS, TS/TSX/JSX, JSON, cycles, namespaces, re-exports, externals,
  package subpaths, and side effects through one bundler authority.
- Preserve the existing IPC response shape, cache identity model, compression formats,
  confidence metadata, module contributions, and `truly_treeshakeable` contract.
- Keep one in-memory build for file-level multi-import sizing so shared dependencies are
  counted once.
- Fail visibly and conservatively when the engine cannot produce a trustworthy bundle.
- Bound CPU, memory, source bytes, module count, and dependency API churn.
- Delete the old semantic bundler after cutover rather than maintaining two implementations.

### 3.2 Non-goals

- Changing the MessagePack protocol or TypeScript extension behavior.
- Redesigning the papaya/redb cache lifecycle, project shards, recency, eviction, SWR, or
  persistence format.
- Matching esbuild or Rolldown byte-for-byte. Import Lens retains its configured OXC
  minification and compression measurements.
- Using Rolldown's Node API, CLI, a subprocess, or JavaScript plugins.
- Importing private Rolldown modules, copying Rolldown's linker, or maintaining a fork.
- Shipping the candidate engine before qualification passes.
- Updating the SRS before this design is reviewed and approved.

The SRS's existing packaged-extension size cap remains unchanged, but it is deferred for this
work and is not a Rolldown qualification or adoption gate. This design prioritizes product
correctness, stability, and runtime performance. Distribution-size compliance is handled as a
separate release concern after the engine architecture is accepted. The headroom is measured,
not assumed: current per-target VSIXes are 3.0–3.5 MB against the 20 MB cap, with daemon
binaries at 4.9–6.9 MB, so Rolldown's added code cannot plausibly threaten the cap.

### 3.3 Evaluated alternatives

The alternatives were reviewed for supported embedding surface, ownership burden, fit with the
existing OXC architecture, correctness metadata, and runtime performance:

| candidate | finding | decision |
| --- | --- | --- |
| Rolldown | Embeddable Rust bundler in the OXC ecosystem; exposes linking, tree-shaking, native hooks, and rendered-module metadata. Its Rust API has no semver or maintenance guarantee. | qualify behind exact pins, a narrow adapter, and full regression gates |
| esbuild | Mature and fast, but its supported APIs are CLI, JavaScript, and Go; plugins require JavaScript or Go. Rust use would require a subprocess, embedded Go, or an unsupported wrapper. | use only as a correctness/performance oracle |
| SWC bundler | Exposes a Rust `Bundler<L, R>`, but the caller must provide `Load` and `Resolve` implementations and own more AST, resolution, minification, and output orchestration. | reject because it recreates too much custom bundler glue and introduces a second compiler stack |
| Rspack | Fast Rust bundler presented as a broad webpack-compatible build system powered by SWC, with substantially more compiler and plugin surface than a one-entry measurement engine needs. | reject for integration scope and OXC misalignment |
| Farm | Full multi-asset Rust web build pipeline with a large default-plugin surface and toolchain requirements that do not fit this stable-toolchain OXC daemon. | reject for integration scope and toolchain misalignment |

Primary sources:

- <https://rolldown.rs/guide/introduction>
- <https://rolldown.rs/apis/rust-crates>
- <https://esbuild.github.io/api/>
- <https://esbuild.github.io/plugins/>
- <https://rustdoc.swc.rs/swc_bundler/struct.Bundler.html>
- <https://github.com/web-infra-dev/rspack>
- <https://farm-fe.github.io/docs/why-farm/>

Alternatives are reconsidered only if Rolldown fails correctness, supported-public-API,
stability, or absolute product-performance gates. Rejection does not authorize a return to a
custom semantic bundler.

## 4. Dependency and maintenance policy

### 4.1 Exact versioning

The qualification candidate uses this coordinated stack:

| package set | selected version | manifest requirement |
| --- | --- | --- |
| `rolldown` | 1.1.5 | exact `=1.1.5` |
| every direct OXC monorepo crate | 0.139.0 | exact `=0.139.0` |
| `oxc_resolver` | 11.23.0 | exact `=11.23.0` |

During qualification it is an optional normal dependency behind a non-default
`rolldown-candidate` Cargo feature. This permits release-mode candidate daemon measurement
without changing the dependency graph or behavior of the normally shipped binary. It becomes
an unconditional production dependency only after every gate in §10 passes.

Exact direct pins are mandatory because Rolldown's published Rust API has no semver guarantee
and its internal dependencies use compatible ranges. Broad or patch-compatible direct ranges
such as `^1.1`, `~1.1.5`, `~0.139.0`, or `>=1.1.5` are prohibited. `Cargo.lock` and the graph
validation in §4.4 control transitive packages that cannot be exact-pinned through the top-level
Rolldown declaration alone.

### 4.2 Adapter isolation

Only the engine adapter may import Rolldown types. `analyze.rs`, `file_size.rs`, service code,
cache code, and IPC types communicate through Import Lens-owned request, artifact, and error
types.

No public or persistent type may contain a Rolldown type. An upgrade that changes the
Rolldown Rust API must therefore be confined to the adapter and its plugin unless its
behavioral output intentionally changes.

Every adapter responsibility requires an explicit product reason. If a supported public
Rolldown or OXC API performs the work, the adapter delegates to it rather than duplicating its
logic. Import Lens may generate a virtual entry, map pre-resolved roots, enforce product
resource limits, collect loaded paths, translate public output, and map typed failures. It may
not implement resolver, package-side-effect, binding, liveness, interop, or emission semantics.

Direct OXC use remains intentionally narrow:

- `oxc_allocator`, `oxc_parser`, `oxc_span`, and `oxc_syntax` parse imports and completion
  context from the user's open document;
- `oxc_resolver` resolves the root request before bundling so cache identity and fast cache
  hits do not require constructing Rolldown;
- `oxc_allocator`, `oxc_parser`, `oxc_semantic`, `oxc_minifier`, `oxc_codegen`, and `oxc_span`
  validate and minify Rolldown's one unminified linked chunk.

After cutover, direct `oxc_ast`, `oxc_ast_visit`, and `oxc_transformer` dependencies are
removed with the marker rewriter and manual graph. They may remain transitive implementation
details of OXC or Rolldown; Import Lens does not import or version them independently unless a
new reviewed product requirement needs their public API.

### 4.3 Compiler-stack source of truth and updater

The current OXC-only configuration and updater become one compiler-stack workflow covering:

- the selected exact Rolldown version;
- all retained direct OXC monorepo crates at one exact version;
- the exact `oxc_resolver` version;
- the resolved Rolldown workspace crates and OXC packages reachable from Rolldown.

The future command is:

```text
pnpm deps:update:compiler --rolldown <version> [--oxc <version>] [--resolver <version>] [--dry-run]
```

The existing `deps:update:oxc` command and OXC-only configuration names are replaced rather
than kept as aliases. Contributor documentation, tests, and automation move atomically to the
new command. That includes the `oxc-upgrade` skill (`.claude/skills/oxc-upgrade/`): in the
same change it is renamed to a compiler-stack upgrade skill (working name
`compiler-stack-upgrade`) and rewritten around the coordinated workflow — Rolldown release
review first, then the OXC/resolver changelog review it already teaches, the
`deps:update:compiler` flow, and the §10 requalification gates. It is not updated before the
new command exists; a skill documenting a command that is not there yet is exactly the
stale-guidance failure commit `02b3368` fixed.

Cargo's resolver is the compatibility authority. The updater creates a temporary manifest,
exact-pins the requested Rolldown version, and runs Cargo resolution outside the repository
before editing tracked files. It must not parse or intersect Cargo version requirements using
hand-written semver code.

When `--oxc` or `--resolver` is omitted, the updater derives the highest compatible stable
version selected by Cargo for the requested Rolldown release. Explicit overrides are added as
exact constraints to the temporary manifest; an unsatisfiable graph is rejected before edits.
The updater also verifies that every retained direct OXC crate exists at the selected monorepo
version.

A successful non-dry update changes the compiler-stack configuration, exact manifest pins,
`Cargo.lock`, affected version documentation, and a generated fingerprint of the sorted
Rolldown/OXC package name, version, and source tuples reachable from the top-level Rolldown
package. The fingerprint is generated data and is never edited manually. `--dry-run` performs
the same resolution and validation but writes no repository file or lockfile.

### 4.4 Locked graph and dependency automation

CI recomputes the compiler-stack fingerprint from `cargo metadata --locked` and compares it
with the configured fingerprint, exact direct manifest pins, configured versions, and the
resolved graph. Validation fails on any uncoordinated direct version, unexpected duplicate
version of a coordinated OXC crate, independently moved Rolldown workspace crate, resolver
drift, or stale fingerprint.

All build, test, benchmark, coverage, packaging, and CI Cargo commands use `--locked`. Only a
dependency-update command may rewrite `Cargo.lock`.

`deps:update:safe` may update other pnpm and Cargo dependencies, but before success it restores
every compiler-stack package to the exact package/version set recorded by the compiler-stack
configuration, regenerates the fingerprint, and runs the locked graph validation. If the final
graph differs, the command fails without presenting the update as safe.

The contributor rule that currently permits hard-coded dependency versions only for OXC
coordination expands to the coordinated Rolldown/OXC compiler stack. Tests derive selected
versions from the compiler-stack configuration rather than copying version literals.

### 4.5 TypeScript build-time Rolldown

The extension manifest has no direct `rolldown` dependency. The `tsdown` development dependency
brings its own Rolldown transitively and uses it only to produce the TypeScript extension
bundle. It does not analyze packages, run in the extension host, or share a binary/API boundary
with the Rust daemon.

The TypeScript and Rust Rolldown versions are intentionally independent. Keep `tsdown`, do not
add a direct npm `rolldown` dependency, and do not force its transitive version to match the
Rust crate. `pnpm-lock.yaml` plus frozen installs provides reproducibility for that build-time
stack.

The production migration removes the stale `oxc-parser` entry from `tsdown.config.ts`'s
`neverBundle` list. A manifest drift test ensures neither `rolldown` nor `oxc-parser` becomes a
direct extension-host runtime dependency; it permits Rolldown only as a transitive dependency
of build tooling.

### 4.6 Upgrade policy

After adoption, a Rolldown upgrade must:

1. select an exact Rolldown version through `deps:update:compiler`;
2. let Cargo derive or validate the exact compatible OXC and resolver versions;
3. update and verify the complete compiler-stack graph and generated fingerprint;
4. compile all six target binaries with the committed lockfile;
5. rerun the complete construct, package, accuracy, absolute performance, memory, and
   concurrency gates;
6. bump `ANALYZER_REVISION` if any measured output can change.

Automatic dependency updates must not move Rolldown, its workspace crates, OXC, or
`oxc_resolver` independently.

Rolldown's caret requirements cap the stack at the OXC minor and `oxc_resolver` major it was
released against, so Import Lens can no longer move OXC or the resolver ahead of Rolldown:
the OXC upgrade cadence becomes bounded by Rolldown releases. Current evidence says the bound
is tight — Rolldown has released weekly, and 1.1.5 followed oxc 0.139.0 by two days — but a
stalled Rolldown release now stalls the whole compiler stack, and the updater must reject
such an upgrade request rather than split the stack.

## 5. Stable engine contract

The following types describe behavior, not a requirement to place everything in one Rust
file. Production types, the adapter, virtual-entry construction, and plugin state should
remain separate modules.

```rust
struct BundleRequest {
    entries: Vec<BundleEntry>,
    runtime: ImportRuntime,
    purpose: BundlePurpose,
}

struct BundleEntry {
    entry_path: PathBuf,
    package_root: PathBuf,
    selection: BundleSelection,
    reported_side_effects: SideEffectsMode,
}

enum BundleSelection {
    Named(Vec<String>),
    Default,
    Namespace,
    Full,
}

enum BundlePurpose {
    ImportSize,
    FileSize,
    FullPackageComparison,
    ExportEnumeration,
}

struct BundleArtifact {
    code: String,
    loaded_paths: Vec<PathBuf>,
    contributions: Vec<ModuleContribution>,
    exported_names: Vec<String>,
    diagnostics: Vec<ImportDiagnostic>,
    matched_side_effect_paths: Vec<PathBuf>,
}

struct BundleFailure {
    stage: String,
    message: String,
    diagnostics: Vec<ImportDiagnostic>,
    loaded_paths: Vec<PathBuf>,
}
```

The engine exposes two async operations:

```rust
trait BundlingEngine {
    async fn bundle(&self, request: BundleRequest) -> Result<BundleArtifact, BundleFailure>;
    async fn enumerate_exports(
        &self,
        request: ExportEnumerationRequest,
    ) -> Result<ExportEnumeration, BundleFailure>;
}

struct ExportEnumeration {
    names: Vec<String>,
    diagnostics: Vec<ImportDiagnostic>,
    read_time_fingerprints: Vec<FileFingerprint>,
    unhashed_paths: Vec<PathBuf>,
}
```

Enumeration returns a struct rather than a bare `Vec<String>` for two reasons, both
found in post-cutover verification:

- **Diagnostics on the success path.** §8.4 wants a successful enumeration's warnings
  surfaced, but a `Vec<String>` had nowhere to put them, so they were dropped. (A
  *missing* or *ambiguous* export is reported by Rolldown as an error and always
  reached the user; only true warnings were lost.)
- **Freshness inputs.** The read-time fingerprints let the caller memoize an
  enumeration instead of running a full engine build of the whole package graph on
  every completion popup, and expire it exactly when the files it was derived from
  change. `unhashed_paths` being non-empty means the enumeration must not be cached —
  there is nothing to expire it against.

Exact Rust trait syntax may use boxed futures if required by object safety. The semantic
contract must not change.

### 5.1 Contract invariants

- `code` is one complete, parseable, unminified ESM chunk.
- `loaded_paths` contains every internal real file loaded during the scan, including modules
  later removed by tree-shaking. It is canonicalized, sorted, and deduplicated.
- `contributions` contains only modules rendered into the output and uses Rolldown's rendered
  module length. It remains a pre-minification approximation.
- `exported_names` comes from the entry chunk's public export list, not a second custom star
  export walker.
- `reported_side_effects` and `matched_side_effect_paths` are reporting/confidence metadata.
  They may use Rolldown/`oxc_resolver` public package metadata but never override Rolldown's
  semantic side-effect decision.
- Diagnostics contain no Rolldown-owned types or unstable debug representations.
- A failure never returns partially linked code for measurement.

**Amended 2026-07-12 (I19) — an emitted asset is not a failure.** The adapter enforces "exactly one
chunk **and no assets**" and fails the build otherwise. The single-*chunk* half is a real invariant:
it is what stops code-splitting from silently under-reporting. The *no-assets* half was never an
invariant, only an assumption that nothing but JavaScript exists. The load hook deliberately lets
Rolldown infer module type from the extension, so a `.css` module becomes `ModuleType::Css`, Rolldown
emits it as an asset, and **the build fails**. Every CSS-shipping package — `swiper`,
`react-datepicker`, `react-toastify`, and most UI kits — is therefore unbuildable today. It has been
masked because the failure degraded to the entry-file fallback and produced a plausible number; under
the I16 amendment it would produce a blank row instead. The regression is created by I16, so the floor
is part of the same release.

The no-assets clause is dropped. The single-chunk guard stays. The JavaScript chunk is measured as
before, and the build **emits a diagnostic naming the non-JavaScript bytes it did not count**.

Folding those bytes into the Import Cost is the correct end state — CSS, wasm and fonts shipped by a
package are real bytes in a real bundle, and a user comparing two datepickers is otherwise told the
one shipping heavy CSS is the cheaper one. It is deferred, not declined: it changes this contract
(`BundleArtifact` must carry assets), both pipelines, the module breakdown, and the esbuild oracle,
and it moves numbers on a whole category of packages. Compression follows the I20 artifact rule — an
asset is a separate artifact from the JavaScript chunk, so it is compressed on its own and summed.
Until then the gap is **disclosed on the result**, never silently omitted.

## 6. Virtual entry design

### 6.1 Resolved targets

Import Lens continues resolving each requested package from the active document path through
the shared direct `oxc_resolver` pipeline for package identity, runtime conditions, type-only
detection, fallback context, and cache lookup before any Rolldown build exists.

The virtual entry does not repeat bare-package root resolution. For each `BundleEntry`, the
plugin exposes a stable synthetic target such as `import-lens:target/0` and resolves it to the
already-selected absolute `entry_path`.

This avoids:

- disagreement between two root package resolutions;
- Windows path escaping inside JavaScript source;
- accidental selection of a different package copy in nested workspaces;
- reinterpreting the original package export map after cache identity was established.

Rolldown exclusively resolves all transitive imports using the runtime-specific resolve
configuration. Import Lens does not apply its legacy module-graph resolver to a transitive
request.

### 6.2 Generated forms

Every retained output is given a valid, unique entry export so tree-shaking cannot remove the
requested surface. User-controlled names and module specifiers are serialized, never
interpolated without escaping.

Named selection:

```js
export { requestedName as __il_entry_0_export_0 } from "import-lens:target/0";
```

String-literal export name:

```js
export { "a-b" as __il_entry_0_export_0 } from "import-lens:target/0";
```

Default selection:

```js
export { default as __il_entry_0_default } from "import-lens:target/0";
```

Namespace and full selection:

```js
import * as __il_entry_0_namespace from "import-lens:target/0";
export { __il_entry_0_namespace };
```

The escaping namespace form is intentional. `export * from` excludes the target's default
export and is therefore not a correct model of `import * as ns` or the full package surface.

Dynamic-import sizing maps to `Full`: Import Lens measures the complete asynchronously loaded
module cost, but the measurement build remains a single static virtual entry. It does not
create runtime code splitting. Transitive dynamic imports inside dependencies must inline
into the same single chunk (§7.1); a lazy-loading package must not silently fall to the
conservative fallback through the one-chunk rule.

### 6.3 Multi-import file sizing

`file_size.rs` supplies all resolved requests in one `BundleRequest`. The virtual entry emits
unique aliases for every requested selection. Rolldown sees one graph and deduplicates shared
modules naturally.

The adapter must not concatenate independently generated package bundles. That would duplicate
shared dependencies, recreate symbol-boundary problems, and make `shared_bytes` unreliable.

**Amended 2026-07-12 (I15) — one build per _runtime_, not one build per document.** The rule
above is correct *within* a runtime and wrong *across* runtimes. A `BundleRequest` carries a
single `ImportRuntime`, and Client and Server resolve dependencies under materially different
conditions (different `condition_names`, `main_fields`, and `alias_fields`). A single Astro or
Svelte file legitimately mixes both — frontmatter is Server, a processed `<script>` is Client —
and forcing them into one graph would resolve one of the two under the wrong conditions, which
is a correctness failure, not an optimization.

The shipped behavior is therefore: group a document's resolved imports **by runtime**, issue one
multi-entry `BundleRequest` per group, and sum the groups. Shared-module deduplication is real
*within* a group, which is what the anti-concatenation rule above exists to protect. It does not
apply *across* groups, and it should not: a package imported under two runtimes genuinely ships
twice.

Two consequences are accepted with this amendment:

- **The compressed totals are a lower bound when a document mixes runtimes.** The groups'
  minified outputs are concatenated before a single compression pass, so redundancy between the
  Client and Server groups is compressed away exactly once, whereas the two bundles that really
  ship would each pay for it. Compressing the groups separately and adding them would be a
  different lie (compression is not additive), so neither figure is exact. The lower bound is the
  honest choice; it must not be presented as the file's exact transfer cost.
- **`shared_bytes` must be partitioned by runtime.** Reporting a module as "shared" across two
  runtime groups claims a deduplication that this amendment explicitly says does not happen.

**Amended 2026-07-12 (I20) — a runtime is an artifact boundary; compress each group and sum.** This
*supersedes I15's first accepted consequence*, whose reasoning does not hold. I15 argued that summing
separately-compressed groups "would be a different lie (compression is not additive)". Compression
non-additivity is real, but it applies to parts that would in reality be compressed **together**.
Two runtime groups are not such parts: they are two artifacts that genuinely ship, and each is
genuinely compressed on its own. Summing their separately-compressed sizes therefore **models reality
exactly**; it is the concatenation that distorts it, by compressing away redundancy between two
payloads that never meet. I15 chose the one figure that corresponds to nothing.

The rule, stated once and applied everywhere: **compressed bytes may be summed across an artifact
boundary and never within one.** A runtime is such a boundary; so is a non-JavaScript asset (I19).
Per-import figures within a runtime are not, which is why they may never be summed into a "total"
(§8.2 aggregation rule).

This is no longer only a reporting concern. The per-file budget gates on this number: it previously
re-derived a file total by *summing per-import* bytes and so raised "budget exceeded" diagnostics on
files that were inside budget, while the correctly-deduplicated figure sat on screen in the status
bar. The budget now consumes the file-size result, so the file-size result must be right. See
[ADR-0005](../../adr/0005-a-runtime-is-an-artifact-boundary.md) and
[ADR-0004](../../adr/0004-import-lens-measures-imports-not-bundles.md).

I15's second consequence — partitioning `shared_bytes` by runtime — was stated as a requirement and
never implemented: sharing is still counted across every result with no runtime partition, and the
extension renders it as a savings insight. A package imported from both Astro frontmatter and a
client script is sold to the user as a shared dependency when each runtime ships its own copy. That
lands with this amendment.

**Amended 2026-07-12 (I14) — the combined build does not record `loaded_paths`.** The
dependency-path index is keyed by `(entry_path, runtime)` and feeds per-entry freshness. The
combined file-size build's `loaded_paths` is the *union* over every entry in the group, so writing
it under each entry's key would clobber the accurate per-entry sets produced by the single-import
path. The combined build therefore records nothing; the per-import path remains the sole writer.

## 7. Rolldown configuration and plugin responsibilities

### 7.1 Fixed build options

The accepted adapter uses these behaviors:

- output format: ESM;
- entry signatures: strict;
- source maps: disabled;
- code splitting: disabled through the public code-splitting mode option, so dynamic imports
  inline into the single chunk (the 1.1.5 Rust API has no separate inline-dynamic-imports
  option);
- minification: disabled;
- one virtual user entry;
- existing component/client/server condition names and main-field priorities mapped into
  Rolldown resolve options;
- builtins and unresolved peers remain external and produce structured diagnostics where
  appropriate.

The build must produce exactly one JavaScript chunk and no unexpected emitted assets. A
different output shape is a typed `output_shape` failure and takes the conservative fallback.

### 7.2 Native plugin

A small native Rust plugin has three responsibilities only:

1. resolve and load the virtual entry;
2. resolve each synthetic target to its pre-resolved real entry;
3. record resolved/loaded real paths and enforce product resource limits.

When the plugin must delegate ordinary resolution, it calls the public plugin context resolver
with self-skipping enabled. It must not reproduce Node/package resolution rules.

The plugin must not inspect OXC AST nodes, classify package `sideEffects`, match package globs,
bind imports/exports, determine statement liveness, implement ESM/CJS interop, rename symbols,
create namespace objects, or rewrite real module source. Those are Rolldown's responsibilities.
Output translation and typed error mapping happen in the adapter outside the plugin.

### 7.3 Hard limits

The existing limits remain hard failures:

- maximum 2,000 internal modules;
- maximum 20 MiB for one module source;
- maximum 100 MiB of internal module source across a build.

Resolution/load hooks reject an oversized real file before returning its source when possible.
The module-parsed hook counts unique internal modules. Counters exclude the virtual entry and
external modules.

Limit state is per build, thread-safe, and monotonic. Exceeding a limit returns a structured
`module_graph_limit` failure; it must not panic or continue with a partial graph.

### 7.4 Side effects

Rolldown is the only semantic authority for statement and module retention. It uses its native
resolver's nearest-package metadata, built-in `package.json#sideEffects` boolean/string/array
handling, source-level side-effect analysis, and configured public tree-shaking options.

The plugin returns no `HookSideEffects` override for real modules. It never forces
`NoTreeshake`, implements a glob matcher, applies the root package's metadata to a transitive
package, or substitutes an Import Lens purity decision.

Import Lens retains root `SideEffectsMode`, the public `side_effects` field,
`truly_treeshakeable`, and matched-path diagnostics only as product metadata. Where matched
paths are available, the adapter obtains them through Rolldown/`oxc_resolver` public package
metadata behavior rather than its own matcher. This reporting data cannot change what code
Rolldown retains. Missing reporting metadata yields a conservative confidence diagnostic, not
a semantic override.

Qualification covers `sideEffects` false, true, missing, invalid, string, arrays, and nearest
transitive package metadata so the public reporting contract is tested against the code
Rolldown actually emits.

**Amended 2026-07-12 (I17) — the entry module never reached Rolldown's nearest-package
metadata.** The first paragraph above is false for the one module that matters most. The plugin
returns `HookResolveIdOutput::from_id(target)` with no `package_json_path`, and for a
plugin-resolved id Rolldown builds `ResolvedId.package_json` **only** from that field — so the entry
gets `package_json: None` and its side-effect classification falls back to pure source analysis.
Every *transitive* module is resolved by Rolldown itself and does get its metadata; the entry is the
sole hole, and the entry is the file every measurement is rooted at. A package declaring
`"sideEffects": false` whose entry has top-level statements source analysis cannot prove pure
(`Object.freeze(...)`, a prototype patch, a self-registration call) keeps those statements and
everything they reach; Rollup and webpack drop them, and the reported size is inflated.

The fix is to supply `package_json_path = package_root/package.json` — **metadata supply, not a
semantic override**, and therefore squarely permitted by this section and §14.6. `package_root` is
already carried on `BundleEntry` and used only as `cwd`.

This stayed invisible because every side-effects matrix row builds a **workspace-root `entry.js`**
that does `import 'testpkg'`, making `testpkg` transitive and correctly metadata-bearing. Production
is the opposite shape: the user imports `date-fns`, so the entry *is* `node_modules/date-fns/…`,
resolved by the plugin — the exact path that loses its metadata. The rows proving "Rolldown owns
`sideEffects`" all exercise the one code path production never takes. Qualification must carry a
row whose `BundleEntry` points **into** a `node_modules` package.

**Amended 2026-07-12 (I18) — `side_effects` is a property of the import, and the matcher is
Rolldown's.** Two corrections:

1. `analyze.rs` computed `has_side_effects() || is_array()`. `has_side_effects()` already answers
   correctly for the array form — it consults the matched patterns — and the `|| is_array()`
   overrode that correct answer with `true` unconditionally, which gated off the full-package
   comparison and forced `truly_treeshakeable` to `false` **by construction**, then dropped
   confidence to Medium. `"sideEffects": ["**/*.css"]` is an everyday declaration, so every such
   package was reported side-effectful and never tree-shakeable even where Rolldown demonstrably
   tree-shook it. The code justified this with "glob matching unavailable from public bundler
   metadata" — the premise **retracted by §10.7 divergence 1**, which matrix rows 42/43 now
   disprove. The override is removed. **Side-Effectful means: does the entry being measured match a
   side-effect pattern.** For `["**/*.css"]` and a JavaScript entry that is `false`, and that is
   true.
2. This section forbids the plugin implementing "a glob matcher" and requires matched paths be
   obtained "through Rolldown/`oxc_resolver` public package metadata behavior rather than its own
   matcher" — yet `resolver.rs` carries ~80 hand-written lines of brace expansion and segment
   matching, while `fast-glob` (the matcher Rolldown itself uses, via
   `rolldown_utils::pattern_filter`) is already in `Cargo.lock`. The hand-written matcher is
   replaced by `fast_glob::glob_match`, and `fast-glob` is **exact-pinned into the compiler stack and
   its fingerprint**: its purpose is to agree with Rolldown, and a version skew would break that
   agreement silently. See
   [ADR-0002](../../adr/0002-upstream-owns-everything-it-can-answer.md).

Rolldown cannot supply the badge itself: in 1.1.5 `ModuleInfo` — everything a plugin may learn about
a module — carries no side-effect field, and `DeterminedSideEffects` reaches no output type. The
retained reader is therefore reporting-only, as the I9 amendment already requires. If a future
Rolldown exposes the classification, the reader is deleted, not kept.

Swapping matchers is a **behaviour change, not a refactor** — the two engines disagree on some
patterns, which is the reason for the swap. The real-package badge baseline (§10.6 amendment) must
exist first.

## 8. Output measurement and metadata

### 8.1 Raw and minified code

Rolldown emits one unminified linked ESM chunk. Its byte length is `raw_bytes`.

The OXC minification wrapper parses that plain-JavaScript linked chunk once, performs semantic
validation, invokes `oxc_minifier`, and emits through `oxc_codegen`. Rolldown has already
handled TS/TSX/JSX transformation during its build, so the post-link path does not run
`oxc_transformer`. This is orchestration around public OXC APIs, not manual tree-shaking.

Although Rolldown can minify its output, its public build result does not provide both the
unminified and minified chunk required by the product from one link pass: in 1.1.5,
`generate()` takes no per-call output options, minification is fixed in `BundlerOptions` at
construction, and each `generate()` call performs a fresh full build. Running Rolldown
twice—once unminified and once minified—is rejected because it would load and link the graph
twice, consume more CPU, and could make raw/minified measurements observe different filesystem
states. Direct use of OXC's public minifier is the smaller and faster boundary for this
two-measurement requirement.

The existing compression pipeline runs gzip, Brotli, and zstd over the one minified string.

### 8.2 Contributions

Rolldown output exposes rendered modules and their rendered lengths. The adapter maps each
real module to `ModuleContribution` and excludes:

- the virtual entry;
- Rolldown runtime-only virtual modules;
- external modules;
- loaded modules with zero rendered contribution.

Contribution lengths remain pre-final-minification approximations. They support module
breakdowns and cross-import shared-byte attribution but are not required to sum exactly to
the final chunk length because chunk glue and final minification are not attributable to one
module.

**Amended 2026-07-12 — aggregation rule.** This design specifies a *per-import* measurement and
stops at `ImportResult`. It never stated how those results may be combined, and consumers
(the workspace report's totals, the treemap denominator, the per-file budget check) filled the
gap by **summing per-import compressed bytes**. That is invalid, for two independent reasons,
and both over-count:

1. **Shared dependencies are counted once per import site.** Fifty files importing `react` sum to
   fifty Reacts; the bundle ships one.
2. **Compressed sizes are not additive.** `brotli(A) + brotli(B) > brotli(A ∪ B)` — compression
   exploits redundancy across the union that it cannot see when the parts are compressed
   separately. This holds *even when the two imports share nothing*.

The rule, binding on every consumer of `ImportResult`:

> **A per-import `raw`/`minified`/`gzip`/`brotli`/`zstd` figure may never be summed with another.**
> A truthful multi-import total comes from one engine build over the union of those imports —
> which is exactly what §6.3 file sizing exists to produce and what any such consumer must use.
> Where a union build is unavailable, the aggregate must be deduplicated by module contribution,
> or labelled as what it is (a sum of independent import costs — valid for *ranking*, meaningless
> as a bundle size) and never called a "total".

**Amended 2026-07-12 (I21) — of the rule's two escapes, we take the second. Deduplicating the
workspace total by module is *declined*, not deferred.** Import Lens measures **imports**, priced
against an otherwise-empty application. It has no model of "what is already in the bundle" and never
intended one; building the project-wide union that a deduplicated total requires is a different
product, to be decided on its own merits rather than smuggled in as a bug fix.

So the workspace report's headline keeps its arithmetic and loses its name. "Total Brotli" becomes
**Combined Import Cost** — the sum of independent Import Costs, which counts a dependency shared
across fifty files fifty times — and the report says so. It ranks and it apportions blame; it is not
a size. Treemap percentages become a share of that figure. The duplicate-imports table becomes
correct by label: `react` across fifty files genuinely *does* have a combined import cost of fifty
Reacts, and that is the point of the panel. The report's `duplicate_imports` and `shared_modules`
groups are therefore computed and deliberately **not** subtracted from the headline — that is
correct, not an oversight.

The per-file budget takes the *first* escape, because there a union build **is** available: it is
the §6.3 file-size result, already fetched and already displayed in the status bar. See
[ADR-0004](../../adr/0004-import-lens-measures-imports-not-bundles.md).

### 8.3 Dependency fingerprints

Freshness uses every real path loaded by Rolldown, not only rendered modules. This is required
because editing a previously tree-shaken module can change export resolution, side effects,
or future retention.

Package manifests used for resolution or side-effect classification are included alongside
source paths. Existing node_modules versus first-party fingerprint policy remains unchanged.

### 8.4 Export enumeration

Export enumeration uses the same engine with the resolved real entry as a strict entry and
reads the output chunk's export list. It must not call the old recursive
`module_exported_names`/`module_provides_export` logic.

Ambiguous star exports, missing requested exports, and external-only exports follow Rolldown's
resolution result and become structured diagnostics. They are never guessed.

**Amended 2026-07-12 (I22) — enumeration must carry the import's runtime, and its memo must expire
on manifests.** Two defects, both invisible because enumeration was hardcoded to a single runtime.

1. **The runtime is hardcoded to `Component`.** Component and Client resolve with
   `alias_fields = ["browser"]` and browser conditions; Server resolves with node conditions. In a
   Server-context file (Astro frontmatter is Server for sizing), a package whose `exports` map
   diverges across `node` and `browser` is enumerated under **browser** conditions while the *size*
   of that same import is correctly computed under **Server** — so the completion list omits names
   the file can import and offers names it cannot. Completions and sizing disagree by construction.

   The runtime is **derived in the daemon, not passed in** — one classifier, not two
   ([ADR-0002](../../adr/0002-upstream-owns-everything-it-can-answer.md)).
   `CompleteImportMembersRequest` already carries `source` and `cursor_offset`, and the document
   region classifier already maps an offset to a runtime, so it needs **no protocol change**;
   `EnumerateExportsRequest` gains the offset it lacks. The enumeration memo is already keyed by
   runtime, but production only ever wrote the `Component` key — that dimension comes alive here, so
   enumerations cached before this change must not be trusted after it.

2. **The memo ignores package manifests and has no TTL.** It stores only the enumeration's source-
   module fingerprints, while the size path deliberately adds the root and first-party manifests per
   §8.3. A first-party package under development whose `package.json` flips `"type": "module"` (or
   edits `exports`/`sideEffects`) moves no source file and bumps no cache generation — so the
   completion popup serves the **old export list indefinitely**. The manifests join the memo's
   fingerprints, as §8.3 already requires of everything else.

## 9. Concurrency and lifecycle integration

Rolldown's build API is async, uses Tokio for module loading, and uses Rayon internally for
CPU-heavy stages. The current service also uses the global Rayon pool to parallelize imports.
Calling one async Rolldown build from every outer Rayon worker risks starving or oversubscribing
the same CPU pool.

The target rule is:

> Rolldown builds run as async work with a daemon-wide concurrency limit. They are never
> invoked from an outer global-Rayon import loop.

The initial limit is two concurrent Rolldown builds per daemon. Cache hits bypass the limit
and do not construct a bundler. Batch and file-size results preserve input ordering even when
misses complete out of order. Streaming responses may continue to emit in completion order
with their existing indexes.

Blocking cache, fingerprint, OXC minifier, and compression work remains off the Tokio I/O
threads. Rolldown retains ownership of its internal Rayon parallelism.

This is an analysis-execution boundary change, not a cache lifecycle redesign. The following
remain unchanged:

- papaya and redb ownership;
- cache keys and persistent envelope schema;
- project shards and storage locations;
- recency, preload, prewarm, SWR, eviction, maintenance, and recycle policy;
- single-flight semantics.

Only the dependency-path producer and the analyzer revision integrate with the new engine.

## 10. Qualification before production adoption

Qualification is a separate phase. It adds a feature-gated candidate adapter and does not
change default production analysis, cache contents, or user-visible output. Candidate daemon
binaries are built explicitly with `rolldown-candidate` and are not published.

### 10.1 Candidate harness

The harness runs the current engine and Rolldown candidate in the same release build and
records:

- success/error class;
- emitted parse/semantic validity;
- requested export presence;
- observable side-effect fixtures;
- loaded paths and rendered contributions;
- raw/minified/compressed sizes;
- cold latency and batch latency;
- peak memory and concurrency behavior.

The current engine is an informational performance baseline only. Its output is not a
correctness oracle and its relative speed is not an adoption gate because it performs
incomplete work for known constructs. Correctness is asserted through explicit fixture
expectations and, where useful, a reference bundler comparison.

### 10.2 Required construct matrix

The repository must gain a table-driven matrix covering at least:

- local named/default exports and aliases;
- imported-then-exported bindings;
- direct named re-exports;
- single and chained `export *`;
- ambiguous star providers;
- `export * as namespace` and forwarded namespace exports;
- namespace static property reads, computed reads, optional reads, and escaping namespaces;
- empty namespace targets;
- string-literal import/export names;
- side-effect-only imports;
- pure and effectful unused declarations;
- exported and non-exported effectful initializers;
- top-level statements that declare no binding;
- cycles and shared diamond dependencies;
- external imports, external named re-exports, and external star re-exports;
- ESM/CJS interop and representative CJS export shapes;
- TS, TSX, JSX, JSON, `.mts`, and `.cts` inputs;
- symbol collisions, including source identifiers beginning with `__il_`;
- missing exports, parse failures, semantic failures, and every hard graph limit;
- named, default, namespace, dynamic/full, and combined multi-package requests;
- a transitive dynamic import inside a dependency, which must inline into the single measured
  chunk rather than split into a second chunk or take the fallback;
- `sideEffects` false, true, missing, invalid, string, arrays, and nearest transitive package
  metadata, using Rolldown's native interpretation without hook overrides.

The matrix described in the previous proposal was a scratch instrument, not a committed
repository test. Qualification is incomplete until it exists in the repository.

### 10.3 Real-package set

At minimum, qualification covers pinned local fixtures for:

- `css-tree` (pinned today at 3.2.1);
- `date-fns` (pinned today at 4.1.0);
- `lodash` (pinned today at 4.17.21, kept for real-package CJS coverage);
- `lodash-es`;
- `zod`;
- `react`;
- `uuid`.

The first three already exist in `scripts/accuracy-fixtures` with a committed lockfile; the
remaining packages join the same pinned-fixture mechanism. Installing fixtures from the
committed lockfile is an explicit setup step; qualification test execution itself performs no
network access.

### 10.4 Correctness gates

All of the following must pass:

- every construct produces the expected export, side-effect, or typed failure behavior;
- every successful emitted bundle parses and passes OXC semantic validation;
- no unresolved identifier with the virtual-entry `__il_entry_` prefix exists;
- the four remaining `css-tree` dangling references reach zero and the fixed `date-fns` case
  stays at zero;
- the empty namespace case emits valid output;
- the effectful-unused-initializer case retains its dependency;
- ambiguous star exports produce a typed error/diagnostic;
- explicit external re-exports remain represented in output;
- `loaded_paths` includes tree-shaken dependencies;
- contributions contain only rendered real modules;
- multi-import file sizing deduplicates shared modules;
- all graph limits and fallback stages are observable and deterministic;
- package-side-effect fixtures use Rolldown's native metadata/source analysis and no Import
  Lens `HookSideEffects` override or custom glob matcher;
- no private Rolldown API is used.

### 10.5 Compiler-stack and automation gates

All of the following must pass before the candidate result can be accepted:

- direct Rolldown, OXC monorepo, and `oxc_resolver` manifest requirements are exact and match
  the compiler-stack configuration;
- the candidate combination resolves successfully through the temporary Cargo manifest;
- incompatible explicit OXC/resolver combinations are rejected before tracked files change;
- omitted OXC/resolver arguments derive a Cargo-compatible stable selection;
- dry-run performs availability, compatibility, and graph validation without changing a
  manifest, configuration, documentation file, or lockfile;
- the generated fingerprint matches the Rolldown/OXC graph from `cargo metadata --locked`;
- simulated transitive Rolldown/OXC drift and duplicate coordinated OXC versions fail the
  graph check;
- `deps:update:safe` restores the recorded compiler stack and fails if restoration or final
  validation does not succeed;
- build, test, benchmark, coverage, packaging, and CI entry points invoke Cargo with
  `--locked`;
- the extension manifest has no direct `rolldown` or `oxc-parser` runtime dependency and no
  direct `rolldown` development dependency; transitive Rolldown through `tsdown` remains
  permitted.

### 10.6 Runtime performance and stability gates

Measurements use release builds on the same machine, with five warm-up runs followed by at
least 30 recorded runs.

Hard gates:

- single typical cold import p95 is at most 500 ms;
- cache-hit response remains below 50 ms;
- daemon startup remains below 500 ms;
- idle RSS remains below 100 MB;
- a 20-import active batch remains below 400 MB peak RSS;
- all six target daemon binaries compile successfully with the committed lockfile;
- repeated identical candidate runs produce deterministic size fields, loaded paths,
  contributions, and failure stages.

Candidate single-import and batch/file-size p95 are still compared with the current engine on
comparable successful fixtures. More than 15% regression triggers one focused optimization
pass and a recorded explanation, but it does not reject a candidate that passes the absolute
latency, memory, correctness, and stability gates. The old engine can be faster precisely
because it omits required code.

The comparison set includes a small ESM package, a wide barrel, a deep re-export graph, a CJS
package, a namespace/full request, 20 independent imports, 20 imports with shared dependencies,
and repeated different exports from one package.

**Amended 2026-07-12 (I23) — these gates have never run, and the claims they protect are not
instrumented at all.** Three holes, all in the instruments rather than the engine.

1. **The performance and memory gates above are dead code.** `candidate_performance.rs` is
   `#[ignore]`d and requires a fixtures env var, and **nothing invokes it** — no workflow step, no
   `package.json` script. The trap is that a perf gate *appears* to run: CI calls
   `pnpm test:performance`, which is the pre-existing legacy suite over synthetic fixtures, a
   different file entirely. So a Rolldown bump that doubles cold p95 or blows the 20-import RSS
   ceiling passes `deps:update:compiler`, passes `candidate_packages` (which asserts correctness,
   not timing), and ships green. The suite written precisely to prevent this has never executed.
   The pattern to copy already exists: the real-package correctness job installs fixtures and runs
   `-- --ignored`. `candidate_performance` joins it, gated on the absolute numbers above.

2. **No real package's *claims* are baselined anywhere.** The accuracy oracle checks **bytes**;
   nothing checks **badges**. Nothing in `scripts/` mentions `truly_treeshakeable`, and the
   real-package harness works at engine level and never produces an `ImportResult`, so the flag that
   moved most visibly at cutover has no ground truth. A pipeline-level harness over the pinned
   packages must assert `side_effects`, `truly_treeshakeable` and confidence. This is required
   **before** the I18 (side-effects semantics + `fast_glob`), I17 (entry manifest) and I20
   (per-runtime compression) amendments land — each moves real-package numbers or badges, and today
   nothing would detect a wrong move.

3. **The §10.3 real-package set contains no CSS-shipping package**, which is exactly why the I19
   asset defect survived qualification: every pinned fixture is pure JavaScript. A package that
   imports CSS (`swiper`, `react-datepicker`, `react-toastify`) joins the set, and the construct
   matrix gains a CSS row.

### 10.7 Gate outcome

If all gates pass, this document is updated with measured results and marked accepted before
production migration begins.

If a capability requires private Rolldown internals, a fork, or manual replacement of its
linker behavior, the candidate is rejected.

If an absolute latency, memory, concurrency, or stability gate fails, one bounded optimization
pass may adjust public options, adapter allocations, or the build-concurrency limit. If the
hard gates still fail, the candidate is rejected and production remains unchanged.

Rejection does not silently reactivate the old custom fixpoint proposal. A custom engine would
require a new reviewed design because it permanently accepts ownership of bundler semantics.

#### Qualification record (2026-07-11, win32-x64) — PASS, candidate accepted

Instruments: `daemon/tests/candidate_matrix.rs` (48 rows), `daemon/tests/candidate_packages.rs`
(7 pinned real packages), `daemon/tests/candidate_performance.rs` (release measurements), and
the compiler-stack automation suites landed at `b2da0f4`.

Correctness (§10.4): all gates pass. 45 matrix rows green; row 34 (total-source limit — the
only coverage of the `MAX_GRAPH_SOURCE_BYTES` accumulator) green when run explicitly. *(Row 34
has since moved to `daemon/tests/graph_source_limit.rs`, where an environment override shrinks
the ceiling so the branch runs by default; nothing about it is ignored any more.)* The
real-package suite is 7/7 with `css-tree/parse` emitting **zero** dangling `__il_` bindings
(the four §2.2 danglers reach zero and `date-fns` stays at zero), `date-fns/format` loading
304 paths while rendering 36 (tree-shaken freshness gate), and CJS `lodash/debounce` working
through link-time interop (whole-library retention as expected for CJS).

##### Re-baseline (2026-07-12, `rolldown2`) — post-verification sizes

The byte counts recorded on 2026-07-11 were measured by code that got them wrong. N2 was
billing Rolldown's `//#region` debug comments to the package, and N3 left the platform unset,
which made Rolldown append `browser` to every runtime's condition list and inject a
`process.env.NODE_ENV` define. Both are fixed; these are the numbers the shipped analyzer
now produces (`ANALYZER_REVISION = rolldown2`, which rejects every entry measured before them):

| package/export | raw bytes | rendered modules | loaded paths |
| --- | --- | --- | --- |
| `css-tree/parse` | 320,455 | 123 | 128 |
| `zod/z` | 450,089 | 78 | 79 |
| `lodash/debounce` (CJS) | 488,868 | 1 | 1 |
| `date-fns/format` | 75,898 | 36 | 304 |
| `react/useState` | 53,711 | 3 | 3 |
| `lodash-es/debounce` | 11,832 | 14 | 640 |
| `uuid/v4` | 1,498 | 3 | 20 |

Accuracy against esbuild (`pnpm test:accuracy`, brotli) is unchanged by the fixes — the
minifier already stripped the debug comments, and none of the fixtures carries a `browser`
condition — so the worst case stays `css-tree` at **13.0%**, inside the 25% tolerance.

Automation (§10.5): all gates pass via `scripts/test/update-compiler-stack.test.mjs` and
`scripts/test/compiler-stack-coordination.test.mjs` — exact family pins (rolldown,
rolldown_common, rolldown_error at one monorepo version), temp-manifest probe with sibling
pins and split-stack rejection before any tracked edit, dry-run purity, fingerprint drift
against `cargo metadata --locked`, `--locked` on every non-update entry point, and no direct
rolldown/oxc-parser in the extension manifest.

Performance and stability (§10.6, release build, 5 warm-ups + 30 recorded runs):

| measurement | result | gate |
| --- | --- | --- |
| cold `css-tree/parse` p50 / p95 / max | 30.2 ms / 52.4 ms / 53.3 ms | p95 ≤ 500 ms |
| current-engine p95 on the same fixture | 97.7 ms (candidate/current = 0.54) | ±15% note only |
| 20-import batch (2 concurrent), wall | 605 ms | — |
| 20-import batch peak RSS | 78 MB | < 400 MB |
| determinism (per-package byte-compare of code, loaded paths, contributions, exports; stable failure stages across repeated matrix runs) | pass | required |

**Caveat, resolved 2026-07-12.** These measurements were taken on a Tokio runtime sized to
`num_cpus`, while the shipped daemon built its engine runtime with `worker_threads(ENGINE_PERMITS)`
= 2 — so the table above did not describe production, and the "~2× faster per build" claim was
measured on a runtime the user never had. The runtime is now sized to `available_parallelism`
clamped to `[ENGINE_PERMITS, 8]`, with the permits (and so peak memory) untouched. Measured over
four real-package cold builds: **median 299 ms → 181 ms**. The record and production now agree.

The candidate is faster than the current engine, so the >15% regression clause is not in
play. Startup, idle RSS, and the cache-hit path are unchanged by construction in Phase 1:
the shipped binary does not compile the candidate feature and the default dependency graph
was verified unmoved (the lockfile delta is two direct-dependency edges, no version moves).
Platform scope: per owner direction (2026-07-11), the qualification compile proof covers
win32-x64 — the primary supported platform — only; the remaining targets are exercised by
the Part E release packaging of the default graph, and the candidate adds only pure-Rust
crates on top of it.

Known divergences accepted into the record:

1. ~~**rolldown 1.1.5 never matches string/array `sideEffects` globs on Windows**~~ —
   **RETRACTED 2026-07-12. This divergence does not exist.** It was a misdiagnosis, and
   both halves of the stated cause are refutable. Rolldown matches through `fast_glob`,
   which uses `std::path::is_separator` and deliberately accepts `\` for a pattern's `/`
   on Windows, so backslashes cannot be the cause; and the fixture's pattern resolved to
   `fx.js` at the package root — a path containing no separator on any platform — so
   neither the separator nor the zero-directory `**/name` form could be involved.

   Rows 42/43 failed because the fixture never reached the matcher. Its `entry.js` did a
   bare `import 'testpkg'`, and `index.js` is not in the `sideEffects` list, so the
   package entry is side-effect-free, the import is legitimately dropped, and `fx.js` is
   never resolved. The expectation was wrong, not the bundler.

   With the package entry kept alive, glob `sideEffects` matching is **correct on
   Windows**: `"./fx.js"`, `"fx.js"` and `"*.js"` all retain the matched effectful module
   and drop the unmatched pure one. Rows 42/43 now assert exactly that and run by default.

   Consequences for anything that cited this divergence: there is no Windows glob
   `sideEffects` size undercount, and no user-facing diagnostic should claim one. The
   product's own conservatism around array `sideEffects` (forcing `side_effects = true`
   for any array form, so the package loses its "truly tree-shakeable" badge) is a
   product-side choice, not a bundler defect, and is now unjustified by this record.
2. **CJS export enumeration yields `default` only** through the chunk export list, even for
   statically plain `exports.x =` assignments; named CJS access works at link time via
   interop (the `lodash/debounce` case). Matrix row 27 pins the behavior per §8.4's
   never-guess rule.
3. **Link-time constant inlining** renders trivial constants into their use sites, so such
   modules legitimately contribute zero bytes (§8.2 already excludes zero-length
   contributions); they remain in `loaded_paths` for freshness.
4. **Unresolved imports externalize with a warning** rather than failing the build; the
   boundary stays in the output with a structured diagnostic (matrix rows 24/25), which is
   the §2.2-required non-empty-chunk behavior.
5. **Internal ambiguous star exports** surface through the missing-export diagnostic with an
   ambiguity message; the adapter classifies these as `ambiguous_export` (matrix row 7).

## 11. Production migration after a passing gate

### Phase 0 — specification approval

- Review and approve this document.
- Update the SRS and compiler-stack dependency policy in a separate post-approval
  specifications change. Do not change the SRS in this design revision.
- Do not change production behavior or the analyzer revision.

### Phase 1 — qualification

- Replace OXC-only dependency coordination with the compiler-stack updater, exact direct pins,
  locked commands, generated graph fingerprint, and drift tests from §4 and §10.5.
- Rename and rewrite the `oxc-upgrade` skill into the compiler-stack upgrade skill in the same
  change that replaces the updater command (§4.3).
- Add the exact optional Rolldown dependency and non-default `rolldown-candidate` feature.
- Implement the candidate adapter/plugin and construct matrix outside the production path.
- Record correctness, latency, memory, concurrency, stability, and compiler-stack results.
- Update this document with the result and final concurrency measurement.

### Phase 2 — guarded production integration

- Move the exact Rolldown dependency into production.
- Integrate the stable engine contract with individual analysis, full-package comparison,
  export enumeration, prewarm, and combined file sizing.
- Move miss-producing analysis paths to the bounded async execution boundary.
- Preserve the existing conservative static fallback and structured error behavior.
- Keep the old engine available only in tests for temporary differential verification; it is
  not selected at runtime.

### Phase 3 — atomic cutover and cleanup

- Make Rolldown the only semantic bundler.
- Bump `ANALYZER_REVISION` in the same change. Do not bump it during qualification.
- Delete the old engine wholesale — nothing Rolldown now owns keeps a parallel Import Lens
  implementation: `pipeline/bundle.rs`, `pipeline/reachability.rs`, `pipeline/cjs.rs`, the
  manual module-graph construction in `pipeline/graph.rs`, the marker passes in `minify.rs`,
  `analyze.rs`, and `file_size.rs`, generated binding fabrication, namespace-object
  construction, ~~the package-side-effect matcher/override,~~ and bundling-only graph records.

  **Amended 2026-07-12 (I9).** The package-side-effect matcher is **retained**, as an
  explicit, recorded divergence. It does not survive as bundler semantics: it never reaches
  Rolldown, and it cannot change what is retained or what size is reported. It survives only
  as a *reporting* input on the **static-fallback** path — the path taken when the engine
  fails — where it drives the `side_effects` flag and the matched-path diagnostic. Deleting
  it outright would have been the cleaner gate, but §7.4 assumes matched paths are available
  through public bundler metadata, and **Rolldown 1.1.5 does not expose its retention
  decisions**, so the fallback would silently lose fidelity with nothing to replace it.
  Retention-neutral and reporting-only is the honest characterization; the alternative was to
  close the gate on paper by degrading a real diagnostic.
- Delete the old engine's tests with it: `daemon/tests/bundle.rs`, the bundling coverage in
  `daemon/tests/graph.rs`, and every other test that asserts custom linking or tree-shaking
  internals. After cutover, no Import Lens test re-verifies Rolldown-owned semantics; that
  coverage lives solely in the committed §10.2 construct matrix, which asserts the engine
  contract over Rolldown output.
- Remove direct `oxc_ast`, `oxc_ast_visit`, and `oxc_transformer` dependencies and the stale
  `oxc-parser` tsdown externalization; retain only the direct OXC responsibilities in §4.2.
- Relocate small non-bundling helpers before deleting their former modules.
- Remove the temporary differential engine/tests once replacement assertions cover the same
  behavior.
- Update the README in this same atomic cutover. Describe Rolldown, built on OXC, as the
  linking/tree-shaking engine and direct OXC as the document parser, root resolver, validator,
  and final minifier. Remove the custom-reachability and OXC-only claims, including the footer,
  while preserving privacy, local-only size analysis, caching, and compression descriptions.

### Phase 4 — release re-baseline

- Re-run real-package accuracy and `truly_treeshakeable` baselines.
- Run all Rust, TypeScript, script, performance, packaging, and hash checks with locked
  dependency installs. The existing SRS distribution-size cap remains a separate release
  concern, not a Rolldown adoption gate.
- Regenerate daemon hashes only after the final binaries are accepted.

There is no shipped dual-engine mode. After cutover, a Rolldown build failure takes the
existing conservative static fallback; it never invokes the deleted custom bundler.

## 12. Failure policy

Failures are typed by stage and surfaced through existing structured diagnostics.

| failure | behavior |
| --- | --- |
| root package cannot be resolved | existing package/type-only/static fallback behavior |
| Rolldown cannot resolve an internal import | preserve legitimate external when possible; otherwise conservative static fallback |
| missing or ambiguous requested export | error result with zero size fields, not a guessed binding |
| parse/transform/link/generate failure | conservative static fallback with stage diagnostic |
| unexpected output shape | conservative static fallback with `output_shape` diagnostic |
| graph limit exceeded | conservative static fallback with `module_graph_limit` diagnostic |
| OXC validation/minification failure after linking | conservative static fallback with the OXC stage diagnostic |
| compression failure | existing per-import computation error behavior |

No failure path may fabricate a symbol, measure partial linked code, or silently switch to an
unvalidated result.

**Amended 2026-07-12 (I16) — a failed build reports no size at all.** The "conservative static
fallback" above fabricates a number wherever no build succeeded, and the number is not an Import
Cost. Sizing the entry file alone understates by ignoring the entire graph it pulls in: a large UI
kit that breaches the graph limit was reported at the few kilobytes of its barrel, when the true
answer is megabytes. Sizing the package's directory understates nothing and overstates everything —
it counts tests, source maps, and unused files, unminified and uncompressed. A confidence badge does
not repair either; users read the byte count, and a number wrong by an order of magnitude while
looking like a measurement is worse than no number, because it is actionable and the action is
wrong.

Every row above reading *conservative static fallback* becomes **Unmeasured**: no size, a typed
stage, and the diagnostics that explain it. This is recorded as
[ADR-0003](../../adr/0003-no-size-without-a-build.md) and is not a tightening of §12's spirit but a
reversal of its mechanism — §12's own rule that no failure path may "silently switch to an
unvalidated result" is precisely what the fallback did.

Three consequences are accepted:

- Coverage drops. Imports whose manifest cannot be parsed, whose entry exceeds
  `MAX_MODULE_SOURCE_BYTES`, or whose build fails now report no size.
- The hand-written estimators (`approximate_directory_size`, `estimate_minified_source`) are
  deleted. If OXC cannot minify it, we do not guess — see
  [ADR-0002](../../adr/0002-upstream-owns-everything-it-can-answer.md).
- **The failure stage becomes the primary thing a user sees**, so it must be deterministic.
  `adapter.rs` selects the first non-`link` diagnostic in a vector Rolldown accumulates from
  concurrent module tasks, so identical inputs can report `parse` on one run and `resolve` on the
  next — and the value is cached. Stages are ranked by pipeline order, earliest wins. An Unmeasured
  import logs its **full diagnostic vector at `warn`**, not `debug`: after this amendment "error and
  no size" is the entire failure path, and the diagnostics *are* the answer.

An honest lower bound ("at least 4 MB; graph limit exceeded") is strictly better than either a
fabrication or a blank, since a limit breach means much of the graph was loaded before we stopped.
The engine currently discards the partial graph on failure, so this is the intended successor to
I16, not a hypothetical.

Because the stage is now the answer, the engine boundary contributes three stages of its own, and
all three are part of I16's vocabulary:

| stage | meaning |
| --- | --- |
| `panic` | a Rolldown/OXC panic unwound into the boundary's `catch_unwind`; Unmeasured for this import only |
| `timeout` | the build did not finish within `BUILD_TIMEOUT` (8s) and was cancelled. That is its whole meaning: one build, its own clock. It says nothing about how long a *document analysis* took, because that request no longer waits for the builds its imports miss on — but `AnalyzeSpecifiers` deliberately still does (SRS FR-004b), and there a parked build costs that command its full 8s |
| `engine_gone` | the engine runtime was torn down without replying — not a panic, and must not be counted as one |

`timeout` is not a slowness policy; it is the *only* containment for the panic class that matters.
Rolldown loads each module on a task it `tokio::spawn`s and contains no `catch_unwind` anywhere, so
a parse-path panic is swallowed by Tokio: the task never sends its completion message, the loader's
outstanding-module count never reaches zero, and — because the loader holds its own clone of the
message sender — its receive loop never sees a closed channel either. The build future parks
forever. Nothing unwinds, so `catch_unwind` never fires; the permit and the in-flight guard are
never released, and two such packages exhaust the permit pool and wedge every subsequent build until
the daemon restarts. Cancelling the future on timeout is what releases them. At ~120x the §10.6
cold-p95 gate the bound cannot fire on a legitimate build.

That bounds a *build*, and a build is all it needs to bound.

It cannot bound a *request* — a permit is acquired outside the timeout, so parked builds queue
rather than merely run late — and for one round it was made to try: each request carried an engine
**budget**, a deadline stamped on arrival, and a build admitted with nothing left of it abandoned
without running. That bounded the wall clock and paid for it in fabricated numbers: an import whose
build was abandoned reported the static fallback, which is not its size, and that number went into
the cache.

The bound belonged in the wrong place because the *response* was in the wrong shape. An interactive
document response was atomic — every import or none — so a single parked build pushed the whole
thing past the client's 10s deadline and the extension threw the document away, cached hits
included. Nothing about a build timeout can fix that; only not waiting can.

So the response no longer waits. `AnalyzeDocument` answers with what the cache holds plus a
`loading` placeholder per miss, and each miss is built off the response path and pushed to the
client as it lands (`RefreshedResults`). A parked build now delays one import's number and nothing
else — the budget's whole purpose, at none of its cost. It is deleted, and `BUILD_TIMEOUT` is the
one timeout left: it exists so a parked build cannot hold a permit forever, which is the only thing
it was ever able to promise.

## 13. Expected blast radius

The initial diff is larger than the custom fixpoint because this design removes whole
subsystems instead of modifying their retention rules.

Approximate line churn after a passing qualification:

| area | expected churn |
| --- | ---: |
| this design plus later SRS update | 900–1,300 lines |
| qualification adapter, fixtures, and benchmarks | 1,000–1,800 lines |
| production adapter/plugin and async integration | 1,000–2,000 lines |
| manual bundler/graph/CJS production deletion or simplification | 3,500–4,500 lines |
| test deletion, conversion, and replacement | 2,000–3,000 lines |
| total implementation churn | roughly 8,400–12,600 lines |

Expected net production size is **1,500–3,500 fewer lines** (the deletion row minus the
production-adapter row), even though churn is high. This
estimate is retained as a directional outcome and is rechecked after qualification rather than
used as an adoption gate.

Primary production impact is limited to the Rust resolver/analysis/bundling/file-size path and
service scheduling. The IPC schema, extension UI, cache database schema, cache lifecycle, and
compression format are not redesigned. Supporting churn also reaches dependency-update
automation, locked Cargo entry points, contributor policy, qualification tests, the tsdown
external list, and README wording at cutover; none of those changes alter extension-host
runtime behavior.

## 14. Risks and mitigations

### 14.1 Unsupported Rust API

Risk: a Rolldown update can break compilation without a major version change.

Mitigation: exact direct pins, one adapter boundary, Cargo-resolved compatibility, a locked
compiler-stack fingerprint, and mandatory full qualification for every update.

### 14.2 Transitive compiler-stack drift

Risk: Rolldown's published internal dependency ranges allow a general `cargo update` to move a
Rolldown workspace crate, OXC crate, or resolver without changing the top-level direct pin.
The `oxc_resolver` requirement in particular lives in the `rolldown_resolver` workspace crate,
not in `rolldown` itself, so nothing at the direct-dependency level anchors it.

Mitigation: exact direct requirements, committed lockfile, `--locked` for every non-update
Cargo entry point, generated graph fingerprint, `deps:update:compiler`, safe-update restoration,
and CI rejection of uncoordinated or duplicate resolved versions.

### 14.3 Async/Rayon contention

Risk: outer import parallelism and Rolldown internal parallelism can contend or deadlock if
nested incorrectly.

Mitigation: no outer global-Rayon build loop, daemon-wide async build semaphore, initial limit
of two, and explicit batch latency/peak-memory qualification.

### 14.4 Output movement

Risk: many cached sizes and `truly_treeshakeable` results will change because current values
contain both over-counts and under-counts.

Mitigation: construct-level expected behavior, real-package re-baselining, atomic analyzer
revision bump at cutover, and no cache schema change.

### 14.5 Metadata mismatch

Risk: rendered contributions and final minified bytes are different measurement stages.

Mitigation: keep contributions explicitly approximate, use rendered lengths consistently,
and never require their sum to equal the final chunk.

### 14.6 Product-specific policy leaking into bundling

Risk: cache, UI, or diagnostics requirements could grow into a second linker over time.

Mitigation: the adapter may select entries, set public options, record paths, enforce limits,
and translate outputs. It may not analyze binding reachability, rewrite real modules, classify
package side effects, match `sideEffects` globs, or override Rolldown's resolution/retention
results. Every adapter responsibility must cite a product requirement not handled by a public
Rolldown/OXC API.

## 15. Definition of done

The replacement is complete only when:

- the qualification result is recorded and every §10 gate passes;
- the SRS reflects the accepted architecture;
- Rolldown, direct OXC crates, and `oxc_resolver` are exactly pinned, the resolved compiler
  stack matches its generated fingerprint, and Rolldown types remain isolated behind Import
  Lens-owned types;
- compiler-stack updater, dry-run, incompatibility, transitive-drift, safe-update, and locked
  Cargo command tests pass;
- the renamed compiler-stack upgrade skill describes the shipped workflow, and no stale
  `deps:update:oxc` or OXC-only-configuration reference remains;
- all size-producing paths use the same engine;
- the committed construct matrix covers every category in §10.2;
- known dangling, effectful-initializer, ambiguous-star, and external-re-export cases pass;
- all successful output parses and passes semantic validation;
- graph limits, side effects, externals, CJS, cycles, and combined file sizing pass;
- absolute latency, memory, startup, concurrency, determinism, and six-target compilation gates
  pass;
- `ANALYZER_REVISION` is bumped atomically with cutover;
- the old manual semantic bundler, silent binding fabrication, custom reachability, namespace
  construction, CJS linker, and marker-removal pass are removed together with their dedicated
  test files, and no test outside the §10.2 construct matrix asserts linking or tree-shaking
  semantics. **The package-side-effect matcher is retained by the 2026-07-12 amendment above
  (I9)** — reporting-only, retention-neutral, static-fallback path only;
- direct `oxc_ast`, `oxc_ast_visit`, and `oxc_transformer` dependencies and the stale
  `oxc-parser` tsdown externalization are removed;
- neither `rolldown` nor `oxc-parser` is a direct TypeScript runtime dependency, and tsdown's
  transitive Rolldown remains independent from the Rust compiler stack;
- cache lifecycle and IPC compatibility tests remain green;
- the README truthfully describes Rolldown/OXC ownership and no longer claims custom
  reachability or an OXC-only bundler;
- all six packaged targets and daemon hashes are regenerated successfully.

Until these conditions hold, the current production engine remains in place and this design
remains proposed.

# Asset-counting design and implementation audit

- **Reviewed:** 2026-07-18
- **Branch:** `bundler-b2-asset-counting` at `7729faa`
- **Baseline:** `main` at merge-base `b29c329` (`git diff main...HEAD`, 16 commits)
- **Scope:** the design, plan, implementation, cache behavior, UI/protocol flow, tests, and already-accepted
  limits for counting CSS, wasm, and font assets.

## Executive summary

The central architecture is sound: classify assets at the bundler boundary, process them after the JS build,
compress each shipped artifact separately, add the results to Import Cost and File Cost, and expose a typed
breakdown. The implementation also handles CSS cycles, bounded `@import` traversal, partial stylesheet
fallback, per-runtime artifact boundaries, and wire compatibility thoughtfully.

The branch is not release-ready under this repository's release bar. Three defect groups can produce a wrong
number or wedge the daemon:

1. Local assets referenced from CSS `url()`—including the supported font types—are silently omitted while the
   result remains High confidence.
2. Asset cache fingerprints do not always describe the bytes that were measured; failed CSS children are not
   tracked at all, and the File Cost cache has a related 30-second stale window.
3. Stubbed assets bypass the graph's aggregate byte limit and are processed outside the engine's admission and
   timeout boundary, permitting tens of gigabytes of I/O/compression from one package graph.

These should be fixed before merging the feature. The missing integration tests and stale SRS made all three
easier to miss.

## What is already good

- `AssetKind` and `AssetContribution` form a clear daemon-to-extension contract.
- JS, combined CSS, wasm, and font artifacts are compressed independently and then summed, matching ADR-0005.
- The single-import and per-runtime File Cost paths both add counted asset sizes.
- CSS unioning deduplicates shared `@import` content; degraded per-sheet processing is disclosed as reading high.
- A broken sheet no longer discards every healthy sheet in the same set.
- Canonicalized CSS resolution stops `../` import cycles from killing the daemon.
- CSS traversal has file and byte limits, and all-failure/partial-failure diagnostics avoid double disclosure.
- Lightning CSS is exact-pinned and guarded as a standalone size-determining dependency.
- The extension labels the selected compression on included-asset rows and omits empty breakdowns.

## Release-blocking findings

### AC-01: CSS-referenced local assets are silently missing

- **Severity:** Blocker — common wrong-number path
- **Affected:** Import Cost, Combined Import Cost, File Cost, cache freshness, confidence, asset breakdown

Assets are discovered only when Rolldown invokes the JS plugin's `load` hook
(`daemon/src/engine/plugin.rs:426-453`). Lightning CSS follows `@import`, but both print passes use default
`PrinterOptions` and never consume `url()` dependencies (`daemon/src/pipeline/assets.rs:282-335`). A font used
normally from CSS therefore never reaches the font classifier:

```css
@font-face {
  font-family: Probe;
  src: url("./probe.woff2") format("woff2");
}
```

A focused end-to-end probe used an 8 KiB local `probe.woff2`. The result counted a CSS contribution, returned
no Font contribution or diagnostic, and still reported High confidence. The same discovery hole reaches local
images and other emitted files referenced by `url()`, although those types are outside the stated CSS/wasm/font
feature scope and need an explicit future-scope decision. Remote `@import` URLs are deliberately preserved but
are also not disclosed as external network weight.

**Recommended change**

1. Run Lightning CSS dependency analysis as a separate analysis pass so its placeholder URLs do not become the
   bytes being measured. The pinned crate reports both `@import` and `url()` dependencies.
2. Resolve local URL dependencies relative to the stylesheet that contains them, then canonicalize and dedupe
   them with plugin-discovered assets.
3. Count supported local fonts and wasm under the same per-artifact rules. Record images and other types as an
   explicit exclusion or future extension in the SRS; count or disclose them only if the settled scope requires
   it.
4. Treat data URLs, fragments, remote URLs, and missing files as distinct cases. Remote/unresolved resources
   should produce a precision diagnostic unless the product scope explicitly excludes them in the SRS.
5. Feed every resolved dependency into both exact-byte freshness and the aggregate resource budget.

**Acceptance tests**

- JS -> CSS -> local WOFF2 produces CSS and Font rows and raises the headline by the font artifact's size.
- Two sheets referencing the same font count it once in File Cost.
- A missing local URL is disclosed and cannot remain High confidence.
- A remote URL follows the documented scope and diagnostic policy.
- The independent oracle emits and counts the same local asset once.

### AC-02: freshness can bind a size to fingerprints from different bytes

- **Severity:** Blocker — durable wrong/stale results
- **Affected:** import memory/disk cache, failed CSS recovery, first-party packages, File Cost/status/budgets

There are three related failures in the exact-byte freshness contract:

1. The plugin reads and hashes each top-level asset. Its collected asset record retains only path, kind, and
   length while the read-time fingerprint is stored separately (`daemon/src/engine/plugin.rs:421-447`). CSS,
   wasm, and fonts are read again for the measurement (`daemon/src/pipeline/assets.rs:130-147,627-646`). The
   separately retained earlier fingerprint can therefore describe different bytes from the later measured
   artifact.
2. A successful CSS `@import` child contributes only a path. It is hashed later in
   `daemon/src/service.rs:3118-3124`, after Lightning CSS already measured it. If the child changes between
   those operations, the cache can store an old size beside the new fingerprint and consider it Fresh
   indefinitely.
3. `bundle_css` and `bundle_css_set` return through `?` before recovering the provider's paths on an error
   (`daemon/src/pipeline/assets.rs:211-237`). A broken imported child is consequently absent from freshness.
   `uncounted_assets` is allowed into durable stores (`daemon/src/pipeline/stage.rs:131-138`), so fixing only
   that child need not invalidate the cached fallback.

There is a bounded aggregate variant too. Asset child paths enter the import fingerprint source, but not the
dependency-path index used by the File Cost signature (`daemon/src/pipeline/analyze.rs:353,526-553`;
`daemon/src/pipeline/file_size_cache.rs:169-258`). File Cost can therefore remain stale for its 30-second L1
TTL even after import analysis has rebuilt the child correctly.

This is distinct from known issue D11. D11 concerns a transient read failure being cached; this finding shows
the cache identity itself can describe bytes other than the bytes used for the answer.

**Recommended change**

- Make an asset input carry the exact bytes and `FileFingerprint` captured by the read that supplied those
  bytes. Reuse the bytes for binary processing instead of reading the file again.
- Have `TrackingProvider::read` capture exact read-time fingerprints, not paths. Return the fingerprints in both
  success and failure outcomes; a CSS processing error needs a structured error containing the inputs read
  before it failed. Also retain attempted paths for missing/unreadable children, which have no bytes to hash;
  either fingerprint their absent state or keep that machine-dependent outcome out of durable stores.
- Merge those fingerprints directly into `FingerprintSource`; remove the post-build path hashing and the filter
  that prefers the plugin's earlier CSS-entry fingerprint.
- Include asset dependency identity in the File Cost signature, or include a stable generation/value from the
  already-revalidated import results.

**Acceptance tests**

- Fixing only a broken `@import` child invalidates a cached `uncounted_assets` result.
- A deterministic test hook mutates a child between read and cache insertion; the stored fingerprint must match
  the bytes whose size was reported.
- The same test exists for a direct binary asset.
- Editing a child and immediately requesting File Cost returns the new value without waiting for the 30-second
  TTL.

### AC-03: assets bypass aggregate limits and post-processing escapes bounded admission

- **Severity:** Blocker — daemon wedge/resource-exhaustion path
- **Affected:** CPU, memory, disk I/O, engine scheduling, shutdown responsiveness

The plugin enforces 20 MiB per module before reading it (`daemon/src/engine/plugin.rs:396-423`), but a
classified asset is returned as an empty module (`:439-453`). Aggregate graph accounting later charges
`module_info.code.len()` (`:487-503`), which is zero for every stubbed asset. The 100 MiB graph-source budget
therefore does not cover asset bytes.

The 2,000-module limit still applies, so the theoretical admitted shape is roughly 2,000 x 20 MiB: about
40 GiB of asset reads. Wasm and fonts are then read again and compressed (`daemon/src/pipeline/assets.rs:627-646`).
This work runs after `bundle_sync` has left the two-permit/eight-second engine boundary
(`daemon/src/pipeline/analyze.rs:377-407`; `daemon/src/engine/boundary.rs:127-145,222-224`). Four miss-drain
workers can consequently perform heavy asset tails concurrently, while Lightning CSS and compression also use
Rayon. If a CSS union exceeds its budget, the per-sheet retry resets that budget for each sheet, so the total
post-build work is not bounded by the 8 MiB union limit either.

**Recommended change**

- Add a per-build asset file/byte budget and charge it from metadata before reading. Either share the graph's
  aggregate byte budget or define a separate limit with an explicit total-resource rationale.
- Keep the loaded bytes in the bounded asset input so binary files are not read twice.
- Put CSS processing and asset compression behind bounded admission and a deadline. Measure before deciding
  whether this shares engine permits or uses a separate small semaphore.
- On a deterministic aggregate-budget breach, return one typed durable limit result, consistent with the graph
  limit. Treat a transient deadline breach as non-durable according to ADR-0006. In either case, do not continue
  per-sheet retries beyond the overall build budget.

**Acceptance tests and gates**

- With a test-sized aggregate limit, several direct font/wasm/CSS imports exceed it before their bytes are read
  and produce the documented typed result.
- A high-count per-sheet fallback cannot reset the overall budget.
- A concurrency test proves the post-build asset stage never exceeds its admission width.
- Add CSS-heavy and binary-heavy p95/RSS measurements to the existing release performance harness.

## Other required improvements

### AC-04: the binary shipping model is asserted, not qualified

**Priority:** High design/test gap

The design chooses emitted-file bytes rather than base64 inlining, but `ModuleType::Empty` removes the runtime
URL/reference code a real file-loader build emits (`daemon/src/engine/plugin.rs:434-451`). No end-to-end wasm or
font fixture proves that the selected neutral model agrees with the oracle; the only binary test manually
constructs `CollectedAsset` inside `assets.rs`.

Qualify direct default/value imports, side-effect imports, wasm initialization patterns, and URL imports against
the independent oracle. Then either model the small emitted-reference artifact, document a bounded difference,
or disclose the unsupported import form. Do not describe the JS chunk as exact until this is measured.

### AC-05: the planned integration gates are incomplete

**Priority:** High

`docs/asset-counting-plan.md:105-114` requires binary and broken-CSS end-to-end tests, real child-cache
invalidation, and File Cost asset aggregation. The branch currently has one CSS happy-path end-to-end test;
freshness, fallback, and binary coverage stop at processor units.

Add the following independent tests:

- the classifier's full extension matrix and passthrough behavior;
- CSS `url()` dependency discovery;
- wasm/font discovery through the real plugin, headline, breakdown, wire, and cache;
- broken CSS through the full pipeline;
- successful and failed `@import` child cache invalidation;
- File Cost CSS union/deduplication and immediate invalidation;
- aggregate asset limits and processing admission;
- an oracle fixture containing both `@import` and a local emitted asset. The current real CSS oracle fixture has
  neither, so it cannot guard those paths.

### AC-06: the authoritative documentation contradicts the shipped behavior

**Priority:** High contract/maintenance defect

- `docs/ImportLens-SRS.md:431-435,800,1613` still requires assets to be disclosed and never counted.
- The SRS wire model at `docs/ImportLens-SRS.md:1062-1080` omits `asset_breakdown`.
- `docs/asset-counting-design.md:3-5` still says "not yet implemented" and "RELEASE BLOCKER."
- The README does not explain that the headline can include CSS/wasm/font artifacts or how to read the
  breakdown and degraded cases.
- The design required Lightning CSS to join the Rolldown dependency-graph fingerprint and restore set, while
  the implementation deliberately treats it as an independently chosen exact pin. The implementation's
  standalone pin plus drift test is coherent; the design/plan should be updated to record that refined
  decision instead of forcing an unrelated crate into the Rolldown graph closure.

Update the SRS in the same fix that settles AC-01 through AC-04, then mark the design implemented/superseded and
add the user-facing README explanation. The SRS should precisely define which asset edges are counted, which are
disclosed, which artifact-loader model is used, and how confidence changes.

### AC-07: the asset module is too broad for the repository's conventions

**Priority:** Medium maintainability/performance risk

`daemon/src/pipeline/assets.rs` is 1,209 lines and owns limits, provider state, path resolution, CSS bundling,
domain/result types, diagnostics, binary I/O/compression, and all tests. That conflicts with the repository rule
to keep types, constants, helpers, and unrelated responsibilities in their proper modules, and it makes the
freshness and resource contracts difficult to see as one invariant.

After behavior is pinned by tests, split it into a small orchestration module plus focused CSS provider/bundler,
binary processor, diagnostics, and shared types/limits modules. Also:

- share the repeated uncounted-asset name/byte summarizer with `daemon/src/engine/adapter.rs:304-330`;
- remove or use the currently unused `AssetKind::ALL`, `AssetKind::as_str`, and `ProcessedAssets::is_empty`;
- keep the exact-byte fingerprint and overall resource budget in the common orchestration boundary so CSS and
  binary paths cannot drift again.

### AC-08: shared/external asset visibility needs an explicit product decision

**Priority:** Medium scope/UX

`shared_bytes` and module-sharing insights remain JS-module-only, even though File Cost now deduplicates CSS and
assets. A user can therefore see a gap between Combined Import Cost and File Cost without an asset-sharing
explanation. Remote CSS imports are similarly preserved without saying that their downloaded bytes are outside
the number.

Decide whether v1 should add internal per-asset path contributions for shared-cost explanations. If that is out
of scope, document that `shared_bytes` means JS module bytes only and add a generic asset-deduplication note when
the file-level arithmetic differs. Remote resources should always have an explicit scope/disclosure policy.

## Already recorded limits

The audit did not refile these as new defects:

| Existing item | Current decision |
| --- | --- |
| D7: CSS declared droppable can still be counted | Deferred; rare wrong-number package shape |
| D8: parser/bare-import fallback and cyclic CSS undercount | Accepted fallback limit |
| D9: 256-file/8 MiB CSS tree and per-sheet overcount | Accepted bounded degradation |
| D10: Brotli quality 4 reads high | Deferred product-wide compression trade-off |
| D11: transient asset read failure can be cached | Accepted disclosed fallback |
| D12: File Cost omits an unreadable asset without becoming a floor | Accepted pre-B2 parity |

AC-01 through AC-03 are not restatements of those decisions: they are silent omission, freshness-identity, and
resource-admission defects introduced or exposed by the new asset pipeline.

## Recommended implementation order

1. Add red end-to-end/cache/resource tests for AC-01 through AC-03, plus the File Cost stale case.
2. Introduce one bounded asset-input/result contract carrying exact bytes, exact read-time fingerprints,
   contributions, diagnostics, and an overall budget.
3. Add CSS dependency analysis for local `url()` resources and the explicit external-resource policy.
4. Move CSS/binary processing behind bounded admission; reuse bytes instead of rereading them.
5. Feed exact fingerprints through import and File Cost caches, including failure outcomes.
6. Qualify the direct binary shipping model and expand the independent oracle/performance gates.
7. Split the asset module, update the SRS/design/README, then run the full verification and Windows package gate.

## Verification performed for this audit

- `git diff --check main...HEAD` — passed.
- `pnpm check` — passed.
- Asset processor tests — 13 passed.
- Asset-kind/compiler-stack contract tests — 12 passed.
- A temporary local-font end-to-end probe reproduced AC-01 and was removed.
- A suspected overcount through an unselected export was rejected: an independent esbuild run emitted the same
  side-effectful CSS, so only the already-recorded D7 shape remains.

The full test suite and Windows packaging gate were not rerun because this audit changed documentation only.
They remain required after implementation fixes.

## Two-axis review summary

### Standards

1. The SRS is stale despite being the repository's source of truth.
2. `assets.rs` mixes several responsibilities in one 1,209-line file.
3. File Cost's new asset path has no behavior-level regression test.
4. Uncounted-asset summarization is duplicated between the engine adapter and asset pipeline.
5. Three asset helpers/constants are unused speculative surface.

### Spec

1. Asset freshness does not carry read-time fingerprints from the bytes actually measured, and failure paths
   discard dependencies.
2. CSS `url()` dependencies omit supported fonts from the headline and breakdown.
3. The emitted-file model for direct binary imports is not qualified end to end.
4. The compiler-stack fingerprint requirement was intentionally refined into a standalone exact pin, but the
   design was not updated to record the decision.
5. The planned binary/fallback/freshness/File Cost integration gates are missing.

**Axis totals:** Standards: 5 findings (worst: authoritative SRS drift). Spec: 5 findings (worst: exact-byte
freshness and silently omitted CSS-referenced fonts).

# Module-by-module release audit

A reusable harness for a systematic, whole-codebase release review. Each module is read deeply by a
dedicated *finder* subagent that writes a `NN-slug.findings.md`. A separate *fresh* *verifier* subagent
then takes that doc and the code, denies every claim by default, hunts for code evidence, and writes a
`NN-slug.verdict.md` recording each claim as **ACCEPTED** (with evidence) or **REJECTED** (with reason).
No fixes are made until every module has both docs.

Confirmed findings are migrated into `docs/known-issues.md` (the durable tracker), then the per-module
`.findings.md` / `.verdict.md` working docs are deleted. This README stays as the template for the next run.

**Release bar** (what blocks a release): a finding blocks only if it (a) shows the user a **WRONG NUMBER**
or (b) can **WEDGE** the system or **LOSE DATA**. Everything else is a known-limit and goes to
`docs/known-issues.md`.

## Method

- Run in batches (guideline: 3 tasks per batch); review the reports between batches, do not run all at once.
- A batch is 3 tasks. That can be 3 whole modules, or one oversized module split into ~3 focused tasks
  (e.g. a single very large file reviewed by concern).
- Each module: finder deep-reads the listed files and writes its findings doc; a fresh adversarial verifier
  re-traces every claim against the code and writes the verdict doc.
- The lead independently re-verifies any **accepted-blocking** finding against the code before it counts.

## Modules & status

Status legend: ⏳ pending · 🔎 in progress · ✅ done.

| # | Module | Files | findings | verdict |
|---|--------|-------|----------|---------|
| D3 | Pipeline · size & compression | file_size, file_size_cache, build_memo, minify, compress, full_package | ⏳ | ⏳ |
| D2 | Pipeline · resolve & side-effects | resolver, analyze, stage, types_only, node_builtins, export_list, fallback, util | ⏳ | ⏳ |
| D4 | Cache · memory & identity | memory, key, budget, recency | ⏳ | ⏳ |
| D5 | Cache · disk & project | disk, project | ⏳ | ⏳ |
| D9 | Report aggregate | model, scanner, executor | ⏳ | ⏳ |
| D6 | IPC & protocol (daemon) | server, protocol, codec | ⏳ | ⏳ |
| D7 | Service orchestration | service, prefetch, analysis_flight, lifecycle, main, logging (split into ~3 tasks: dispatch/panic-isolation · streaming/shutdown · prefetch/prewarm) | ⏳ | ⏳ |
| D8 | Document analysis | imports, script_regions, package_json, ignore, completion, positions, specifier | ⏳ | ⏳ |
| D1 | Engine boundary & build | plugin, adapter, mod, scheduling, entry, dependency_paths, limits, boundary | ⏳ | ⏳ |
| D10 | Registry | service, cache, client, constants, types, executor | ⏳ | ⏳ |
| E3 | Analysis state (ext) | history, documentStates, insights, budgets, fileSize, refreshMerge, fileCostQuality, transience, … | ⏳ | ⏳ |
| E4 | UI · cost surfaces | format, currentFileSize, report(Content), tooltip(Markdown), statusbar(Text), inlayHints, budgetDiagnostics, … | ⏳ | ⏳ |
| E2 | IPC client (ext) | client, protocol, codec, requestIds | ⏳ | ⏳ |
| E1 | Daemon lifecycle & transport (ext) | nativeTransport, transport, manager, processLifecycle, recycleGuard, restartPolicy, platform | ⏳ | ⏳ |
| E7 | Host wiring (ext) | extension, listener, watcher(Invalidation), config*, workspaceContext, logging | ⏳ | ⏳ |
| E5 | UI · package.json & actions | packageJson*, cacheManager*, bundleImpactHistoryView | ⏳ | ⏳ |
| E6 | Guidance & prewarm (ext) | packageJsonAnalysis, registryRefresh, packageJsonPartial, prewarm/* | ⏳ | ⏳ |

## Recommended order (highest release-bar risk first, following the data flow)

**D3 → D2 → D4 → D5 → D9 → D6 → D7 → D8 → D1 → D10 → E3 → E4 → E2 → E1 → E7 → E5 → E6**

Rationale: the daemon *produces* the numbers (D-modules), the extension *displays* them (E-modules). Start
where a wrong size originates (size/compression, resolve/side-effects, cache/durability), then the wire and
orchestration, then the extension surfaces where a correct number can be shown wrong or go stale after an edit.

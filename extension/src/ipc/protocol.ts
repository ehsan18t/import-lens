export const protocolVersion = 7;

export type ImportKind = "named" | "default" | "namespace" | "dynamic";

export type ImportRuntime = "component" | "client" | "server";

export type ImportSyntax = "static" | "reexport" | "star_reexport" | "dynamic";

export type LogLevel = "error" | "warn" | "info" | "debug";

export type ConfidenceLevel = "high" | "medium" | "low";

export interface SourcePosition {
  line: number;
  character: number;
}

export interface SourceRange {
  start: SourcePosition;
  end: SourcePosition;
}

export interface DetectedImport {
  specifier: string;
  packageName: string;
  named: string[];
  importKind: ImportKind;
  syntax: ImportSyntax;
  runtime: ImportRuntime;
  line: number;
  quoteEnd: SourcePosition;
  specifierRange: SourceRange;
  statementRange: SourceRange;
}

export interface ImportRequest {
  specifier: string;
  package: string;
  version: string;
  named: string[];
  import_kind: ImportKind;
  runtime: ImportRuntime;
}

/**
 * One import's analysis, in exactly one of the two states a response can carry (ADR-0006).
 * The third — Loading — is not an `ImportResult` at all: it is an `ImportAnalysisItem` with
 * `status: "loading"` and no result.
 *
 * - **Measured**: every size is a `number`, `unmeasured_stage` is absent, `error` is `null`.
 * - **Unmeasured**: every size is `null`, `unmeasured_stage` names the stage that could not
 *   answer, and `error` carries its message.
 *
 * A size is `number | null` and NOT optional, because the question a consumer must ask is
 * **"is there a size?"** — never "is there an error?". A degraded result used to carry
 * `error: null` PLUS a fabricated size, so every `!result.error` check in this codebase waved it
 * through; there is no size to misuse now. Use `measuredSizes()` in `ui/format.ts` to ask.
 */
export interface ImportResult {
  specifier: string;
  raw_bytes: number | null;
  minified_bytes: number | null;
  gzip_bytes: number | null;
  brotli_bytes: number | null;
  zstd_bytes: number | null;
  cache_hit: boolean;
  side_effects: boolean;
  truly_treeshakeable: boolean;
  is_cjs: boolean;
  confidence: ConfidenceLevel;
  confidence_reasons: string[];
  error: string | null;
  // The stage that could not answer, when there is no size. `null` on a measurement. It is what
  // tells a broken package (`parse` — a permanent fact) from a flaky daemon (`timeout` — a fact
  // about nothing at all).
  unmeasured_stage?: string | null;
  diagnostics: ImportDiagnostic[];
  module_breakdown?: ModuleContribution[];
  shared_bytes?: number;
  // Data-layer freshness of this served value. Omitted by the daemon when Fresh
  // (skip_serializing_if), so an absent field means Fresh. No consumer reads it yet.
  freshness?: ResultFreshness;
}

export type FreshnessKind = "fresh" | "stale" | "unverified";

export interface ResultFreshness {
  kind: FreshnessKind;
  // Meaningful when kind === "stale": a background recompute is in flight.
  revalidating: boolean;
  // Meaningful when kind === "unverified": why verification could not complete.
  reason?: string;
}

export interface ImportDiagnostic {
  stage: string;
  message: string;
  details: string[];
}

export interface ModuleContribution {
  path: string;
  bytes: number;
}

export type ImportAnalysisStatus = "loading" | "ready" | "missing" | "unavailable";

export interface ImportAnalysisItem {
  detected: DetectedImport;
  status: ImportAnalysisStatus;
  message?: string;
  request?: ImportRequest;
  result?: ImportResult;
}

export interface AnalyzeDocumentRequest {
  type: "analyze_document";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  source: string;
}

export interface AnalyzeDocumentResponse {
  version: number;
  request_id: number;
  imports: ImportAnalysisItem[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface AnalyzeSpecifiersRequest {
  type: "analyze_specifiers";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  specifiers: string[];
}

export interface AnalyzeSpecifiersResponse {
  version: number;
  request_id: number;
  imports: ImportAnalysisItem[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface FileSizeDocumentRequest {
  type: "file_size_document";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  source: string;
  // When true, the daemon bypasses stale-while-revalidate and recomputes
  // synchronously (CI / budget checks). Omitted by interactive clients, which get SWR.
  force_fresh?: boolean;
  // The analysis generation (the triggering document analysis's request id) this
  // size read belongs to. The daemon echoes it back on the resulting SWR
  // `refreshed_results` push so the client can drop a push that a newer analysis
  // has since superseded. Optional / additive for back-compat.
  analysis_generation?: number;
}

export interface FileSizeDocumentResponse {
  version: number;
  request_id: number;
  raw_bytes: number;
  minified_bytes: number;
  gzip_bytes: number;
  brotli_bytes: number;
  zstd_bytes: number;
  imports: ImportResult[];
  states: ImportAnalysisItem[];
  // The totals are a FLOOR, not the file's size: an import that belongs in them was never
  // measured — its own build had not landed (`status: "loading"`), or a transient engine failure
  // fabricated the size it carries. Safe to show with the diagnostics that say so (FR-024a),
  // never safe to record as a historical data point (FR-026c). Absent from an older daemon, which
  // is why it is optional; read it as `incomplete === true`.
  incomplete?: boolean;
  // The file's OWN combined build failed, so these totals are an un-deduplicated sum of the
  // per-import costs — a *different quantity* from a File Cost (ADR-0004), and an OVER-count.
  //
  // `incomplete` structurally cannot see this: a combined build is the biggest build in the system,
  // so it is the likeliest to hit the daemon's build timeout — and when it does, every one of the
  // file's imports may still be perfectly Measured. The response then carries `incomplete: false`,
  // `error: null`, a size on every import, and a number the file never had. Show it, never store it,
  // never judge a budget against it (ADR-0006, invariants 4 and 5).
  degraded?: boolean;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

// A stable per-import identity for the SWR refresh push: the specifier alone is
// NOT unique (two imports of the same package differ by kind / named exports but
// share a specifier), so pushes carry this alongside each result to disambiguate.
//
// `runtime` is part of the identity because it is part of the import: an Astro document can import
// the same package, with the same kind and the same named exports, from its frontmatter (server)
// and from a client <script>, and those are two rows with two different sizes (ADR-0005). Without
// it the two variants collide on one key and the client collapses them into a single row.
export interface RefreshedImportIdentity {
  specifier: string;
  import_kind: ImportKind;
  named: string[];
  runtime: ImportRuntime;
}

// Unsolicited server->client push: freshly-recomputed sizes for a document after a
// background stale-while-revalidate. Dispatched by `type`, not `request_id`.
export interface RefreshedResultsResponse {
  type: "refreshed_results";
  version: number;
  workspace_root: string;
  document_path: string;
  results: ImportResult[];
  // Per-result import identity, index-aligned with `results`, so the client can
  // disambiguate same-specifier variants (e.g. `import React from "react"` vs
  // `import { useState } from "react"`). Omitted by an older daemon -> the client
  // falls back to specifier-only keying.
  identities?: RefreshedImportIdentity[];
  // The analysis generation this refresh was computed for (echoed from the
  // triggering file_size_document request). The client drops the push if a newer
  // analysis has since superseded it. Omitted by an older daemon.
  generation?: number;
}

export interface RegistryHint {
  latestVersion?: string;
  latestPublishedAt?: string;
  isLatest?: boolean;
  deprecated?: boolean;
  fetchedAt?: number;
}

export type RegistryHintMode = "off" | "cached" | "refresh_stale" | "force_refresh";

export interface RegistryHintTarget {
  name: string;
  installedVersion?: string;
}

export interface RegistryHintResult {
  target: RegistryHintTarget;
  hint?: RegistryHint | null;
  error?: string | null;
  origin?: "cache" | "network";
}

export interface RefreshRegistryHintsRequest {
  type: "refresh_registry_hints";
  version: number;
  request_id: number;
  targets: RegistryHintTarget[];
  mode: "refresh_stale" | "force_refresh";
  // Opaque per-manifest key (the document key) that scopes the daemon's bulk
  // supersession to one source, so refreshing a different package.json does not
  // cancel this manifest's in-flight block.
  source?: string;
}

export interface RefreshRegistryHintsResponse {
  version: number;
  request_id: number;
  results: RegistryHintResult[];
  indexes?: number[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export type PackageJsonDependencySectionName =
  | "dependencies"
  | "devDependencies"
  | "peerDependencies"
  | "optionalDependencies";

export interface PackageJsonDependencyEntry {
  name: string;
  version: string;
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  nameRange: SourceRange;
  valueRange: SourceRange;
}

export interface PackageJsonDependencySection {
  section: PackageJsonDependencySectionName;
  range: SourceRange;
  objectRange: SourceRange;
}

export interface PackageJsonDependencyAnalysisItem {
  entry: PackageJsonDependencyEntry;
  name: string;
  section: PackageJsonDependencySectionName;
  status: ImportAnalysisStatus;
  installedVersion?: string;
  registryHint?: RegistryHint | null;
  message?: string;
  result?: ImportResult;
}

export interface AnalyzePackageJsonRequest {
  type: "analyze_package_json";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  source: string;
  streaming?: boolean;
  include_registry_hints?: boolean;
  force_registry_refresh?: boolean;
  refresh_section?: PackageJsonDependencySectionName;
  registry_hint_mode?: RegistryHintMode;
}

export interface AnalyzePackageJsonResponse {
  version: number;
  request_id: number;
  sections: PackageJsonDependencySection[];
  states: PackageJsonDependencyAnalysisItem[];
  indexes?: number[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface HelloMessage {
  type: "hello";
  version: number;
  workspace_root: string;
  storage_path: string;
  enable_disk_cache: boolean;
  cache_max_size_mb: number;
  registry_cache_max_size_mb: number;
  log_level: LogLevel;
}

export interface CacheInvalidateMessage {
  type: "cache_invalidate";
  package: string;
}

export interface CacheInvalidateAllMessage {
  type: "cache_invalidate_all";
}

export interface PrewarmPackageJsonMessage {
  type: "prewarm_package_json";
  package_json_path: string;
  active_document_path: string;
}

/**
 * Something the daemon memoized is no longer true. Two kinds of path, because two kinds of file
 * feed its resolvers: a `node_modules/<pkg>/package.json` (an install or uninstall), and a
 * `tsconfig.json` / `jsconfig.json` — the workspace's alias table, which is the only thing that
 * tells a path alias apart from a package that is not installed. The daemon read that table once
 * and never again, so adding the `paths` entry that repairs an unrecognized alias did nothing for
 * the rest of the daemon's life.
 */
export interface NodeModulesChangedMessage {
  type: "node_modules_changed";
  package_json_paths: string[];
  tsconfig_paths: string[];
}

export interface EnumerateExportsRequest {
  type: "enumerate_exports";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  specifier: string;
  package: string;
  package_version: string;
}

export interface EnumerateExportsResponse {
  version: number;
  request_id: number;
  specifier: string;
  exports: string[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface FileSizeRequest {
  type: "file_size";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  imports: ImportRequest[];
}

export interface FileSizeResponse {
  version: number;
  request_id: number;
  raw_bytes: number;
  minified_bytes: number;
  gzip_bytes: number;
  brotli_bytes: number;
  zstd_bytes: number;
  imports: ImportResult[];
  // The same two flags, with the same meanings, as on `FileSizeDocumentResponse`. The daemon has
  // always computed them for this surface too; omitting them here made it the one response where a
  // floor and a measurement are indistinguishable.
  incomplete?: boolean;
  degraded?: boolean;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface CompleteImportMembersRequest {
  type: "complete_import_members";
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  source: string;
  cursor_offset: number;
}

export interface CompleteImportMembersResponse {
  version: number;
  request_id: number;
  specifier: string | null;
  exports: string[];
  imported_names: string[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface CacheShardInfo {
  shard_id: string;
  project_root: string;
  normalized_root: string;
  cache_path: string;
  size_bytes: number;
  last_used_millis: number | null;
  loaded: boolean;
  /**
   * Number of cache entries this shard holds, from the daemon's O(1) per-shard
   * summary. Optional so a response from an older daemon that predates the field
   * still decodes (read it as `entry_count ?? 0`).
   */
  entry_count?: number;
}

export interface CacheOperationResult {
  shard_id: string;
  project_root: string;
  cache_path: string;
  removed: boolean;
  error: string | null;
}

export interface CacheStatusRequest {
  type: "cache_status";
  version: number;
  request_id: number;
  workspace_root?: string;
}

export interface CacheStatusResponse {
  version: number;
  request_id: number;
  total_size_bytes: number;
  project_count: number;
  max_size_mb: number;
  current_project: CacheShardInfo | null;
  /**
   * Sum of every shard's logical (envelope) bytes — the budget-tracked total,
   * distinct from `total_size_bytes` (the physical on-disk footprint). Optional
   * for forward/back-compat (`total_bytes ?? 0`).
   */
  total_bytes?: number;
  /** Global disk-byte budget the daemon enforces (`cacheMaxSizeMB` in bytes). */
  budget_bytes?: number;
  /** Serialized size of the shared npm-registry metadata snapshot, in bytes. */
  registry_size_bytes?: number;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface CacheListRequest {
  type: "cache_list";
  version: number;
  request_id: number;
}

export interface CacheListResponse {
  version: number;
  request_id: number;
  shards: CacheShardInfo[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export type CacheRemoveScope = "current_project" | "selected" | "all" | "orphans" | "registry";

export interface CacheRemoveRequest {
  type: "cache_remove";
  version: number;
  request_id: number;
  scope: CacheRemoveScope;
  workspace_root?: string;
  shard_ids?: string[];
}

export interface CacheRemoveResponse {
  version: number;
  request_id: number;
  removed: CacheOperationResult[];
  failed: CacheOperationResult[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface ShutdownMessage {
  type: "shutdown";
}

export interface WorkspaceReportRequest {
  type: "workspace_report";
  version: number;
  request_id: number;
  workspace_root: string;
  budgets?: WorkspaceReportBudgets;
}

/**
 * The budgets the workspace report can judge: the **per-import** one, and only that.
 *
 * A per-file budget is judged against a File Cost — one bundle over all a file's imports (ADR-0004)
 * — and the report has no combined build behind a row, only a sum of per-import costs, which counts
 * a shared module twice. It used to warn off that sum, and disagreed with the editor and
 * `importlens check` about the same file under the same budget. The per-file budget is theirs; it
 * is not on this request, so nothing here can be judged against a number the report does not have.
 */
export interface WorkspaceReportBudgets {
  perImportBrotliBytes?: number;
}

export interface WorkspaceReportRow {
  packageName: string;
  specifier: string;
  sourceFile: string;
  line: number;
  runtime: string;
  // `null` when the import could not be measured. Not zero: an exported report that prints "0 B"
  // for a package the engine could not build is the sentinel this model exists to abolish.
  minifiedBytes: number | null;
  gzipBytes: number | null;
  brotliBytes: number | null;
  zstdBytes: number | null;
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

/**
 * Every import of one specifier across the workspace, and what they cost **together**: three files
 * importing `react` is three Reacts. It was `totalBrotliBytes`, and a "total" of three Reacts is a
 * number no project ships — under the honest label the panel finally says what it exists to say.
 */
export interface DuplicateImportGroup {
  specifier: string;
  count: number;
  combinedImportCostBrotliBytes: number;
  sourceFiles: string[];
}

/**
 * One module, and the imports that reach it — **two** numbers, because the module's size and what
 * its importing sites pay are two different quantities (ADR-0004).
 *
 * It carried one, `totalBytes`: the module's bytes added up once per importing row, rendered under
 * the header "Total Bytes". `react-dom/index.js` **is 100 kB** and is reached by three imports, and
 * the panel said **300 kB**.
 */
export interface DuplicateModuleGroup {
  modulePath: string;
  basename: string;
  /** The number of imports that reach this module. */
  count: number;
  /** The module's own rendered size — the largest contribution seen across the builds that reached it. */
  moduleBytes: number;
  /** The module counted once per importing site: a Combined Import Cost, an upper bound. */
  combinedImportCostBytes: number;
  specifiers: string[];
  vendored: boolean;
}

/**
 * The report's headline is a **Combined Import Cost**: the sum of independent Import Costs, each
 * priced as though the application were otherwise empty (ADR-0004).
 *
 * It counts a dependency at every site it is imported from — `react` in fifty files is fifty Reacts,
 * and one `import React, { useState } from "react"` is TWO imports, counted twice. Subtracting the
 * overlap would assert a project-level bundle quantity this product does not model, and compressed
 * sizes are not additive regardless, so the figure is an upper bound. It ranks imports and
 * apportions blame; it is never a size. It was `totalBrotliBytes`, rendered "Total Brotli" — the
 * arithmetic was right and the word was the defect. The treemap's percentages are shares of it.
 */
export interface WorkspaceReportSummary {
  importCount: number;
  combinedImportCostBrotliBytes: number;
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

export type ClientMessage =
  | HelloMessage
  | AnalyzeDocumentRequest
  | AnalyzePackageJsonRequest
  | RefreshRegistryHintsRequest
  | AnalyzeSpecifiersRequest
  | CacheInvalidateMessage
  | CacheInvalidateAllMessage
  | PrewarmPackageJsonMessage
  | NodeModulesChangedMessage
  | EnumerateExportsRequest
  | FileSizeRequest
  | FileSizeDocumentRequest
  | CompleteImportMembersRequest
  | CacheStatusRequest
  | CacheListRequest
  | CacheRemoveRequest
  | WorkspaceReportRequest
  | ShutdownMessage;

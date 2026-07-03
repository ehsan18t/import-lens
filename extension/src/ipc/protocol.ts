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

export interface ImportResult {
  specifier: string;
  raw_bytes: number;
  minified_bytes: number;
  gzip_bytes: number;
  brotli_bytes: number;
  zstd_bytes: number;
  cache_hit: boolean;
  side_effects: boolean;
  truly_treeshakeable: boolean;
  is_cjs: boolean;
  confidence: ConfidenceLevel;
  confidence_reasons: string[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
  module_breakdown?: ModuleContribution[];
  shared_bytes?: number;
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
  error: string | null;
  diagnostics: ImportDiagnostic[];
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
}

export interface RefreshRegistryHintsRequest {
  type: "refresh_registry_hints";
  version: number;
  request_id: number;
  targets: RegistryHintTarget[];
  mode: "refresh_stale" | "force_refresh";
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
  cache_max_age_days: number;
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

export interface NodeModulesChangedMessage {
  type: "node_modules_changed";
  package_json_paths: string[];
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
  max_age_days: number;
  last_cleanup_millis: number | null;
  current_project: CacheShardInfo | null;
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface CacheCleanupRequest {
  type: "cache_cleanup";
  version: number;
  request_id: number;
}

export interface CacheCleanupResponse {
  version: number;
  request_id: number;
  total_size_bytes: number;
  removed: CacheOperationResult[];
  failed: CacheOperationResult[];
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

export type CacheRemoveScope = "current_project" | "selected" | "all";

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

export interface WorkspaceReportBudgets {
  perImportBrotliBytes?: number;
  perFileBrotliBytes?: number;
}

export interface WorkspaceReportRow {
  packageName: string;
  specifier: string;
  sourceFile: string;
  line: number;
  runtime: string;
  minifiedBytes: number;
  gzipBytes: number;
  brotliBytes: number;
  zstdBytes: number;
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

export interface DuplicateImportGroup {
  specifier: string;
  count: number;
  totalBrotliBytes: number;
  sourceFiles: string[];
}

export interface DuplicateModuleGroup {
  modulePath: string;
  basename: string;
  count: number;
  totalBytes: number;
  specifiers: string[];
  vendored: boolean;
}

export interface WorkspaceReportSummary {
  importCount: number;
  totalBrotliBytes: number;
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
  | CacheCleanupRequest
  | CacheListRequest
  | CacheRemoveRequest
  | WorkspaceReportRequest
  | ShutdownMessage;

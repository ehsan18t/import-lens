export const protocolVersion = 5;

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

export interface BatchRequest {
  version: number;
  request_id: number;
  workspace_root: string;
  active_document_path: string;
  imports: ImportRequest[];
  streaming?: boolean;
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

export interface BatchResponse {
  version: number;
  request_id: number;
  imports: ImportResult[];
  indexes?: number[];
}

export type ImportAnalysisStatus = "ready" | "missing" | "unavailable";

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
  include_registry_hints?: boolean;
  force_registry_refresh?: boolean;
  refresh_section?: PackageJsonDependencySectionName;
}

export interface AnalyzePackageJsonResponse {
  version: number;
  request_id: number;
  sections: PackageJsonDependencySection[];
  states: PackageJsonDependencyAnalysisItem[];
  error: string | null;
  diagnostics: ImportDiagnostic[];
}

export interface HelloMessage {
  type: "hello";
  version: number;
  workspace_root: string;
  storage_path: string;
  enable_disk_cache: boolean;
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

export interface ShutdownMessage {
  type: "shutdown";
}

export type ClientMessage =
  | HelloMessage
  | AnalyzeDocumentRequest
  | AnalyzePackageJsonRequest
  | AnalyzeSpecifiersRequest
  | BatchRequest
  | CacheInvalidateMessage
  | CacheInvalidateAllMessage
  | PrewarmPackageJsonMessage
  | NodeModulesChangedMessage
  | EnumerateExportsRequest
  | FileSizeRequest
  | FileSizeDocumentRequest
  | CompleteImportMembersRequest
  | ShutdownMessage;

export const protocolVersion = 2;

export type ImportKind = "named" | "default" | "namespace" | "dynamic";

export type LogLevel = "error" | "warn" | "info" | "debug";

export interface ImportRequest {
  specifier: string;
  package: string;
  version: string;
  named: string[];
  import_kind: ImportKind;
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

export interface ShutdownMessage {
  type: "shutdown";
}

export type ClientMessage =
  | HelloMessage
  | BatchRequest
  | CacheInvalidateMessage
  | CacheInvalidateAllMessage
  | PrewarmPackageJsonMessage
  | EnumerateExportsRequest
  | ShutdownMessage;

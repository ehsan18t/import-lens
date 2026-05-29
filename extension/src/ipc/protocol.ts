export const protocolVersion = 1;

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
  active_document_path: string;
  imports: ImportRequest[];
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
}

export interface BatchResponse {
  version: number;
  request_id: number;
  imports: ImportResult[];
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

export interface ShutdownMessage {
  type: "shutdown";
}

export type ClientMessage =
  | HelloMessage
  | BatchRequest
  | CacheInvalidateMessage
  | CacheInvalidateAllMessage
  | ShutdownMessage;


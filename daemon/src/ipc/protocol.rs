use crate::document::{PackageJsonDependencyEntry, PackageJsonDependencySection};
use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 7;

pub fn is_supported_protocol_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Named,
    Default,
    Namespace,
    Dynamic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportRuntime {
    #[default]
    Component,
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    #[default]
    Low,
}

impl ImportRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSyntax {
    Static,
    Reexport,
    StarReexport,
    Dynamic,
}

impl ImportSyntax {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Reexport => "reexport",
            Self::StarReexport => "star_reexport",
            Self::Dynamic => "dynamic",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedImport {
    pub specifier: String,
    pub package_name: String,
    pub named: Vec<String>,
    pub import_kind: ImportKind,
    pub syntax: ImportSyntax,
    pub runtime: ImportRuntime,
    pub line: u32,
    pub quote_end: SourcePosition,
    pub specifier_range: SourceRange,
    pub statement_range: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRequest {
    pub specifier: String,
    #[serde(rename = "package")]
    pub package_name: String,
    pub version: String,
    pub named: Vec<String>,
    pub import_kind: ImportKind,
    #[serde(default)]
    pub runtime: ImportRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub imports: Vec<ImportRequest>,
    #[serde(default)]
    pub streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportResult {
    pub specifier: String,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub cache_hit: bool,
    pub side_effects: bool,
    pub truly_treeshakeable: bool,
    pub is_cjs: bool,
    #[serde(default)]
    pub confidence: ConfidenceLevel,
    #[serde(default)]
    pub confidence_reasons: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_breakdown: Option<Vec<ModuleContribution>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_bytes: Option<u64>,
    #[serde(default, skip)]
    pub internal_contributions: Vec<ModuleContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDiagnostic {
    pub stage: String,
    pub message: String,
    pub details: Vec<String>,
}

impl ImportDiagnostic {
    pub fn for_stage(stage: &str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.to_owned(),
            message: message.into(),
            details: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleContribution {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAnalysisStatus {
    Loading,
    Ready,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAnalysisItem {
    pub detected: DetectedImport,
    pub status: ImportAnalysisStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<ImportRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImportResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeDocumentRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_document_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeDocumentResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportAnalysisItem>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeSpecifiersRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_specifiers_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub specifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzeSpecifiersResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportAnalysisItem>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeDocumentRequest {
    #[serde(rename = "type")]
    #[serde(default = "file_size_document_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeDocumentResponse {
    pub version: u32,
    pub request_id: u64,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub imports: Vec<ImportResult>,
    pub states: Vec<ImportAnalysisItem>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_latest: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageJsonDependencyAnalysisItem {
    pub entry: PackageJsonDependencyEntry,
    pub name: String,
    pub section: String,
    pub status: ImportAnalysisStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_hint: Option<RegistryHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImportResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHintMode {
    Off,
    Cached,
    RefreshStale,
    ForceRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintTarget {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHintResult {
    pub target: RegistryHintTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<RegistryHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// "cache" or "network" — how this hint was resolved. Optional for
    /// backward compatibility with older daemons/extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsRequest {
    #[serde(rename = "type")]
    #[serde(default = "refresh_registry_hints_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub targets: Vec<RegistryHintTarget>,
    pub mode: RegistryHintMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRegistryHintsResponse {
    pub version: u32,
    pub request_id: u64,
    pub results: Vec<RegistryHintResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_import_brotli_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_file_brotli_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportRequest {
    #[serde(rename = "type")]
    #[serde(default = "workspace_report_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    #[serde(default)]
    pub budgets: WorkspaceReportBudgets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportRow {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub line: u32,
    pub runtime: String,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub shared_bytes: u64,
    pub confidence: String,
    pub confidence_reasons: String,
    pub top_modules: String,
    pub warning: String,
    pub module_contributions: Vec<ModuleContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportTreemapItem {
    pub package_name: String,
    pub specifier: String,
    pub source_file: String,
    pub brotli_bytes: u64,
    pub percentage: u64,
    pub confidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateImportGroup {
    pub specifier: String,
    pub count: u64,
    pub total_brotli_bytes: u64,
    pub source_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateModuleGroup {
    pub module_path: String,
    pub basename: String,
    pub count: u64,
    pub total_bytes: u64,
    pub specifiers: Vec<String>,
    pub vendored: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReportSummary {
    pub import_count: u64,
    pub total_brotli_bytes: u64,
    pub low_confidence_count: u64,
    pub medium_confidence_count: u64,
    pub conservative_count: u64,
    pub budget_violation_count: u64,
    pub duplicate_imports: Vec<DuplicateImportGroup>,
    pub shared_modules: Vec<DuplicateModuleGroup>,
    pub treemap: Vec<WorkspaceReportTreemapItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReportResponse {
    pub version: u32,
    pub request_id: u64,
    pub rows: Vec<WorkspaceReportRow>,
    pub summary: WorkspaceReportSummary,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzePackageJsonRequest {
    #[serde(rename = "type")]
    #[serde(default = "analyze_package_json_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub include_registry_hints: bool,
    #[serde(default)]
    pub force_registry_refresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_hint_mode: Option<RegistryHintMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzePackageJsonResponse {
    pub version: u32,
    pub request_id: u64,
    pub sections: Vec<PackageJsonDependencySection>,
    pub states: Vec<PackageJsonDependencyAnalysisItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<usize>>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteImportMembersRequest {
    #[serde(rename = "type")]
    #[serde(default = "complete_import_members_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub source: String,
    pub cursor_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteImportMembersResponse {
    pub version: u32,
    pub request_id: u64,
    pub specifier: Option<String>,
    pub exports: Vec<String>,
    pub imported_names: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    #[serde(default = "hello_message_type")]
    pub message_type: String,
    pub version: u32,
    pub workspace_root: String,
    pub storage_path: String,
    pub enable_disk_cache: bool,
    #[serde(default = "default_cache_max_size_mb")]
    pub cache_max_size_mb: u64,
    #[serde(default = "default_cache_max_age_days")]
    pub cache_max_age_days: u64,
    pub log_level: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInvalidateMessage {
    #[serde(rename = "type")]
    #[serde(default = "cache_invalidate_message_type")]
    pub message_type: String,
    #[serde(rename = "package")]
    pub package_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInvalidateAllMessage {
    #[serde(rename = "type")]
    #[serde(default = "cache_invalidate_all_message_type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrewarmPackageJsonMessage {
    #[serde(rename = "type")]
    #[serde(default = "prewarm_package_json_message_type")]
    pub message_type: String,
    pub package_json_path: String,
    pub active_document_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeModulesChangedMessage {
    #[serde(rename = "type")]
    #[serde(default = "node_modules_changed_message_type")]
    pub message_type: String,
    pub package_json_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumerateExportsRequest {
    #[serde(rename = "type")]
    #[serde(default = "enumerate_exports_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub specifier: String,
    #[serde(rename = "package")]
    pub package_name: String,
    pub package_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumerateExportsResponse {
    pub version: u32,
    pub request_id: u64,
    pub specifier: String,
    pub exports: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeRequest {
    #[serde(rename = "type")]
    #[serde(default = "file_size_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub workspace_root: String,
    pub active_document_path: String,
    pub imports: Vec<ImportRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSizeResponse {
    pub version: u32,
    pub request_id: u64,
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub imports: Vec<ImportResult>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheShardInfo {
    pub shard_id: String,
    pub project_root: String,
    pub normalized_root: String,
    pub cache_path: String,
    pub size_bytes: u64,
    pub last_used_millis: Option<u64>,
    pub loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheOperationResult {
    pub shard_id: String,
    pub project_root: String,
    pub cache_path: String,
    pub removed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatusRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_status_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatusResponse {
    pub version: u32,
    pub request_id: u64,
    pub total_size_bytes: u64,
    pub project_count: usize,
    pub max_size_mb: u64,
    pub max_age_days: u64,
    pub last_cleanup_millis: Option<u64>,
    pub current_project: Option<CacheShardInfo>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCleanupRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_cleanup_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCleanupResponse {
    pub version: u32,
    pub request_id: u64,
    pub total_size_bytes: u64,
    pub removed: Vec<CacheOperationResult>,
    pub failed: Vec<CacheOperationResult>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheListRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_list_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheListResponse {
    pub version: u32,
    pub request_id: u64,
    pub shards: Vec<CacheShardInfo>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRemoveScope {
    CurrentProject,
    Selected,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRemoveRequest {
    #[serde(rename = "type")]
    #[serde(default = "cache_remove_message_type")]
    pub message_type: String,
    pub version: u32,
    pub request_id: u64,
    pub scope: CacheRemoveScope,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub shard_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRemoveResponse {
    pub version: u32,
    pub request_id: u64,
    pub removed: Vec<CacheOperationResult>,
    pub failed: Vec<CacheOperationResult>,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownMessage {
    #[serde(rename = "type")]
    #[serde(default = "shutdown_message_type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClientMessage {
    Hello(HelloMessage),
    AnalyzeDocument(AnalyzeDocumentRequest),
    AnalyzePackageJson(AnalyzePackageJsonRequest),
    AnalyzeSpecifiers(AnalyzeSpecifiersRequest),
    Batch(BatchRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    NodeModulesChanged(NodeModulesChangedMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
    FileSizeDocument(FileSizeDocumentRequest),
    CompleteImportMembers(CompleteImportMembersRequest),
    CacheStatus(CacheStatusRequest),
    CacheCleanup(CacheCleanupRequest),
    CacheList(CacheListRequest),
    CacheRemove(CacheRemoveRequest),
    RefreshRegistryHints(RefreshRegistryHintsRequest),
    WorkspaceReport(WorkspaceReportRequest),
    Shutdown(ShutdownMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum ClientMessageWire {
    Typed(TypedClientMessage),
    Batch(BatchRequestWire),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TypedClientMessage {
    Hello(HelloMessage),
    AnalyzeDocument(AnalyzeDocumentRequest),
    AnalyzePackageJson(AnalyzePackageJsonRequest),
    AnalyzeSpecifiers(AnalyzeSpecifiersRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    NodeModulesChanged(NodeModulesChangedMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
    FileSizeDocument(FileSizeDocumentRequest),
    CompleteImportMembers(CompleteImportMembersRequest),
    CacheStatus(CacheStatusRequest),
    CacheCleanup(CacheCleanupRequest),
    CacheList(CacheListRequest),
    CacheRemove(CacheRemoveRequest),
    RefreshRegistryHints(RefreshRegistryHintsRequest),
    WorkspaceReport(WorkspaceReportRequest),
    Shutdown(ShutdownMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchRequestWire {
    version: u32,
    request_id: u64,
    workspace_root: String,
    active_document_path: String,
    imports: Vec<ImportRequest>,
    #[serde(default)]
    streaming: bool,
}

impl<'de> Deserialize<'de> for ClientMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ClientMessageWire::deserialize(deserializer).map(Into::into)
    }
}

impl From<ClientMessageWire> for ClientMessage {
    fn from(message: ClientMessageWire) -> Self {
        match message {
            ClientMessageWire::Typed(message) => message.into(),
            ClientMessageWire::Batch(request) => Self::Batch(request.into()),
        }
    }
}

impl From<TypedClientMessage> for ClientMessage {
    fn from(message: TypedClientMessage) -> Self {
        match message {
            TypedClientMessage::Hello(message) => Self::Hello(message),
            TypedClientMessage::AnalyzeDocument(message) => Self::AnalyzeDocument(message),
            TypedClientMessage::AnalyzePackageJson(message) => Self::AnalyzePackageJson(message),
            TypedClientMessage::AnalyzeSpecifiers(message) => Self::AnalyzeSpecifiers(message),
            TypedClientMessage::CacheInvalidate(message) => Self::CacheInvalidate(message),
            TypedClientMessage::CacheInvalidateAll(message) => Self::CacheInvalidateAll(message),
            TypedClientMessage::PrewarmPackageJson(message) => Self::PrewarmPackageJson(message),
            TypedClientMessage::NodeModulesChanged(message) => Self::NodeModulesChanged(message),
            TypedClientMessage::EnumerateExports(message) => Self::EnumerateExports(message),
            TypedClientMessage::FileSize(message) => Self::FileSize(message),
            TypedClientMessage::FileSizeDocument(message) => Self::FileSizeDocument(message),
            TypedClientMessage::CompleteImportMembers(message) => {
                Self::CompleteImportMembers(message)
            }
            TypedClientMessage::CacheStatus(message) => Self::CacheStatus(message),
            TypedClientMessage::CacheCleanup(message) => Self::CacheCleanup(message),
            TypedClientMessage::CacheList(message) => Self::CacheList(message),
            TypedClientMessage::CacheRemove(message) => Self::CacheRemove(message),
            TypedClientMessage::RefreshRegistryHints(message) => {
                Self::RefreshRegistryHints(message)
            }
            TypedClientMessage::WorkspaceReport(message) => Self::WorkspaceReport(message),
            TypedClientMessage::Shutdown(message) => Self::Shutdown(message),
        }
    }
}

impl From<BatchRequestWire> for BatchRequest {
    fn from(request: BatchRequestWire) -> Self {
        Self {
            version: request.version,
            request_id: request.request_id,
            workspace_root: request.workspace_root,
            active_document_path: request.active_document_path,
            imports: request.imports,
            streaming: request.streaming,
        }
    }
}

fn hello_message_type() -> String {
    "hello".to_owned()
}

fn default_cache_max_size_mb() -> u64 {
    512
}

fn default_cache_max_age_days() -> u64 {
    30
}

fn analyze_document_message_type() -> String {
    "analyze_document".to_owned()
}

fn analyze_package_json_message_type() -> String {
    "analyze_package_json".to_owned()
}

fn analyze_specifiers_message_type() -> String {
    "analyze_specifiers".to_owned()
}

fn cache_invalidate_message_type() -> String {
    "cache_invalidate".to_owned()
}

fn cache_invalidate_all_message_type() -> String {
    "cache_invalidate_all".to_owned()
}

fn prewarm_package_json_message_type() -> String {
    "prewarm_package_json".to_owned()
}

fn node_modules_changed_message_type() -> String {
    "node_modules_changed".to_owned()
}

fn enumerate_exports_message_type() -> String {
    "enumerate_exports".to_owned()
}

fn file_size_message_type() -> String {
    "file_size".to_owned()
}

fn file_size_document_message_type() -> String {
    "file_size_document".to_owned()
}

fn complete_import_members_message_type() -> String {
    "complete_import_members".to_owned()
}

fn cache_status_message_type() -> String {
    "cache_status".to_owned()
}

fn cache_cleanup_message_type() -> String {
    "cache_cleanup".to_owned()
}

fn cache_list_message_type() -> String {
    "cache_list".to_owned()
}

fn cache_remove_message_type() -> String {
    "cache_remove".to_owned()
}

fn shutdown_message_type() -> String {
    "shutdown".to_owned()
}

fn refresh_registry_hints_message_type() -> String {
    "refresh_registry_hints".to_owned()
}

fn workspace_report_message_type() -> String {
    "workspace_report".to_owned()
}

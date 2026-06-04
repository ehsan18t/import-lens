use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl ImportRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Client => "client",
            Self::Server => "server",
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    #[serde(default = "hello_message_type")]
    pub message_type: String,
    pub version: u32,
    pub workspace_root: String,
    pub storage_path: String,
    pub enable_disk_cache: bool,
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
pub struct ShutdownMessage {
    #[serde(rename = "type")]
    #[serde(default = "shutdown_message_type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClientMessage {
    Hello(HelloMessage),
    Batch(BatchRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
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
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    PrewarmPackageJson(PrewarmPackageJsonMessage),
    EnumerateExports(EnumerateExportsRequest),
    FileSize(FileSizeRequest),
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
            TypedClientMessage::CacheInvalidate(message) => Self::CacheInvalidate(message),
            TypedClientMessage::CacheInvalidateAll(message) => Self::CacheInvalidateAll(message),
            TypedClientMessage::PrewarmPackageJson(message) => Self::PrewarmPackageJson(message),
            TypedClientMessage::EnumerateExports(message) => Self::EnumerateExports(message),
            TypedClientMessage::FileSize(message) => Self::FileSize(message),
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

fn cache_invalidate_message_type() -> String {
    "cache_invalidate".to_owned()
}

fn cache_invalidate_all_message_type() -> String {
    "cache_invalidate_all".to_owned()
}

fn prewarm_package_json_message_type() -> String {
    "prewarm_package_json".to_owned()
}

fn enumerate_exports_message_type() -> String {
    "enumerate_exports".to_owned()
}

fn file_size_message_type() -> String {
    "file_size".to_owned()
}

fn shutdown_message_type() -> String {
    "shutdown".to_owned()
}

use serde::{Deserialize, Deserializer, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Named,
    Default,
    Namespace,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRequest {
    pub specifier: String,
    #[serde(rename = "package")]
    pub package_name: String,
    pub version: String,
    pub named: Vec<String>,
    pub import_kind: ImportKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub version: u32,
    pub request_id: u64,
    pub active_document_path: String,
    pub imports: Vec<ImportRequest>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDiagnostic {
    pub stage: String,
    pub message: String,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub version: u32,
    pub request_id: u64,
    pub imports: Vec<ImportResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
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
    pub message_type: String,
    #[serde(rename = "package")]
    pub package_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInvalidateAllMessage {
    #[serde(rename = "type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownMessage {
    #[serde(rename = "type")]
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ClientMessage {
    Hello(HelloMessage),
    Batch(BatchRequest),
    CacheInvalidate(CacheInvalidateMessage),
    CacheInvalidateAll(CacheInvalidateAllMessage),
    Shutdown(ShutdownMessage),
}

impl<'de> Deserialize<'de> for ClientMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("hello") => serde_json::from_value(value)
                .map(Self::Hello)
                .map_err(serde::de::Error::custom),
            Some("cache_invalidate") => serde_json::from_value(value)
                .map(Self::CacheInvalidate)
                .map_err(serde::de::Error::custom),
            Some("cache_invalidate_all") => serde_json::from_value(value)
                .map(Self::CacheInvalidateAll)
                .map_err(serde::de::Error::custom),
            Some("shutdown") => serde_json::from_value(value)
                .map(Self::Shutdown)
                .map_err(serde::de::Error::custom),
            Some(message_type) => Err(serde::de::Error::custom(format!(
                "unknown client message type: {message_type}"
            ))),
            None => serde_json::from_value(value)
                .map(Self::Batch)
                .map_err(serde::de::Error::custom),
        }
    }
}

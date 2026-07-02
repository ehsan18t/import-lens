use crate::ipc::protocol::RegistryHint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPackageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_published_at: Option<String>,
    #[serde(default)]
    pub deprecated_versions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPackageMetadataEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RegistryPackageMetadata>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub not_found: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRegistryResponse {
    pub status: u16,
    pub retry_after_ms: Option<u64>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHintLookup {
    pub hint: Option<RegistryHint>,
    pub error: Option<String>,
}

pub trait RegistryHttpClient: Send + Sync {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String>;
}

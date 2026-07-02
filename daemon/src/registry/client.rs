use super::{constants::DEFAULT_TIMEOUT_MS, types::HttpRegistryResponse};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UreqRegistryHttpClient {
    timeout_ms: u64,
}

impl Default for UreqRegistryHttpClient {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl UreqRegistryHttpClient {
    pub fn new(timeout_ms: u64) -> Self {
        Self { timeout_ms }
    }
}

impl super::types::RegistryHttpClient for UreqRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        let url = registry_url(package_name);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(self.timeout_ms)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .header(
                "accept",
                "application/vnd.npm.install-v1+json, application/json",
            )
            .call()
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let retry_after_ms = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(retry_after_delay_ms);
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?;

        Ok(HttpRegistryResponse {
            status,
            retry_after_ms,
            body,
        })
    }
}

fn registry_url(package_name: &str) -> String {
    if let Some(rest) = package_name.strip_prefix('@') {
        format!("https://registry.npmjs.org/@{}", rest.replace('/', "%2F"))
    } else {
        format!("https://registry.npmjs.org/{package_name}")
    }
}

fn retry_after_delay_ms(header: &str) -> Option<u64> {
    header
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds.max(0.0) * 1000.0).round() as u64)
}

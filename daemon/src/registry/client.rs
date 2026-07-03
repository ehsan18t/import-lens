use super::{constants::DEFAULT_TIMEOUT_MS, types::HttpRegistryResponse};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct UreqRegistryHttpClient {
    agent: ureq::Agent,
}

impl Default for UreqRegistryHttpClient {
    fn default() -> Self {
        Self::new(DEFAULT_TIMEOUT_MS)
    }
}

impl UreqRegistryHttpClient {
    pub fn new(timeout_ms: u64) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}

impl super::types::RegistryHttpClient for UreqRegistryHttpClient {
    fn get_package_metadata(&self, package_name: &str) -> Result<HttpRegistryResponse, String> {
        let url = registry_url(package_name);
        let mut response = self
            .agent
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
            .and_then(|value| retry_after_delay_ms(value, SystemTime::now()));
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

fn retry_after_delay_ms(header: &str, now: SystemTime) -> Option<u64> {
    if let Ok(seconds) = header.parse::<f64>() {
        return Some((seconds.max(0.0) * 1000.0).round() as u64);
    }

    // RFC 7231 allows Retry-After to carry an HTTP-date instead of
    // delta-seconds; proxies/CDNs in front of registries emit this form.
    // A past date clamps to zero (retry immediately), matching the old
    // extension-host parser this daemon client replaced.
    let retry_at = httpdate::parse_http_date(header).ok()?;
    Some(
        retry_at
            .duration_since(now)
            .map(|delay| delay.as_millis() as u64)
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::retry_after_delay_ms;
    use std::time::{Duration, SystemTime};

    #[test]
    fn retry_after_parses_delta_seconds() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("120", now), Some(120_000));
        assert_eq!(retry_after_delay_ms("1.5", now), Some(1_500));
    }

    #[test]
    fn retry_after_clamps_negative_delta_seconds_to_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("-5", now), Some(0));
    }

    #[test]
    fn retry_after_parses_future_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let header = httpdate::fmt_http_date(now + Duration::from_secs(30));
        assert_eq!(retry_after_delay_ms(&header, now), Some(30_000));
    }

    #[test]
    fn retry_after_clamps_past_http_date_to_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let header = httpdate::fmt_http_date(now - Duration::from_secs(30));
        assert_eq!(retry_after_delay_ms(&header, now), Some(0));
    }

    #[test]
    fn retry_after_rejects_unparseable_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay_ms("soon", now), None);
        assert_eq!(retry_after_delay_ms("", now), None);
    }
}

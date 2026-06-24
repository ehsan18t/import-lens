use crate::ipc::protocol::RegistryHint;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CACHE_FILE_NAME: &str = "registry-hints.json";
const OK_CACHE_TTL_MS: u64 = 6 * 60 * 60 * 1000;
const NOT_FOUND_CACHE_TTL_MS: u64 = 6 * 60 * 60 * 1000;
const ERROR_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub struct RegistryHintStore {
    cache_path: Option<PathBuf>,
    cache: Arc<Mutex<HashMap<String, RegistryHintCacheEntry>>>,
    in_flight: Arc<Mutex<HashMap<String, Arc<InFlightRegistryHint>>>>,
    agent: ureq::Agent,
}

#[derive(Debug, Default)]
struct InFlightRegistryHint {
    result: Mutex<Option<Option<RegistryHint>>>,
    complete: Condvar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryHintCacheEntry {
    status: RegistryHintCacheStatus,
    timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_latest: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deprecated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RegistryHintCacheStatus {
    Ok,
    NotFound,
    Error,
}

#[derive(Debug, Deserialize)]
struct RegistryMetadata {
    #[serde(rename = "dist-tags")]
    dist_tags: Option<RegistryDistTags>,
    time: Option<HashMap<String, String>>,
    versions: Option<HashMap<String, RegistryVersionMetadata>>,
}

#[derive(Debug, Deserialize)]
struct RegistryDistTags {
    latest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersionMetadata {
    deprecated: Option<Value>,
}

impl RegistryHintStore {
    pub fn new(storage_path: Option<PathBuf>) -> Self {
        let cache_path = storage_path.map(|path| path.join(CACHE_FILE_NAME));
        let cache = cache_path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|contents| {
                serde_json::from_str::<HashMap<String, RegistryHintCacheEntry>>(&contents).ok()
            })
            .unwrap_or_default();
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REGISTRY_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();

        Self {
            cache_path,
            cache: Arc::new(Mutex::new(cache)),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            agent,
        }
    }

    pub fn hint_for_package(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        force_refresh: bool,
    ) -> Option<RegistryHint> {
        let key = cache_key(package_name, installed_version);
        let now = now_ms();

        if !force_refresh && let Some(entry) = self.cached_entry(&key, now) {
            return hint_from_entry(&entry);
        }

        let (flight, owner) = self.in_flight_entry(&key);
        if !owner {
            return wait_for_in_flight(&flight);
        }

        let result = self.fetch_uncached(package_name, installed_version);
        {
            let mut result_slot = flight.result.lock().expect("registry result lock poisoned");
            *result_slot = Some(result.clone());
            flight.complete.notify_all();
        }
        self.in_flight
            .lock()
            .expect("registry in-flight lock poisoned")
            .remove(&key);
        result
    }

    fn cached_entry(&self, key: &str, now: u64) -> Option<RegistryHintCacheEntry> {
        let entry = self
            .cache
            .lock()
            .expect("registry cache lock poisoned")
            .get(key)
            .cloned()?;

        if entry
            .retry_after
            .is_some_and(|retry_after| retry_after > now)
        {
            return Some(entry);
        }

        if now.saturating_sub(entry.timestamp) < cache_ttl_for_status(entry.status) {
            return Some(entry);
        }

        None
    }

    fn in_flight_entry(&self, key: &str) -> (Arc<InFlightRegistryHint>, bool) {
        let mut in_flight = self
            .in_flight
            .lock()
            .expect("registry in-flight lock poisoned");

        if let Some(existing) = in_flight.get(key) {
            return (Arc::clone(existing), false);
        }

        let flight = Arc::new(InFlightRegistryHint::default());
        in_flight.insert(key.to_owned(), Arc::clone(&flight));
        (flight, true)
    }

    fn fetch_uncached(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
    ) -> Option<RegistryHint> {
        let url = format!(
            "https://registry.npmjs.org/{}",
            package_name.replace('/', "%2F")
        );
        let mut retry_after = None;

        for attempt in 1..=MAX_ATTEMPTS {
            match self
                .agent
                .get(&url)
                .header("accept", "application/json")
                .call()
            {
                Ok(mut response) => {
                    let status = response.status().as_u16();

                    if (200..300).contains(&status) {
                        let Ok(body) = response.body_mut().read_to_string() else {
                            break;
                        };
                        let Ok(metadata) = serde_json::from_str::<RegistryMetadata>(&body) else {
                            break;
                        };
                        let timestamp = now_ms();
                        let hint = registry_hint_from_metadata(metadata, installed_version);
                        self.store_entry(
                            package_name,
                            installed_version,
                            RegistryHintCacheEntry {
                                status: RegistryHintCacheStatus::Ok,
                                timestamp,
                                latest_version: hint.latest_version.clone(),
                                latest_published_at: hint.latest_published_at.clone(),
                                is_latest: hint.is_latest,
                                deprecated: hint.deprecated,
                                retry_after: None,
                            },
                        );
                        return Some(RegistryHint {
                            fetched_at: Some(timestamp),
                            ..hint
                        });
                    }

                    if status == 404 {
                        self.store_entry(
                            package_name,
                            installed_version,
                            RegistryHintCacheEntry {
                                status: RegistryHintCacheStatus::NotFound,
                                timestamp: now_ms(),
                                latest_version: None,
                                latest_published_at: None,
                                is_latest: None,
                                deprecated: None,
                                retry_after: None,
                            },
                        );
                        return None;
                    }

                    retry_after = if status == 429 {
                        retry_after_delay_ms(&response, now_ms())
                    } else {
                        None
                    };

                    if attempt < MAX_ATTEMPTS {
                        std::thread::sleep(
                            retry_after
                                .map(Duration::from_millis)
                                .unwrap_or(RETRY_DELAY),
                        );
                        continue;
                    }
                }
                Err(_) if attempt < MAX_ATTEMPTS => {
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
                Err(_) => {}
            }

            break;
        }

        self.store_entry(
            package_name,
            installed_version,
            RegistryHintCacheEntry {
                status: RegistryHintCacheStatus::Error,
                timestamp: now_ms(),
                latest_version: None,
                latest_published_at: None,
                is_latest: None,
                deprecated: None,
                retry_after: retry_after.map(|delay| now_ms().saturating_add(delay)),
            },
        );
        None
    }

    fn store_entry(
        &self,
        package_name: &str,
        installed_version: Option<&str>,
        entry: RegistryHintCacheEntry,
    ) {
        {
            let mut cache = self.cache.lock().expect("registry cache lock poisoned");
            cache.insert(cache_key(package_name, installed_version), entry);
        }
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.cache_path else {
            return;
        };

        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return;
        }

        let Ok(cache) = self.cache.lock() else {
            return;
        };
        let Ok(contents) = serde_json::to_string(&*cache) else {
            return;
        };

        let _ = fs::write(path, contents);
    }
}

fn wait_for_in_flight(flight: &InFlightRegistryHint) -> Option<RegistryHint> {
    let mut result = flight.result.lock().expect("registry result lock poisoned");

    while result.is_none() {
        result = flight
            .complete
            .wait(result)
            .expect("registry condvar lock poisoned");
    }

    result.clone().unwrap_or(None)
}

fn registry_hint_from_metadata(
    metadata: RegistryMetadata,
    installed_version: Option<&str>,
) -> RegistryHint {
    let latest_version = metadata.dist_tags.and_then(|tags| tags.latest);
    let latest_published_at = latest_version
        .as_ref()
        .and_then(|latest| metadata.time.as_ref()?.get(latest).cloned());
    let version_for_deprecation = installed_version.or(latest_version.as_deref());
    let deprecated = version_for_deprecation
        .and_then(|version| metadata.versions.as_ref()?.get(version))
        .and_then(|version| version.deprecated.as_ref())
        .is_some_and(is_truthy_deprecation);
    let is_latest = installed_version
        .zip(latest_version.as_deref())
        .map(|(installed, latest)| installed == latest);

    RegistryHint {
        latest_version,
        latest_published_at,
        is_latest,
        deprecated: Some(deprecated),
        fetched_at: None,
    }
}

fn is_truthy_deprecation(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(value) if value.is_empty() => false,
        _ => true,
    }
}

fn hint_from_entry(entry: &RegistryHintCacheEntry) -> Option<RegistryHint> {
    if entry.status != RegistryHintCacheStatus::Ok {
        return None;
    }

    Some(RegistryHint {
        latest_version: entry.latest_version.clone(),
        latest_published_at: entry.latest_published_at.clone(),
        is_latest: entry.is_latest,
        deprecated: entry.deprecated,
        fetched_at: Some(entry.timestamp),
    })
}

fn cache_key(package_name: &str, installed_version: Option<&str>) -> String {
    format!("{package_name}\n{}", installed_version.unwrap_or(""))
}

fn cache_ttl_for_status(status: RegistryHintCacheStatus) -> u64 {
    match status {
        RegistryHintCacheStatus::Ok => OK_CACHE_TTL_MS,
        RegistryHintCacheStatus::NotFound => NOT_FOUND_CACHE_TTL_MS,
        RegistryHintCacheStatus::Error => ERROR_CACHE_TTL_MS,
    }
}

fn retry_after_delay_ms(response: &ureq::http::Response<ureq::Body>, now: u64) -> Option<u64> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    parse_http_date_millis(value).and_then(|timestamp| timestamp.checked_sub(now))
}

fn parse_http_date_millis(_value: &str) -> Option<u64> {
    None
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
};

static FORCED_INSERT_FAILURES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static INSERT_ATTEMPTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

fn forced_insert_failures() -> &'static Mutex<HashSet<String>> {
    FORCED_INSERT_FAILURES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn insert_attempts() -> &'static Mutex<Vec<String>> {
    INSERT_ATTEMPTS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn fail_inserts_for_keys<I, S>(keys: I)
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut failures = forced_insert_failures()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    failures.extend(keys.into_iter().map(Into::into));
}

pub(crate) fn unique_failure_token(name: &str) -> String {
    let id = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{id}")
}

pub(crate) fn clear_failures_for_token(token: &str) {
    forced_insert_failures()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|key| !key.contains(token));
}

pub(crate) fn clear_insert_attempts_for_token(token: &str) {
    insert_attempts()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|key| !key.contains(token));
}

pub(crate) fn take_insert_attempts_for_token(token: &str) -> Vec<String> {
    let mut attempts = insert_attempts()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut taken = Vec::new();
    attempts.retain(|key| {
        if key.contains(token) {
            taken.push(key.clone());
            false
        } else {
            true
        }
    });
    taken
}

pub(super) fn record_insert_attempt(key: &str) {
    insert_attempts()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(key.to_owned());
}

pub(super) fn should_fail_insert(key: &str) -> bool {
    forced_insert_failures()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(key)
}

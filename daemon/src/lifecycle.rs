use crate::time::unix_millis;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

pub const CACHE_RECYCLE_ENTRY_LIMIT: usize = 200_000;
const UPTIME_RECYCLE_AFTER: Duration = Duration::from_secs(4 * 60 * 60);
const IDLE_RECYCLE_AFTER: Duration = Duration::from_secs(15 * 60);
const RECYCLE_DETECTION_WINDOW: Duration = Duration::from_secs(10 * 60);
const RECYCLE_FILE_NAME: &str = "importlens-recycles.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleReason {
    IdleAfterUptime,
    CacheEntryLimit,
}

#[derive(Debug, Clone)]
pub struct LifecycleState {
    started_at: Instant,
    last_batch_at: Option<Instant>,
}

impl Default for LifecycleState {
    fn default() -> Self {
        Self::new()
    }
}

impl LifecycleState {
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub fn new_at(started_at: Instant) -> Self {
        Self {
            started_at,
            last_batch_at: None,
        }
    }

    pub fn record_batch(&mut self) {
        self.record_batch_at(Instant::now());
    }

    pub fn record_batch_at(&mut self, now: Instant) {
        self.last_batch_at = Some(now);
    }

    pub fn should_recycle(&self, now: Instant, cache_len: usize) -> Option<RecycleReason> {
        if cache_len > CACHE_RECYCLE_ENTRY_LIMIT {
            return Some(RecycleReason::CacheEntryLimit);
        }

        if now.saturating_duration_since(self.started_at) <= UPTIME_RECYCLE_AFTER {
            return None;
        }

        let last_active_at = self.last_batch_at.unwrap_or(self.started_at);

        (now.saturating_duration_since(last_active_at) > IDLE_RECYCLE_AFTER)
            .then_some(RecycleReason::IdleAfterUptime)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RecycleFile {
    recycles: Vec<u64>,
}

pub fn record_recycle_timestamp(storage_path: &Path, now: SystemTime) -> io::Result<()> {
    fs::create_dir_all(storage_path)?;

    let path = storage_path.join(RECYCLE_FILE_NAME);
    let mut file = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<RecycleFile>(&contents).ok())
        .unwrap_or_default();

    let now_millis = unix_millis(now);
    let cutoff = now_millis.saturating_sub(duration_millis(RECYCLE_DETECTION_WINDOW));
    file.recycles.retain(|timestamp| *timestamp >= cutoff);
    file.recycles.push(now_millis);
    file.recycles.sort_unstable();

    fs::write(path, serde_json::to_string(&file)?)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

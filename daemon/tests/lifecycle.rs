use import_lens_daemon::lifecycle::{
    CACHE_RECYCLE_ENTRY_LIMIT, LifecycleState, RecycleReason, record_recycle_timestamp,
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn temp_storage() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("import-lens-lifecycle-{suffix}"));
    fs::create_dir_all(&path).expect("temp storage should be created");
    path
}

fn recycle_file(storage_path: &Path) -> PathBuf {
    storage_path.join("importlens-recycles.json")
}

#[test]
fn lifecycle_recycles_after_four_hours_when_idle_for_fifteen_minutes() {
    let started_at = Instant::now();
    let mut lifecycle = LifecycleState::new_at(started_at);

    lifecycle.record_batch_at(started_at + Duration::from_secs(60));

    let reason = lifecycle.should_recycle(
        started_at + Duration::from_secs((4 * 60 * 60) + (15 * 60) + 1),
        10,
    );

    assert_eq!(reason, Some(RecycleReason::IdleAfterUptime));
}

#[test]
fn lifecycle_does_not_recycle_when_recent_batch_keeps_daemon_active() {
    let started_at = Instant::now();
    let mut lifecycle = LifecycleState::new_at(started_at);
    let now = started_at + Duration::from_secs((4 * 60 * 60) + 1);

    lifecycle.record_batch_at(now - Duration::from_secs(30));

    assert_eq!(lifecycle.should_recycle(now, 10), None);
}

#[test]
fn lifecycle_recycles_when_cache_exceeds_entry_limit() {
    let lifecycle = LifecycleState::new_at(Instant::now());

    assert_eq!(
        lifecycle.should_recycle(Instant::now(), CACHE_RECYCLE_ENTRY_LIMIT + 1),
        Some(RecycleReason::CacheEntryLimit)
    );
}

#[test]
fn record_recycle_timestamp_appends_millisecond_epoch_values() {
    let storage_path = temp_storage();
    let first = UNIX_EPOCH + Duration::from_millis(1000);
    let second = UNIX_EPOCH + Duration::from_millis(2000);

    record_recycle_timestamp(&storage_path, first).expect("first recycle should be recorded");
    record_recycle_timestamp(&storage_path, second).expect("second recycle should be recorded");

    let contents =
        fs::read_to_string(recycle_file(&storage_path)).expect("recycle file should be readable");

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
    assert_eq!(contents, "{\"recycles\":[1000,2000]}");
}

#[test]
fn record_recycle_timestamp_prunes_entries_outside_ten_minute_window() {
    let storage_path = temp_storage();
    fs::write(
        recycle_file(&storage_path),
        r#"{"recycles":[1000,610000,620000]}"#,
    )
    .expect("existing recycle file should be written");

    record_recycle_timestamp(&storage_path, UNIX_EPOCH + Duration::from_millis(620001))
        .expect("recycle should be recorded");

    let contents =
        fs::read_to_string(recycle_file(&storage_path)).expect("recycle file should be readable");

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
    assert_eq!(contents, "{\"recycles\":[610000,620000,620001]}");
}

use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_millis(time: SystemTime) -> u64 {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

pub fn unix_millis_now() -> u64 {
    unix_millis(SystemTime::now())
}

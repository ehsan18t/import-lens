use std::sync::OnceLock;

const PREFIX: &str = "[import-lens-daemon]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

static CURRENT_LEVEL: OnceLock<LogLevel> = OnceLock::new();

pub fn set_log_level(level: LogLevel) {
    let _ = CURRENT_LEVEL.set(level);
}

pub fn parse_log_level(value: &str) -> LogLevel {
    match value.to_ascii_lowercase().as_str() {
        "error" => LogLevel::Error,
        "warn" | "warning" => LogLevel::Warn,
        "debug" => LogLevel::Debug,
        _ => LogLevel::Info,
    }
}

pub fn current_log_level() -> LogLevel {
    *CURRENT_LEVEL.get().unwrap_or(&LogLevel::Info)
}

fn should_log(level: LogLevel) -> bool {
    level <= current_log_level()
}

fn write_log(level: LogLevel, component: &str, message: &str) {
    if !should_log(level) {
        return;
    }

    let timestamp = chrono_like_timestamp();
    let level_token = level_token(level);
    let line = if component.is_empty() {
        format!("{PREFIX} {timestamp} [{level_token}] {message}")
    } else {
        format!("{PREFIX} {timestamp} [{level_token}] [{component}] {message}")
    };

    match level {
        LogLevel::Error | LogLevel::Warn => {
            let _ =
                std::io::Write::write_all(&mut std::io::stderr(), format!("{line}\n").as_bytes());
        }
        LogLevel::Info | LogLevel::Debug => {
            let _ =
                std::io::Write::write_all(&mut std::io::stdout(), format!("{line}\n").as_bytes());
        }
    }
}

fn level_token(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
    }
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = duration.as_millis();
    let seconds = millis / 1000;
    let subsec_ms = (millis % 1000) as u32;

    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let hours = day_seconds / 3_600;
    let minutes = (day_seconds % 3_600) / 60;
    let secs = day_seconds % 60;

    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{secs:02}.{subsec_ms:03}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as i64, d as i64)
}

pub fn log_error(component: &str, message: impl AsRef<str>) {
    write_log(LogLevel::Error, component, message.as_ref());
}

pub fn log_warn(component: &str, message: impl AsRef<str>) {
    write_log(LogLevel::Warn, component, message.as_ref());
}

pub fn log_info(component: &str, message: impl AsRef<str>) {
    write_log(LogLevel::Info, component, message.as_ref());
}

pub fn log_debug(component: &str, message: impl AsRef<str>) {
    write_log(LogLevel::Debug, component, message.as_ref());
}

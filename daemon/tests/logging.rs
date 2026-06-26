use import_lens_daemon::logging::{LogLevel, current_log_level, parse_log_level};

#[test]
fn parse_log_level_accepts_known_values() {
    assert_eq!(parse_log_level("error"), LogLevel::Error);
    assert_eq!(parse_log_level("WARN"), LogLevel::Warn);
    assert_eq!(parse_log_level("info"), LogLevel::Info);
    assert_eq!(parse_log_level("debug"), LogLevel::Debug);
    assert_eq!(parse_log_level("unknown"), LogLevel::Info);
}

#[test]
fn current_log_level_defaults_to_info() {
    assert_eq!(current_log_level(), LogLevel::Info);
}

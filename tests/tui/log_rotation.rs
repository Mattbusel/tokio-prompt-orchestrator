//! Integration tests for log entry rotation and bounding.
//!
//! Verifies that the log VecDeque stays bounded, newest entries appear at
//! the back, and log entries contain valid data.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{App, LogEntry, LogLevel, LOG_ENTRIES_CAP};
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

#[test]
fn test_log_stays_bounded_under_heavy_generation() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..1000 {
        mock.tick(&mut app);
    }

    assert!(
        app.log_entries.len() <= LOG_ENTRIES_CAP,
        "Log entries exceeded cap: {} > {}",
        app.log_entries.len(),
        LOG_ENTRIES_CAP
    );
}

#[test]
fn test_newest_entries_at_back() {
    let mut app = App::new(Duration::from_secs(1));

    for i in 0..5 {
        app.push_log(LogEntry {
            timestamp: format!("00:00:{:02}", i),
            level: LogLevel::Info,
            message: format!("msg-{}", i),
            fields: String::new(),
        });
    }

    assert_eq!(
        app.log_entries.back().map(|e| e.message.as_str()),
        Some("msg-4")
    );
    assert_eq!(
        app.log_entries.front().map(|e| e.message.as_str()),
        Some("msg-0")
    );
}

#[test]
fn test_oldest_evicted_first() {
    let mut app = App::new(Duration::from_secs(1));

    // Fill to capacity
    for i in 0..LOG_ENTRIES_CAP {
        app.push_log(LogEntry {
            timestamp: format!("{}", i),
            level: LogLevel::Info,
            message: format!("entry-{}", i),
            fields: String::new(),
        });
    }

    assert_eq!(app.log_entries.len(), LOG_ENTRIES_CAP);
    assert_eq!(
        app.log_entries.front().map(|e| e.message.as_str()),
        Some("entry-0")
    );

    // Push one more
    app.push_log(LogEntry {
        timestamp: "new".into(),
        level: LogLevel::Warn,
        message: "newest".into(),
        fields: String::new(),
    });

    assert_eq!(app.log_entries.len(), LOG_ENTRIES_CAP);
    // entry-0 should be gone
    assert_eq!(
        app.log_entries.front().map(|e| e.message.as_str()),
        Some("entry-1")
    );
    assert_eq!(
        app.log_entries.back().map(|e| e.message.as_str()),
        Some("newest")
    );
}

#[test]
fn test_log_entries_have_timestamps() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..20 {
        mock.tick(&mut app);
    }

    for entry in &app.log_entries {
        assert!(
            !entry.timestamp.is_empty(),
            "Log entry timestamp should not be empty"
        );
    }
}

#[test]
fn test_log_entries_have_messages() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..20 {
        mock.tick(&mut app);
    }

    for entry in &app.log_entries {
        assert!(
            !entry.message.is_empty(),
            "Log entry message should not be empty"
        );
    }
}

#[test]
fn test_log_level_variety_from_mock() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..100 {
        mock.tick(&mut app);
    }

    let info_count = app
        .log_entries
        .iter()
        .filter(|e| e.level == LogLevel::Info)
        .count();
    assert!(info_count > 0, "Should have INFO entries");
}

#[test]
fn test_empty_log_operations() {
    let app = App::new(Duration::from_secs(1));
    assert!(app.log_entries.is_empty());
    assert_eq!(app.log_entries.front(), None);
    assert_eq!(app.log_entries.back(), None);
}

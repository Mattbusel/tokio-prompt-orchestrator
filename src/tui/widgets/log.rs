//! # Widget: Log Tail
//!
//! ## Responsibility
//! Renders the last N log entries with color-coded severity levels.
//! INFO=white, WARN=yellow, ERROR=red, DEBUG=gray.
//!
//! ## Guarantees
//! - Fixed-width timestamp column for alignment
//! - Long lines truncated with `…` rather than wrapping
//! - Handles empty log list gracefully
//! - Newest entries appear at the bottom

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, LogLevel};

/// Returns the display color for a log level.
///
/// # Arguments
/// * `level` - The log severity level.
///
/// # Returns
/// White for Info, Yellow for Warn, Red for Error, DarkGray for Debug.
pub fn level_color(level: LogLevel) -> Color {
    match level {
        LogLevel::Info => Color::White,
        LogLevel::Warn => Color::Yellow,
        LogLevel::Error => Color::Red,
        LogLevel::Debug => Color::DarkGray,
    }
}

/// Truncates a string to a maximum width, adding `…` if truncated.
///
/// # Arguments
/// * `s` - The string to potentially truncate.
/// * `max_width` - Maximum character width.
///
/// # Returns
/// The string unchanged if it fits, or truncated with trailing `…`.
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.len() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    format!("{}\u{2026}", &s[..max_width - 1])
}

/// Renders the log tail widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing log entries.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" LOG ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible_count = inner.height as usize;
    let max_line_width = inner.width as usize;

    let entries: Vec<&crate::tui::app::LogEntry> = app
        .log_entries
        .iter()
        .rev()
        .take(visible_count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let lines: Vec<Line> = entries
        .iter()
        .map(|entry| {
            let color = level_color(entry.level);
            let prefix = format!("[{}] {}  ", entry.timestamp, entry.level.label(),);

            let remaining_width = max_line_width.saturating_sub(prefix.len());
            let body = if entry.fields.is_empty() {
                entry.message.clone()
            } else {
                format!("{:<22} {}", entry.message, entry.fields)
            };
            let truncated_body = truncate_with_ellipsis(&body, remaining_width);

            Line::from(vec![
                Span::styled(
                    format!("[{}] ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{}  ", entry.level.label()),
                    Style::default().fg(color),
                ),
                Span::styled(truncated_body, Style::default().fg(color)),
            ])
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_level_color_info_white() {
        assert_eq!(level_color(LogLevel::Info), Color::White);
    }

    #[test]
    fn test_level_color_warn_yellow() {
        assert_eq!(level_color(LogLevel::Warn), Color::Yellow);
    }

    #[test]
    fn test_level_color_error_red() {
        assert_eq!(level_color(LogLevel::Error), Color::Red);
    }

    #[test]
    fn test_level_color_debug_gray() {
        assert_eq!(level_color(LogLevel::Debug), Color::DarkGray);
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let result = truncate_with_ellipsis("hello world", 6);
        assert_eq!(result, "hello\u{2026}");
    }

    #[test]
    fn test_truncate_width_one() {
        let result = truncate_with_ellipsis("hello", 1);
        assert_eq!(result, "\u{2026}");
    }

    #[test]
    fn test_truncate_width_zero() {
        let result = truncate_with_ellipsis("hello", 0);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn test_truncate_width_two() {
        let result = truncate_with_ellipsis("hello", 2);
        assert_eq!(result, "h\u{2026}");
    }
}

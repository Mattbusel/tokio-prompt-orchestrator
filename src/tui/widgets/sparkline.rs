//! # Widget: Throughput Sparkline
//!
//! ## Responsibility
//! Renders a throughput sparkline chart showing requests per second over the
//! last 60 seconds using Ratatui's built-in Sparkline widget.
//!
//! ## Guarantees
//! - Handles empty data gracefully (shows empty chart)
//! - Y-axis adapts to current max value
//! - Never panics on any data range

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Sparkline as RatatuiSparkline};
use ratatui::Frame;

use crate::tui::app::App;

/// Renders the throughput sparkline widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing throughput history.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let current_rps = app.throughput_history.back().copied().unwrap_or(0);
    let title = format!(" THROUGHPUT ({} req/s) ", current_rps);

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let data: Vec<u64> = app.throughput_history.iter().copied().collect();

    let sparkline = RatatuiSparkline::default()
        .block(block)
        .data(&data)
        .style(Style::default().fg(Color::Cyan));

    f.render_widget(sparkline, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_empty_throughput_no_panic() {
        let app = App::new(Duration::from_secs(1));
        assert!(app.throughput_history.is_empty());
        // The render function uses unwrap_or(0) for empty data
        let current = app.throughput_history.back().copied().unwrap_or(0);
        assert_eq!(current, 0);
    }

    #[test]
    fn test_throughput_data_collection() {
        let mut app = App::new(Duration::from_secs(1));
        app.push_throughput(10);
        app.push_throughput(20);
        app.push_throughput(15);
        let data: Vec<u64> = app.throughput_history.iter().copied().collect();
        assert_eq!(data, vec![10, 20, 15]);
    }

    #[test]
    fn test_current_rps_is_last_value() {
        let mut app = App::new(Duration::from_secs(1));
        app.push_throughput(10);
        app.push_throughput(25);
        assert_eq!(app.throughput_history.back().copied().unwrap_or(0), 25);
    }
}

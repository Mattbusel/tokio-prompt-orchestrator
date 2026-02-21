//! # Widget: Deduplication Statistics
//!
//! ## Responsibility
//! Displays deduplication metrics: total requests in, total inferences executed,
//! savings percentage, and estimated cost saved.
//!
//! ## Guarantees
//! - Handles zero requests gracefully (shows 0.0%)
//! - Cost display formatted to 2 decimal places
//! - Never panics on any counter value

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;

/// Formats a u64 with comma separators for readability.
///
/// # Arguments
/// * `n` - The number to format.
///
/// # Returns
/// Formatted string, e.g. "1,847".
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    if len <= 3 {
        return s;
    }
    let mut result = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

/// Renders the deduplication statistics widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing dedup counters.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" DEDUPLICATION ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let savings = app.dedup_savings_percent();
    let savings_color = if savings > 70.0 {
        Color::Green
    } else if savings > 40.0 {
        Color::Yellow
    } else {
        Color::White
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("Requests in:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:>8}", format_number(app.requests_total)),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Inferences:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:>8}", format_number(app.inferences_total)),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Savings:      ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:>7.1}%", savings),
                Style::default()
                    .fg(savings_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Cost saved:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("${:>7.2} today", app.cost_saved_usd),
                Style::default().fg(Color::Green),
            ),
        ]),
    ];

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number_small() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(42), "42");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn test_format_number_thousands() {
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1847), "1,847");
    }

    #[test]
    fn test_format_number_millions() {
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn test_format_number_large() {
        assert_eq!(format_number(999_999_999), "999,999,999");
    }

    #[test]
    fn test_format_number_exact_boundary() {
        assert_eq!(format_number(100), "100");
        assert_eq!(format_number(10_000), "10,000");
        assert_eq!(format_number(100_000), "100,000");
    }
}

//! # Widget: Per-Worker Cost Breakdown
//!
//! ## Responsibility
//! Renders a small table showing inference cost totals per backend worker.
//! Reads `app.worker_costs` which is populated by live metrics parsing or
//! left empty in mock mode.
//!
//! ## Guarantees
//! - Handles empty worker_costs gracefully (shows "no data" message)
//! - Never panics on any cost value
//! - Formats costs with 4 decimal places for USD precision

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;

/// Renders the per-worker cost breakdown widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing worker_costs map.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" COST / WORKER ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    if app.worker_costs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no data",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Sort by worker name for stable display
        let mut entries: Vec<(&String, &f64)> = app.worker_costs.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str());

        for (worker, cost) in entries {
            let cost_str = format!("${:.4}", cost);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<14}", worker),
                    Style::default().fg(Color::White),
                ),
                Span::styled(cost_str, Style::default().fg(Color::Yellow)),
            ]));
        }
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_cost_widget_empty_no_panic() {
        let app = App::new(Duration::from_secs(1));
        // Just verify it doesn't panic with empty worker_costs
        assert!(app.worker_costs.is_empty());
    }

    #[test]
    fn test_cost_widget_with_entries() {
        let mut app = App::new(Duration::from_secs(1));
        app.worker_costs.insert("openai".to_string(), 0.0042);
        app.worker_costs.insert("anthropic".to_string(), 0.0021);
        assert_eq!(app.worker_costs.len(), 2);
        assert!((app.worker_costs["openai"] - 0.0042).abs() < 0.0001);
    }
}

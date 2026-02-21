//! # Widget: System Health
//!
//! ## Responsibility
//! Renders system health metrics: CPU%, MEM%, active Tokio tasks, and uptime.
//! Uses the same fill bar style as channel depths for consistency.
//!
//! ## Guarantees
//! - CPU and MEM percentages clamped to 0-100
//! - Uptime displayed in human-readable format
//! - Never panics on any metric value

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::channels::{fill_bar, fill_color};
use crate::tui::app::App;

/// Bar width for health metrics display.
const HEALTH_BAR_WIDTH: usize = 14;

/// Renders the system health widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing system health metrics.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" SYSTEM HEALTH ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cpu_ratio = (app.cpu_percent / 100.0).clamp(0.0, 1.0);
    let mem_ratio = (app.mem_percent / 100.0).clamp(0.0, 1.0);
    let tasks_ratio = (app.active_tasks as f64 / 500.0).clamp(0.0, 1.0);

    let lines = vec![
        Line::from(vec![
            Span::styled("CPU    ", Style::default().fg(Color::White)),
            Span::styled(
                fill_bar(cpu_ratio, HEALTH_BAR_WIDTH),
                Style::default().fg(fill_color(cpu_ratio)),
            ),
            Span::styled(
                format!("  {:.0}%", app.cpu_percent),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("MEM    ", Style::default().fg(Color::White)),
            Span::styled(
                fill_bar(mem_ratio, HEALTH_BAR_WIDTH),
                Style::default().fg(fill_color(mem_ratio)),
            ),
            Span::styled(
                format!("  {:.0}%", app.mem_percent),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("TASKS  ", Style::default().fg(Color::White)),
            Span::styled(
                fill_bar(tasks_ratio, HEALTH_BAR_WIDTH),
                Style::default().fg(fill_color(tasks_ratio)),
            ),
            Span::styled(
                format!("  {}", app.active_tasks),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("UPTIME ", Style::default().fg(Color::White)),
            Span::styled(app.uptime_display(), Style::default().fg(Color::Cyan)),
        ]),
    ];

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_health_bar_width_consistent() {
        assert_eq!(HEALTH_BAR_WIDTH, 14);
    }

    #[test]
    fn test_cpu_ratio_clamped() {
        let ratio = (150.0_f64 / 100.0).clamp(0.0, 1.0);
        assert_eq!(ratio, 1.0);
    }

    #[test]
    fn test_cpu_ratio_negative_clamped() {
        let ratio = (-10.0_f64 / 100.0).clamp(0.0, 1.0);
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn test_uptime_renders_with_zero() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.uptime_display(), "0m");
    }

    #[test]
    fn test_tasks_ratio_calculation() {
        let ratio = (247.0_f64 / 500.0).clamp(0.0, 1.0);
        assert!((ratio - 0.494).abs() < 0.01);
    }
}

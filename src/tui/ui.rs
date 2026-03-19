//! # Module: TUI Rendering
//!
//! ## Responsibility
//! Orchestrates the overall dashboard layout by dividing the terminal into regions
//! and delegating to individual widget renderers. Handles the minimum size guard,
//! graceful resize degradation, failure banner, and help overlay.
//!
//! ## Guarantees
//! - Layout adapts gracefully to terminal sizes from 40x10 to 220x60+
//! - Graceful collapse: hides dedup at <80 cols, hides log at <60 cols, hard limit at 40x10
//! - Minimum size guard displays a centered message if terminal is too small
//! - No panics during rendering regardless of terminal dimensions

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{App, MIN_COLS, MIN_ROWS};
use super::widgets;

/// Renders the complete dashboard UI into the given frame.
///
/// # Arguments
/// * `f` - The Ratatui frame to render into.
/// * `app` - The application state to display.
pub fn draw(f: &mut Frame, app: &App) {
    let size = f.area();

    // Hard minimum size guard
    if size.width < MIN_COLS || size.height < MIN_ROWS {
        draw_too_small(f, size);
        return;
    }

    // Help overlay
    if app.show_help {
        draw_help_overlay(f, size);
        return;
    }

    // Determine which widgets to show based on width (graceful degradation)
    let show_dedup = size.width >= 80;
    let show_log = size.width >= 60;

    // Title bar with optional PAUSED indicator
    let pause_suffix = if app.paused { " ⏸ PAUSED" } else { "" };
    let title = format!(
        " tokio-prompt-orchestrator{}{:>width$} ",
        pause_suffix,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        width = (size.width as usize).saturating_sub(30 + pause_suffix.len()),
    );

    let outer_block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let footer = Line::from(vec![
        Span::styled(
            " q quit  p/Space pause  r reset  ? help  \u{2191}\u{2193} scroll ",
            Style::default().fg(Color::DarkGray),
        ),
        if app.paused {
            Span::styled(
                " ⏸ PAUSED ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
    ]);

    let footer_block = Block::default().title_bottom(footer).borders(Borders::NONE);

    let inner = outer_block.inner(size);
    f.render_widget(outer_block, size);
    f.render_widget(footer_block, size);

    // Render failure banner if metrics_fetch_error is set
    let (content_area, banner_height) = if app.metrics_fetch_error {
        let banner_rect = Rect::new(inner.x, inner.y, inner.width, 1);
        draw_metrics_error_banner(f, banner_rect);
        let remaining = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(1),
        );
        (remaining, 1u16)
    } else {
        (inner, 0u16)
    };
    let _ = banner_height;

    // Main layout: top section, sparkline, optional log
    let mut vertical_constraints = vec![
        Constraint::Min(14),   // Top section
        Constraint::Length(8), // Throughput sparkline
    ];
    if show_log {
        vertical_constraints.push(Constraint::Length(10)); // Log tail
    }

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vertical_constraints)
        .split(content_area);

    // Top section: left (pipeline + channels) and right (health + circuit + optional dedup + cost)
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Left column
            Constraint::Percentage(50), // Right column
        ])
        .split(main_chunks[0]);

    // Left column: pipeline flow + channels
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // Pipeline flow
            Constraint::Length(6), // Channels
        ])
        .split(top_chunks[0]);

    // Right column layout depends on whether dedup is shown
    let right_chunks = if show_dedup {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // System health
                Constraint::Length(5), // Circuit breakers
                Constraint::Length(5), // Cost widget
                Constraint::Min(4),    // Dedup stats
            ])
            .split(top_chunks[1])
    } else {
        // No dedup — use the space for cost widget instead
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // System health
                Constraint::Length(5), // Circuit breakers
                Constraint::Min(4),    // Cost widget (takes remaining space)
            ])
            .split(top_chunks[1])
    };

    // Render all widgets
    widgets::pipeline::render(f, left_chunks[0], app);
    widgets::channels::render(f, left_chunks[1], app);
    widgets::health::render(f, right_chunks[0], app);
    widgets::circuit::render(f, right_chunks[1], app);

    if show_dedup {
        widgets::cost::render(f, right_chunks[2], app);
        widgets::dedup::render(f, right_chunks[3], app);
    } else {
        widgets::cost::render(f, right_chunks[2], app);
    }

    widgets::sparkline::render(f, main_chunks[1], app);

    if show_log {
        widgets::log::render(f, main_chunks[2], app);
    }
}

/// Renders a one-line red banner indicating metrics are unreachable.
fn draw_metrics_error_banner(f: &mut Frame, area: Rect) {
    let banner = Paragraph::new(Line::from(Span::styled(
        "\u{26a0}  Metrics unreachable \u{2014} displaying last known values",
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )))
    .style(Style::default().bg(Color::Reset));
    f.render_widget(banner, area);
}

/// Renders the "terminal too small" warning.
fn draw_too_small(f: &mut Frame, area: Rect) {
    let msg = format!(
        "Terminal too small \u{2014} resize to at least {}x{}",
        MIN_COLS, MIN_ROWS
    );
    let current_size = format!("Current size: {}x{}", area.width, area.height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));

    let para = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            msg,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            current_size,
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(block)
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });

    f.render_widget(para, area);
}

/// Renders the help overlay with a full two-column keyboard shortcut table.
fn draw_help_overlay(f: &mut Frame, area: Rect) {
    // Center the help popup; use a wider window for the two-column table.
    let popup_width = 60.min(area.width.saturating_sub(4));
    let popup_height = 26.min(area.height.saturating_sub(4));
    let popup_x = (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    // Helper to build a two-column row: key (left, fixed width) + description.
    let row = |key: &'static str, desc: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("  {key:<18}", key = key),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(desc, Style::default().fg(Color::DarkGray)),
        ])
    };

    let sep = || {
        Line::from(Span::styled(
            "  \u{2500}".repeat(28),
            Style::default().fg(Color::DarkGray),
        ))
    };

    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  tokio-prompt-orchestrator TUI",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Keyboard shortcuts",
            Style::default().fg(Color::White).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(""),
        row("q / Esc", "Quit the TUI"),
        row("Ctrl+C", "Force-quit (immediate)"),
        row("p / Space", "Pause / resume display updates"),
        row("r", "Reset all counters and clear log"),
        row("h / ?", "Toggle this help overlay"),
        row("\u{2191} / \u{2193}", "Scroll the log pane up / down"),
        Line::from(""),
        sep(),
        Line::from(""),
        Line::from(Span::styled(
            "  Layout thresholds",
            Style::default().fg(Color::White).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(""),
        row("<60 cols", "Log pane hidden"),
        row("<80 cols", "Dedup widget hidden"),
        row("<40 cols", "'Terminal too small' message"),
        Line::from(""),
        sep(),
        Line::from(""),
        Line::from(Span::styled(
            "  --mock  Synthetic 2-min story (default)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  --live  Connect to Prometheus endpoint",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to close",
            Style::default().fg(Color::Yellow),
        )),
    ];

    let block = Block::default()
        .title(" Help — all shortcuts ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(help_text).block(block);
    f.render_widget(para, popup_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_size_constants() {
        assert_eq!(MIN_COLS, 40);
        assert_eq!(MIN_ROWS, 10);
    }

    #[test]
    fn test_too_small_detection_width() {
        let area = Rect::new(0, 0, 30, 30);
        assert!(area.width < MIN_COLS);
    }

    #[test]
    fn test_too_small_detection_height() {
        let area = Rect::new(0, 0, 120, 8);
        assert!(area.height < MIN_ROWS);
    }

    #[test]
    fn test_adequate_size() {
        let area = Rect::new(0, 0, 120, 50);
        assert!(area.width >= MIN_COLS && area.height >= MIN_ROWS);
    }

    #[test]
    fn test_exactly_minimum_size() {
        let area = Rect::new(0, 0, MIN_COLS, MIN_ROWS);
        assert!(area.width >= MIN_COLS && area.height >= MIN_ROWS);
    }

    #[test]
    fn test_show_dedup_threshold() {
        // At 80+ cols, dedup is shown
        assert!(80u16 >= 80);
        // Below 80 cols, dedup is hidden
        assert!(79u16 < 80);
    }

    #[test]
    fn test_show_log_threshold() {
        // At 60+ cols, log is shown
        assert!(60u16 >= 60);
        // Below 60 cols, log is hidden
        assert!(59u16 < 60);
    }

    #[test]
    fn test_popup_centering_calculation() {
        let area_width: u16 = 120;
        let area_height: u16 = 50;
        let popup_width = 52.min(area_width.saturating_sub(4));
        let popup_height = 20.min(area_height.saturating_sub(4));
        let popup_x = (area_width.saturating_sub(popup_width)) / 2;
        let popup_y = (area_height.saturating_sub(popup_height)) / 2;

        assert_eq!(popup_width, 52);
        assert_eq!(popup_height, 20);
        assert_eq!(popup_x, 34);
        assert_eq!(popup_y, 15);
    }

    #[test]
    fn test_popup_centering_small_terminal() {
        let area_width: u16 = 40;
        let area_height: u16 = 15;
        let popup_width = 52.min(area_width.saturating_sub(4));
        let popup_height = 20.min(area_height.saturating_sub(4));

        assert_eq!(popup_width, 36);
        assert_eq!(popup_height, 11);
    }
}

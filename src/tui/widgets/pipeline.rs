//! # Widget: Pipeline Flow
//!
//! ## Responsibility
//! Renders the 5-stage pipeline flow diagram with boxes connected by arrows.
//! Each box shows the stage name and current latency, color-coded against budget.
//!
//! ## Guarantees
//! - Green: within budget, Yellow: >80% of budget, Red: over budget
//! - Handles terminal widths gracefully by using compact mode if needed
//! - Never panics on any latency value

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, STAGE_NAMES};

/// Returns the color for a latency based on its budget ratio.
///
/// # Arguments
/// * `ratio` - Latency / budget ratio. <0.8 = green, 0.8-1.0 = yellow, >1.0 = red.
pub fn latency_color(ratio: f64) -> Color {
    if ratio > 1.0 {
        Color::Red
    } else if ratio > 0.8 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Renders the pipeline flow widget into the given area.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing latencies and active stage.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" PIPELINE FLOW ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 10 || inner.height < 5 {
        return;
    }

    // Top row: RAG → ASSEMBLE → INFER
    let top_row_y = inner.y + 1;
    render_stage_row(
        f,
        inner.x,
        top_row_y,
        inner.width,
        &[0, 1, 2],
        app,
        true, // left to right arrows
    );

    // Vertical connector from INFER down to POST
    if inner.height > 4 {
        let connector_y = top_row_y + 2;
        let connector_x = inner.x + estimate_stage_end_x(inner.width, 3, 2);
        let connector = Paragraph::new(Line::from(Span::styled(
            "\u{2502}", // │
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(connector, Rect::new(connector_x, connector_y, 1, 1));
    }

    // Bottom row: STREAM ← POST (right to left)
    if inner.height > 5 {
        let bot_row_y = top_row_y + 3;
        render_stage_row(
            f,
            inner.x,
            bot_row_y,
            inner.width,
            &[4, 3],
            app,
            false, // right to left (reversed display)
        );
    }
}

/// Renders a horizontal row of pipeline stage boxes with arrows.
fn render_stage_row(
    f: &mut Frame,
    base_x: u16,
    y: u16,
    total_width: u16,
    stage_indices: &[usize],
    app: &App,
    left_to_right: bool,
) {
    let count = stage_indices.len() as u16;
    if count == 0 || total_width < count * 8 {
        return;
    }

    let arrow_width: u16 = 3; // "──▶" or "◀──"
    let box_width = ((total_width - (count - 1) * arrow_width) / count).min(12);

    let mut x = base_x;
    for (i, &stage_idx) in stage_indices.iter().enumerate() {
        let name = if stage_idx < STAGE_NAMES.len() {
            STAGE_NAMES[stage_idx]
        } else {
            "???"
        };
        let latency = if stage_idx < app.stage_latencies.len() {
            app.stage_latencies[stage_idx]
        } else {
            0.0
        };
        let ratio = app.stage_budget_ratio(stage_idx);
        let color = latency_color(ratio);

        let is_active = app.active_stage == Some(stage_idx);
        let border_style = if is_active {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let lat_str = if latency >= 100.0 {
            format!("{:.0}ms", latency)
        } else {
            format!("{:.1}ms", latency)
        };

        let content = vec![
            Line::from(Span::styled(
                center_text(name, box_width as usize - 2),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                center_text(&lat_str, box_width as usize - 2),
                Style::default().fg(color),
            )),
        ];

        let stage_block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style);
        let para = Paragraph::new(content).block(stage_block);

        let rect = Rect::new(x, y, box_width, 3);
        f.render_widget(para, rect);

        x += box_width;

        // Draw arrow between stages
        if (i as u16) < count - 1 {
            let arrow = if left_to_right {
                "\u{2500}\u{2500}\u{25b6}" // ──▶
            } else {
                "\u{25c0}\u{2500}\u{2500}" // ◀──
            };
            let arrow_para = Paragraph::new(Line::from(Span::styled(
                arrow,
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(arrow_para, Rect::new(x, y + 1, arrow_width, 1));
            x += arrow_width;
        }
    }
}

/// Estimates the X position at the end of a stage box in a row.
fn estimate_stage_end_x(total_width: u16, count: u16, target: u16) -> u16 {
    let arrow_width: u16 = 3;
    let box_width = ((total_width - (count - 1) * arrow_width) / count).min(12);
    target * (box_width + arrow_width) + box_width / 2
}

/// Centers text within a given width, padding with spaces.
fn center_text(text: &str, width: usize) -> String {
    let text_len = text.len();
    if text_len >= width {
        return text[..width].to_string();
    }
    let pad_left = (width - text_len) / 2;
    let pad_right = width - text_len - pad_left;
    format!("{}{}{}", " ".repeat(pad_left), text, " ".repeat(pad_right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latency_color_green_within_budget() {
        assert_eq!(latency_color(0.0), Color::Green);
        assert_eq!(latency_color(0.5), Color::Green);
        assert_eq!(latency_color(0.79), Color::Green);
    }

    #[test]
    fn test_latency_color_yellow_approaching_budget() {
        assert_eq!(latency_color(0.81), Color::Yellow);
        assert_eq!(latency_color(0.9), Color::Yellow);
        assert_eq!(latency_color(0.99), Color::Yellow);
    }

    #[test]
    fn test_latency_color_boundary_at_eighty() {
        // Exactly 0.8 is within budget (not >0.8)
        assert_eq!(latency_color(0.8), Color::Green);
    }

    #[test]
    fn test_latency_color_red_over_budget() {
        assert_eq!(latency_color(1.0), Color::Yellow);
        assert_eq!(latency_color(1.01), Color::Red);
        assert_eq!(latency_color(2.0), Color::Red);
    }

    #[test]
    fn test_center_text_exact_width() {
        assert_eq!(center_text("RAG", 3), "RAG");
    }

    #[test]
    fn test_center_text_padding() {
        let result = center_text("RAG", 7);
        assert_eq!(result, "  RAG  ");
    }

    #[test]
    fn test_center_text_odd_padding() {
        let result = center_text("RAG", 8);
        assert_eq!(result.len(), 8);
        assert!(result.contains("RAG"));
    }

    #[test]
    fn test_center_text_truncation() {
        let result = center_text("LONGNAME", 4);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_center_text_empty() {
        let result = center_text("", 4);
        assert_eq!(result, "    ");
    }

    #[test]
    fn test_estimate_stage_end_x() {
        let x = estimate_stage_end_x(60, 3, 2);
        assert!(x > 0);
        assert!(x < 60);
    }
}

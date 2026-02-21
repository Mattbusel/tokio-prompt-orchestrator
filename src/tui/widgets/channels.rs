//! # Widget: Channel Fill Bars
//!
//! ## Responsibility
//! Renders bounded channel queue depths as fill bars using Unicode block characters.
//! Color-coded: green <60%, yellow 60-85%, red >85%.
//!
//! ## Guarantees
//! - Fill bars render correctly at 0%, 50%, 85%, and 100%
//! - Never panics on any depth/capacity combination including zero capacity

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;

/// Bar width in characters for channel fill display.
const BAR_WIDTH: usize = 18;

/// Returns the color for a fill ratio.
///
/// # Arguments
/// * `ratio` - Fill ratio from 0.0 to 1.0.
///
/// # Returns
/// Green if <0.60, Yellow if 0.60-0.85, Red if >0.85.
pub fn fill_color(ratio: f64) -> Color {
    if ratio > 0.85 {
        Color::Red
    } else if ratio > 0.60 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Builds a fill bar string using Unicode block characters.
///
/// # Arguments
/// * `ratio` - Fill ratio from 0.0 to 1.0.
/// * `width` - Total bar width in characters.
///
/// # Returns
/// String with `\u{2588}` (filled) and `\u{2591}` (empty) characters.
pub fn fill_bar(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Renders the channels widget into the given area.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing channel depths.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" CHANNELS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for ch in &app.channel_depths {
        let ratio = ch.fill_ratio();
        let color = fill_color(ratio);
        let bar = fill_bar(ratio, BAR_WIDTH);
        let depth_str = format!("{:>4}/{}", ch.current, ch.capacity);

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<10}", ch.name),
                Style::default().fg(Color::White),
            ),
            Span::raw("["),
            Span::styled(bar, Style::default().fg(color)),
            Span::raw("] "),
            Span::styled(depth_str, Style::default().fg(Color::DarkGray)),
        ]));
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fill_color_green_low() {
        assert_eq!(fill_color(0.0), Color::Green);
        assert_eq!(fill_color(0.3), Color::Green);
        assert_eq!(fill_color(0.59), Color::Green);
    }

    #[test]
    fn test_fill_color_yellow_medium() {
        assert_eq!(fill_color(0.61), Color::Yellow);
        assert_eq!(fill_color(0.75), Color::Yellow);
        assert_eq!(fill_color(0.85), Color::Yellow);
    }

    #[test]
    fn test_fill_color_boundary_at_sixty() {
        // Exactly 0.60 is green (not >0.60)
        assert_eq!(fill_color(0.60), Color::Green);
    }

    #[test]
    fn test_fill_color_red_high() {
        assert_eq!(fill_color(0.86), Color::Red);
        assert_eq!(fill_color(0.95), Color::Red);
        assert_eq!(fill_color(1.0), Color::Red);
    }

    #[test]
    fn test_fill_bar_empty() {
        let bar = fill_bar(0.0, 10);
        assert_eq!(bar.chars().count(), 10);
        assert!(!bar.contains('\u{2588}'));
    }

    #[test]
    fn test_fill_bar_full() {
        let bar = fill_bar(1.0, 10);
        assert_eq!(bar.chars().count(), 10);
        assert!(!bar.contains('\u{2591}'));
    }

    #[test]
    fn test_fill_bar_half() {
        let bar = fill_bar(0.5, 10);
        assert_eq!(bar.chars().count(), 10);
        let filled: usize = bar.chars().filter(|&c| c == '\u{2588}').count();
        assert_eq!(filled, 5);
    }

    #[test]
    fn test_fill_bar_standard_width() {
        let bar = fill_bar(0.5, BAR_WIDTH);
        assert_eq!(bar.chars().count(), BAR_WIDTH);
    }

    #[test]
    fn test_fill_bar_clamps_over_one() {
        let bar = fill_bar(1.5, 10);
        assert_eq!(bar.chars().count(), 10);
        let filled: usize = bar.chars().filter(|&c| c == '\u{2588}').count();
        assert_eq!(filled, 10);
    }

    #[test]
    fn test_fill_bar_clamps_negative() {
        let bar = fill_bar(-0.5, 10);
        assert_eq!(bar.chars().count(), 10);
        let filled: usize = bar.chars().filter(|&c| c == '\u{2588}').count();
        assert_eq!(filled, 0);
    }

    #[test]
    fn test_fill_bar_zero_width() {
        let bar = fill_bar(0.5, 0);
        assert_eq!(bar.len(), 0);
    }

    #[test]
    fn test_fill_bar_at_eighty_five_percent() {
        let bar = fill_bar(0.85, 20);
        let filled: usize = bar.chars().filter(|&c| c == '\u{2588}').count();
        assert_eq!(filled, 17); // round(0.85 * 20) = 17
    }

    #[test]
    fn test_fill_bar_at_hundred_percent() {
        let bar = fill_bar(1.0, BAR_WIDTH);
        let filled: usize = bar.chars().filter(|&c| c == '\u{2588}').count();
        assert_eq!(filled, BAR_WIDTH);
    }
}

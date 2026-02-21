//! # Widget: Circuit Breaker Status
//!
//! ## Responsibility
//! Renders circuit breaker states for each worker backend with colored symbols.
//! - `●` CLOSED — green
//! - `◐` HALF-OPEN — yellow, with countdown
//! - `○` OPEN — red, with duration since opened
//!
//! ## Guarantees
//! - All three states display correctly with proper symbols and colors
//! - Handles empty circuit breaker list gracefully

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, CircuitState};

/// Returns the display color for a circuit breaker state.
///
/// # Arguments
/// * `state` - The circuit breaker state.
///
/// # Returns
/// Green for Closed, Yellow for HalfOpen, Red for Open.
pub fn state_color(state: CircuitState) -> Color {
    match state {
        CircuitState::Closed => Color::Green,
        CircuitState::HalfOpen => Color::Yellow,
        CircuitState::Open => Color::Red,
    }
}

/// Renders the circuit breaker status widget.
///
/// # Arguments
/// * `f` - Ratatui frame to render into.
/// * `area` - Rectangular area allocated for this widget.
/// * `app` - Application state containing circuit breaker statuses.
pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" CIRCUIT BREAKERS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = app
        .circuit_breakers
        .iter()
        .map(|cb| {
            let color = state_color(cb.state);
            Line::from(vec![
                Span::styled(
                    format!("{:<12}", cb.name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(cb.state.symbol(), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    format!("{:<10}", cb.state.label()),
                    Style::default().fg(color),
                ),
                Span::styled(&cb.detail, Style::default().fg(Color::DarkGray)),
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
    fn test_state_color_closed_is_green() {
        assert_eq!(state_color(CircuitState::Closed), Color::Green);
    }

    #[test]
    fn test_state_color_half_open_is_yellow() {
        assert_eq!(state_color(CircuitState::HalfOpen), Color::Yellow);
    }

    #[test]
    fn test_state_color_open_is_red() {
        assert_eq!(state_color(CircuitState::Open), Color::Red);
    }

    #[test]
    fn test_all_states_have_distinct_colors() {
        let colors = [
            state_color(CircuitState::Closed),
            state_color(CircuitState::HalfOpen),
            state_color(CircuitState::Open),
        ];
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    #[test]
    fn test_circuit_state_symbol_nonempty() {
        assert!(!CircuitState::Closed.symbol().is_empty());
        assert!(!CircuitState::HalfOpen.symbol().is_empty());
        assert!(!CircuitState::Open.symbol().is_empty());
    }

    #[test]
    fn test_circuit_state_labels_nonempty() {
        assert!(!CircuitState::Closed.label().is_empty());
        assert!(!CircuitState::HalfOpen.label().is_empty());
        assert!(!CircuitState::Open.label().is_empty());
    }
}

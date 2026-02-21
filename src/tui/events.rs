//! # Module: TUI Event Handling
//!
//! ## Responsibility
//! Polls crossterm events and translates keyboard input into app state mutations.
//! Handles quit, pause, reset, and help overlay toggling.
//!
//! ## Guarantees
//! - Non-blocking event polling with configurable timeout
//! - No panics on any key combination
//! - Ctrl+C always triggers quit

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use super::app::App;

/// Result of polling for a terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// User pressed quit (q or Ctrl+C).
    Quit,
    /// User toggled pause.
    Pause,
    /// User requested stats reset.
    Reset,
    /// User toggled help overlay.
    Help,
    /// User pressed up arrow to scroll log.
    ScrollUp,
    /// User pressed down arrow to scroll log.
    ScrollDown,
    /// A terminal resize occurred.
    Resize(u16, u16),
    /// No actionable event within the poll window.
    None,
}

/// Polls for a single input event with the given timeout.
///
/// # Arguments
/// * `timeout` - Maximum time to wait for an event.
///
/// # Returns
/// The detected `InputEvent`, or `InputEvent::None` if no event occurred.
///
/// # Errors
/// Returns `InputEvent::None` on any crossterm polling error (never panics).
pub fn poll_event(timeout: Duration) -> InputEvent {
    let available = match event::poll(timeout) {
        Ok(v) => v,
        Err(_) => return InputEvent::None,
    };
    if !available {
        return InputEvent::None;
    }

    match event::read() {
        Ok(Event::Key(key)) => translate_key(key),
        Ok(Event::Resize(w, h)) => InputEvent::Resize(w, h),
        _ => InputEvent::None,
    }
}

/// Applies an input event to the app state.
///
/// # Arguments
/// * `app` - Mutable reference to app state.
/// * `event` - The input event to apply.
pub fn apply_event(app: &mut App, event: InputEvent) {
    match event {
        InputEvent::Quit => app.should_quit = true,
        InputEvent::Pause => app.paused = !app.paused,
        InputEvent::Reset => app.reset_stats(),
        InputEvent::Help => app.show_help = !app.show_help,
        InputEvent::ScrollUp => app.scroll_log_up(),
        InputEvent::ScrollDown => app.scroll_log_down(),
        InputEvent::Resize(_, _) | InputEvent::None => {}
    }
}

/// Translates a crossterm key event to an `InputEvent`.
fn translate_key(key: KeyEvent) -> InputEvent {
    // Ctrl+C always quits
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return InputEvent::Quit;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => InputEvent::Quit,
        KeyCode::Char('p') | KeyCode::Char('P') => InputEvent::Pause,
        KeyCode::Char('r') | KeyCode::Char('R') => InputEvent::Reset,
        KeyCode::Char('h') | KeyCode::Char('H') => InputEvent::Help,
        KeyCode::Esc => InputEvent::Quit,
        KeyCode::Up => InputEvent::ScrollUp,
        KeyCode::Down => InputEvent::ScrollDown,
        _ => InputEvent::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_key_q_quits() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Quit);
    }

    #[test]
    fn test_translate_key_uppercase_q_quits() {
        let key = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Quit);
    }

    #[test]
    fn test_translate_key_ctrl_c_quits() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(translate_key(key), InputEvent::Quit);
    }

    #[test]
    fn test_translate_key_p_pauses() {
        let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Pause);
    }

    #[test]
    fn test_translate_key_r_resets() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Reset);
    }

    #[test]
    fn test_translate_key_h_toggles_help() {
        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Help);
    }

    #[test]
    fn test_translate_key_esc_quits() {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::Quit);
    }

    #[test]
    fn test_translate_key_unknown_returns_none() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::None);
    }

    #[test]
    fn test_translate_key_up_scrolls_up() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::ScrollUp);
    }

    #[test]
    fn test_translate_key_down_scrolls_down() {
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(translate_key(key), InputEvent::ScrollDown);
    }

    #[test]
    fn test_apply_event_quit_sets_flag() {
        let mut app = App::new(Duration::from_secs(1));
        apply_event(&mut app, InputEvent::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn test_apply_event_pause_toggles() {
        let mut app = App::new(Duration::from_secs(1));
        assert!(!app.paused);
        apply_event(&mut app, InputEvent::Pause);
        assert!(app.paused);
        apply_event(&mut app, InputEvent::Pause);
        assert!(!app.paused);
    }

    #[test]
    fn test_apply_event_help_toggles() {
        let mut app = App::new(Duration::from_secs(1));
        assert!(!app.show_help);
        apply_event(&mut app, InputEvent::Help);
        assert!(app.show_help);
        apply_event(&mut app, InputEvent::Help);
        assert!(!app.show_help);
    }

    #[test]
    fn test_apply_event_reset_clears_stats() {
        let mut app = App::new(Duration::from_secs(1));
        app.requests_total = 500;
        app.inferences_total = 100;
        apply_event(&mut app, InputEvent::Reset);
        assert_eq!(app.requests_total, 0);
        assert_eq!(app.inferences_total, 0);
    }

    #[test]
    fn test_apply_event_none_is_noop() {
        let mut app = App::new(Duration::from_secs(1));
        apply_event(&mut app, InputEvent::None);
        assert!(!app.should_quit);
        assert!(!app.paused);
    }

    #[test]
    fn test_apply_event_resize_is_noop() {
        let mut app = App::new(Duration::from_secs(1));
        apply_event(&mut app, InputEvent::Resize(200, 60));
        assert!(!app.should_quit);
    }

    #[test]
    fn test_apply_event_scroll_up() {
        let mut app = App::new(Duration::from_secs(1));
        for i in 0..10 {
            app.push_log(crate::tui::app::LogEntry {
                timestamp: format!("{}", i),
                level: crate::tui::app::LogLevel::Info,
                message: format!("msg {}", i),
                fields: String::new(),
            });
        }
        apply_event(&mut app, InputEvent::ScrollUp);
        assert_eq!(app.log_scroll_offset, 1);
    }

    #[test]
    fn test_apply_event_scroll_down() {
        let mut app = App::new(Duration::from_secs(1));
        app.log_scroll_offset = 3;
        apply_event(&mut app, InputEvent::ScrollDown);
        assert_eq!(app.log_scroll_offset, 2);
    }

    #[test]
    fn test_apply_event_scroll_down_at_zero() {
        let mut app = App::new(Duration::from_secs(1));
        apply_event(&mut app, InputEvent::ScrollDown);
        assert_eq!(app.log_scroll_offset, 0);
    }
}

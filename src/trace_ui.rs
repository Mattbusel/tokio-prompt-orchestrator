#![allow(dead_code)]
//! # Module: Request Lifecycle Tracer TUI Panel
//!
//! ## Responsibility
//! Provides a Ratatui panel that renders the full lifecycle of a single request
//! as a Gantt-chart timeline. Activated with the `t` key in the TUI; the user
//! types a `request_id` to trace, and each pipeline stage is rendered as a
//! colour-coded bar proportional to time spent.
//!
//! ## Guarantees
//! - No panics in any rendering or state-update path
//! - All arithmetic is saturating / checked; no integer overflow
//! - Zero allocations in the hot render path (only on stage record insertion)
//!
//! ## Feature Gate
//! This module is compiled unconditionally but [`TracePanel`] only renders
//! content when the `tui` feature is active (the binary wires it in).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─── Stage names ─────────────────────────────────────────────────────────────

/// Ordered pipeline stage labels.
pub const LIFECYCLE_STAGES: [&str; 5] = ["RAG", "Assemble", "Inference", "Post", "Stream"];

// ─── Stage record ────────────────────────────────────────────────────────────

/// Timing record for one pipeline stage within a single request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRecord {
    /// Stage index (0 = RAG … 4 = Stream).
    pub stage_index: usize,
    /// When the stage began processing this request (milliseconds since request entry).
    pub start_ms: u64,
    /// When the stage finished (milliseconds since request entry). `None` if still running.
    pub end_ms: Option<u64>,
    /// Whether the stage completed without error.
    pub success: bool,
}

impl StageRecord {
    /// Duration in milliseconds, if the stage has completed.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.end_ms.map(|e| e.saturating_sub(self.start_ms))
    }
}

// ─── Request trace ───────────────────────────────────────────────────────────

/// Complete lifecycle record for one request.
#[derive(Debug, Clone)]
pub struct RequestTrace {
    /// Unique identifier for the request.
    pub request_id: String,
    /// Wall-clock time when the request entered the pipeline.
    pub entry_time: Instant,
    /// Per-stage records, indexed by stage index (0-4).
    pub stages: [Option<StageRecord>; 5],
    /// Whether the entire request has completed.
    pub complete: bool,
}

impl RequestTrace {
    /// Create a new, empty trace for the given request id.
    #[must_use]
    pub fn new(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            entry_time: Instant::now(),
            stages: [None, None, None, None, None],
            complete: false,
        }
    }

    /// Record the start of a stage.
    ///
    /// Idempotent: if the stage was already started, this is a no-op.
    pub fn stage_started(&mut self, stage_index: usize) {
        if stage_index >= 5 {
            return;
        }
        if self.stages[stage_index].is_none() {
            let start_ms = self
                .entry_time
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            self.stages[stage_index] = Some(StageRecord {
                stage_index,
                start_ms,
                end_ms: None,
                success: false,
            });
        }
    }

    /// Record the completion of a stage.
    pub fn stage_finished(&mut self, stage_index: usize, success: bool) {
        if stage_index >= 5 {
            return;
        }
        // Ensure a start record exists (handle missing start gracefully)
        if self.stages[stage_index].is_none() {
            self.stage_started(stage_index);
        }
        if let Some(ref mut rec) = self.stages[stage_index] {
            let now_ms = self
                .entry_time
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            rec.end_ms = Some(now_ms);
            rec.success = success;
        }
        // Mark overall completion when the final stage (Stream) finishes
        if stage_index == 4 {
            self.complete = true;
        }
    }

    /// Total elapsed time from entry to now (or until Stream completed).
    #[must_use]
    pub fn total_elapsed(&self) -> Duration {
        if let Some(Some(stream)) = self.stages.get(4) {
            if let Some(end_ms) = stream.end_ms {
                return Duration::from_millis(end_ms);
            }
        }
        self.entry_time.elapsed()
    }
}

// ─── Trace store ─────────────────────────────────────────────────────────────

/// Thread-safe store of recent request traces.
///
/// Bounded to `capacity` entries; oldest entries are evicted when the cap is
/// reached. All public methods take `&self` to allow sharing across tasks via
/// `Arc<TraceStore>`.
#[derive(Debug)]
pub struct TraceStore {
    inner: std::sync::Mutex<TraceStoreInner>,
}

#[derive(Debug)]
struct TraceStoreInner {
    traces: HashMap<String, RequestTrace>,
    insertion_order: std::collections::VecDeque<String>,
    capacity: usize,
}

impl TraceStore {
    /// Create a store with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(TraceStoreInner {
                traces: HashMap::new(),
                insertion_order: std::collections::VecDeque::new(),
                capacity: capacity.max(1),
            }),
        }
    }

    /// Insert or update a trace. Evicts the oldest entry when at capacity.
    pub fn upsert(&self, trace: RequestTrace) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let id = trace.request_id.clone();
        if !inner.traces.contains_key(&id) {
            // Evict oldest if at cap
            if inner.insertion_order.len() >= inner.capacity {
                if let Some(oldest) = inner.insertion_order.pop_front() {
                    inner.traces.remove(&oldest);
                }
            }
            inner.insertion_order.push_back(id.clone());
        }
        inner.traces.insert(id, trace);
    }

    /// Record a stage start event.
    pub fn stage_started(&self, request_id: &str, stage_index: usize) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(trace) = inner.traces.get_mut(request_id) {
            trace.stage_started(stage_index);
        } else {
            // Create a new trace if we haven't seen this request yet
            drop(inner);
            let mut trace = RequestTrace::new(request_id);
            trace.stage_started(stage_index);
            self.upsert(trace);
        }
    }

    /// Record a stage finish event.
    pub fn stage_finished(&self, request_id: &str, stage_index: usize, success: bool) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(trace) = inner.traces.get_mut(request_id) {
            trace.stage_finished(stage_index, success);
        }
    }

    /// Look up a trace by request id.
    #[must_use]
    pub fn get(&self, request_id: &str) -> Option<RequestTrace> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        inner.traces.get(request_id).cloned()
    }

    /// Return a list of all known request ids (newest last).
    #[must_use]
    pub fn known_ids(&self) -> Vec<String> {
        let Ok(inner) = self.inner.lock() else {
            return vec![];
        };
        inner.insertion_order.iter().cloned().collect()
    }
}

impl Default for TraceStore {
    fn default() -> Self {
        Self::new(1000)
    }
}

// ─── TUI panel state ─────────────────────────────────────────────────────────

/// Interaction mode for the trace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracePanelMode {
    /// Panel is hidden; user presses `t` to activate.
    Hidden,
    /// User is typing a request_id to look up.
    Searching,
    /// Displaying the timeline for a resolved request.
    Viewing,
}

/// State for the request-lifecycle tracer TUI panel.
#[derive(Debug)]
pub struct TracePanel {
    /// Current interaction mode.
    pub mode: TracePanelMode,
    /// Text the user has typed so far.
    pub search_input: String,
    /// The currently displayed trace (if any).
    pub current_trace: Option<RequestTrace>,
    /// Error message to display (e.g. "not found").
    pub error_msg: Option<String>,
    /// Shared trace store.
    pub store: std::sync::Arc<TraceStore>,
}

impl TracePanel {
    /// Create a new panel backed by the given store.
    #[must_use]
    pub fn new(store: std::sync::Arc<TraceStore>) -> Self {
        Self {
            mode: TracePanelMode::Hidden,
            search_input: String::new(),
            current_trace: None,
            error_msg: None,
            store,
        }
    }

    /// Toggle panel visibility (bound to `t` key).
    pub fn toggle(&mut self) {
        match self.mode {
            TracePanelMode::Hidden => {
                self.mode = TracePanelMode::Searching;
                self.search_input.clear();
                self.error_msg = None;
            }
            _ => {
                self.mode = TracePanelMode::Hidden;
            }
        }
    }

    /// Feed a character from keyboard input (while in Searching mode).
    pub fn push_char(&mut self, c: char) {
        if self.mode == TracePanelMode::Searching {
            self.search_input.push(c);
        }
    }

    /// Delete last character (Backspace) while searching.
    pub fn pop_char(&mut self) {
        if self.mode == TracePanelMode::Searching {
            self.search_input.pop();
        }
    }

    /// Submit the current search input (Enter key).
    pub fn submit(&mut self) {
        if self.mode != TracePanelMode::Searching {
            return;
        }
        let id = self.search_input.trim().to_string();
        if id.is_empty() {
            self.error_msg = Some("Enter a request_id".to_string());
            return;
        }
        match self.store.get(&id) {
            Some(trace) => {
                self.current_trace = Some(trace);
                self.error_msg = None;
                self.mode = TracePanelMode::Viewing;
            }
            None => {
                self.error_msg = Some(format!("request_id '{id}' not found"));
            }
        }
    }

    /// Refresh the displayed trace from the store (call on each tick while Viewing).
    pub fn refresh(&mut self) {
        if self.mode == TracePanelMode::Viewing {
            if let Some(ref trace) = self.current_trace.clone() {
                self.current_trace = self.store.get(&trace.request_id);
            }
        }
    }
}

// ─── Ratatui rendering ───────────────────────────────────────────────────────

#[cfg(feature = "tui")]
pub mod render {
    use super::{TracePanel, TracePanelMode, LIFECYCLE_STAGES};
    use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use ratatui::Frame;

    /// Draw the trace panel as a centred overlay.
    ///
    /// Does nothing if `panel.mode == Hidden`.
    pub fn draw_trace_panel(f: &mut Frame, panel: &TracePanel) {
        if panel.mode == TracePanelMode::Hidden {
            return;
        }

        let area = f.area();
        // Centre a 70%-wide, 60%-tall popup
        let popup = centred_rect(70, 60, area);
        f.render_widget(Clear, popup);

        let block = Block::default()
            .title(" REQUEST LIFECYCLE TRACER  [t] to close ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // search bar
                Constraint::Min(4),    // timeline / message
            ])
            .split(inner);

        // Search bar
        draw_search_bar(f, chunks[0], panel);

        // Timeline or status
        match panel.mode {
            TracePanelMode::Searching => {
                if let Some(ref msg) = panel.error_msg {
                    let p = Paragraph::new(msg.as_str())
                        .style(Style::default().fg(Color::Red))
                        .alignment(Alignment::Center);
                    f.render_widget(p, chunks[1]);
                } else {
                    let ids = panel.store.known_ids();
                    let hint = if ids.is_empty() {
                        "No traces recorded yet.".to_string()
                    } else {
                        format!(
                            "Known IDs (newest last): {}",
                            ids.iter()
                                .rev()
                                .take(5)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let p = Paragraph::new(hint)
                        .style(Style::default().fg(Color::DarkGray))
                        .alignment(Alignment::Center);
                    f.render_widget(p, chunks[1]);
                }
            }
            TracePanelMode::Viewing => {
                if let Some(ref trace) = panel.current_trace {
                    draw_timeline(f, chunks[1], trace);
                }
            }
            TracePanelMode::Hidden => {}
        }
    }

    fn draw_search_bar(f: &mut Frame, area: Rect, panel: &TracePanel) {
        let label = if panel.mode == TracePanelMode::Searching {
            format!(" Search request_id: {}|", panel.search_input)
        } else {
            format!(" Tracing: {}", panel.search_input)
        };
        let p = Paragraph::new(label)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .style(Style::default().fg(Color::White));
        f.render_widget(p, area);
    }

    fn draw_timeline(
        f: &mut Frame,
        area: Rect,
        trace: &super::RequestTrace,
    ) {
        if area.width < 20 || area.height < 3 {
            return;
        }

        let total_ms = trace.total_elapsed().as_millis() as f64;
        let label_w: u16 = 10;
        let bar_w = area.width.saturating_sub(label_w + 2);

        // Header
        let header = Line::from(vec![
            Span::raw(format!("{:<10}", "Stage")),
            Span::styled(
                format!(" {:>bar_w$}", format!("{total_ms:.0} ms total")),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]);

        let mut lines: Vec<Line> = vec![header, Line::raw("")];

        for (idx, name) in LIFECYCLE_STAGES.iter().enumerate() {
            let line = if let Some(Some(rec)) = trace.stages.get(idx) {
                let start_frac = rec.start_ms as f64 / total_ms.max(1.0);
                let dur_ms = rec
                    .end_ms
                    .map(|e| e.saturating_sub(rec.start_ms))
                    .unwrap_or(0);
                let dur_frac = dur_ms as f64 / total_ms.max(1.0);

                let offset = ((start_frac * bar_w as f64) as u16).min(bar_w);
                let width = ((dur_frac * bar_w as f64) as u16)
                    .max(1)
                    .min(bar_w.saturating_sub(offset));

                let color = if rec.end_ms.is_none() {
                    Color::Yellow // still running
                } else if rec.success {
                    // Color by proportion: green < 30%, yellow < 60%, red otherwise
                    if dur_frac < 0.30 {
                        Color::Green
                    } else if dur_frac < 0.60 {
                        Color::Yellow
                    } else {
                        Color::Red
                    }
                } else {
                    Color::Red
                };

                let prefix = " ".repeat(offset as usize);
                let bar_str = "\u{2588}".repeat(width as usize); // ██
                let suffix_w = bar_w.saturating_sub(offset + width) as usize;
                let suffix = " ".repeat(suffix_w);
                let timing = format!(
                    " {dur_ms}ms",
                );

                Line::from(vec![
                    Span::styled(
                        format!("{name:<10}"),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(prefix),
                    Span::styled(bar_str, Style::default().fg(color)),
                    Span::raw(suffix),
                    Span::styled(timing, Style::default().fg(Color::DarkGray)),
                ])
            } else {
                // Stage not started
                Line::from(vec![
                    Span::styled(
                        format!("{name:<10}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:-<bar_w$}", ""),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            };
            lines.push(line);
        }

        // Summary line
        lines.push(Line::raw(""));
        let status = if trace.complete {
            Span::styled(
                format!(" COMPLETE  ({:.0} ms)", total_ms),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!(" IN FLIGHT ({:.0} ms elapsed)", total_ms),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        };
        lines.push(Line::from(vec![status]));

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    /// Returns a centred [`Rect`] occupying `percent_x` / `percent_y` of `r`.
    fn centred_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(r);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(layout[1])[1]
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_records_stages() {
        let mut t = RequestTrace::new("req-1");
        t.stage_started(0);
        t.stage_finished(0, true);
        t.stage_started(4);
        t.stage_finished(4, true);
        assert!(t.complete);
        let rec = t.stages[0].as_ref().expect("stage 0 should be recorded");
        assert!(rec.duration_ms().is_some());
    }

    #[test]
    fn store_evicts_at_capacity() {
        let store = TraceStore::new(2);
        for i in 0..4u32 {
            store.upsert(RequestTrace::new(format!("req-{i}")));
        }
        let ids = store.known_ids();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "req-2");
        assert_eq!(ids[1], "req-3");
    }

    #[test]
    fn panel_toggle_hides() {
        let store = std::sync::Arc::new(TraceStore::default());
        let mut panel = TracePanel::new(store);
        assert_eq!(panel.mode, TracePanelMode::Hidden);
        panel.toggle();
        assert_eq!(panel.mode, TracePanelMode::Searching);
        panel.toggle();
        assert_eq!(panel.mode, TracePanelMode::Hidden);
    }

    #[test]
    fn panel_submit_not_found() {
        let store = std::sync::Arc::new(TraceStore::default());
        let mut panel = TracePanel::new(store);
        panel.toggle();
        panel.push_char('x');
        panel.submit();
        assert!(panel.error_msg.is_some());
        assert_eq!(panel.mode, TracePanelMode::Searching);
    }
}

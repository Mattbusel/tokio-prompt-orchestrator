//! # Conversation State Machine
//!
//! Tracks the lifecycle phase of a conversation via a finite-state machine.
//! Phases transition based on [`TransitionTrigger`] events detected from user
//! text or explicit external signals (timeout, close).

use std::collections::HashMap;

// ── ConversationPhase ─────────────────────────────────────────────────────────

/// High-level lifecycle phase of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationPhase {
    /// Conversation has just started; waiting for first meaningful input.
    Opening,
    /// User mentioned a task; gathering requirements.
    TaskDiscovery,
    /// Actively working on the user's task.
    ActiveTask,
    /// A clarifying question was asked; waiting for user reply.
    Clarification,
    /// Generating a summary / wrap-up response.
    Summarizing,
    /// Conversation ended gracefully.
    Closing,
    /// Conversation ended due to inactivity or abrupt user departure.
    Abandoned,
}

impl ConversationPhase {
    /// Returns `true` for phases from which no further transitions occur.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closing | Self::Abandoned)
    }
}

// ── TransitionTrigger ─────────────────────────────────────────────────────────

/// Events that may cause a phase transition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransitionTrigger {
    /// User sent a greeting ("hello", "hi", …).
    UserGreeting,
    /// User mentioned a task or goal.
    TaskMentioned,
    /// User asked a question (heuristic: contains "?").
    QuestionAsked,
    /// The active task was completed successfully.
    TaskCompleted,
    /// User expressed satisfaction ("thank you", "thanks", …).
    UserSatisfied,
    /// An inactivity / session timeout fired.
    TimeoutExpired,
    /// User left or connection dropped.
    UserLeft,
    /// Explicit close signal from the system or user ("bye", "close", …).
    ExplicitClose,
}

// ── PhaseTransition ───────────────────────────────────────────────────────────

/// A recorded phase change with its cause and wall-clock timestamp.
#[derive(Debug, Clone)]
pub struct PhaseTransition {
    /// Phase before the transition.
    pub from: ConversationPhase,
    /// Phase after the transition.
    pub to: ConversationPhase,
    /// What triggered this transition.
    pub trigger: TransitionTrigger,
    /// Unix-epoch milliseconds when the transition occurred.
    pub timestamp: u64,
}

// ── ConversationStateMachine ──────────────────────────────────────────────────

/// Finite-state machine that advances a conversation through its lifecycle.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::conversation_state::{
///     ConversationStateMachine, TransitionTrigger,
/// };
///
/// let mut sm = ConversationStateMachine::new();
/// let new_phase = sm.trigger(TransitionTrigger::TaskMentioned, 1_000);
/// assert!(new_phase.is_some());
/// ```
pub struct ConversationStateMachine {
    phase: ConversationPhase,
    phase_entered_at: u64,
    history: Vec<PhaseTransition>,
    /// (from, trigger) -> to  —  the rule table.
    rules: HashMap<(ConversationPhase, TransitionTrigger), ConversationPhase>,
}

impl ConversationStateMachine {
    /// Create a new state machine starting in [`ConversationPhase::Opening`].
    pub fn new() -> Self {
        let rules = Self::build_rules();
        Self {
            phase: ConversationPhase::Opening,
            phase_entered_at: 0,
            history: Vec::new(),
            rules,
        }
    }

    /// Build the static rule table: (from, trigger) → to.
    fn build_rules() -> HashMap<(ConversationPhase, TransitionTrigger), ConversationPhase> {
        let mut r: HashMap<(ConversationPhase, TransitionTrigger), ConversationPhase> =
            HashMap::new();

        // Opening transitions
        r.insert(
            (ConversationPhase::Opening, TransitionTrigger::UserGreeting),
            ConversationPhase::Opening, // greeting stays in opening
        );
        r.insert(
            (ConversationPhase::Opening, TransitionTrigger::TaskMentioned),
            ConversationPhase::TaskDiscovery,
        );
        r.insert(
            (ConversationPhase::Opening, TransitionTrigger::TimeoutExpired),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::Opening, TransitionTrigger::UserLeft),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::Opening, TransitionTrigger::ExplicitClose),
            ConversationPhase::Closing,
        );

        // TaskDiscovery transitions
        r.insert(
            (ConversationPhase::TaskDiscovery, TransitionTrigger::QuestionAsked),
            ConversationPhase::Clarification,
        );
        r.insert(
            (ConversationPhase::TaskDiscovery, TransitionTrigger::TaskMentioned),
            ConversationPhase::ActiveTask,
        );
        r.insert(
            (ConversationPhase::TaskDiscovery, TransitionTrigger::TimeoutExpired),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::TaskDiscovery, TransitionTrigger::UserLeft),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::TaskDiscovery, TransitionTrigger::ExplicitClose),
            ConversationPhase::Closing,
        );

        // Clarification transitions
        r.insert(
            (ConversationPhase::Clarification, TransitionTrigger::TaskMentioned),
            ConversationPhase::ActiveTask,
        );
        r.insert(
            (ConversationPhase::Clarification, TransitionTrigger::QuestionAsked),
            ConversationPhase::Clarification, // nested clarification
        );
        r.insert(
            (ConversationPhase::Clarification, TransitionTrigger::TimeoutExpired),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::Clarification, TransitionTrigger::UserLeft),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::Clarification, TransitionTrigger::ExplicitClose),
            ConversationPhase::Closing,
        );

        // ActiveTask transitions
        r.insert(
            (ConversationPhase::ActiveTask, TransitionTrigger::TaskCompleted),
            ConversationPhase::Summarizing,
        );
        r.insert(
            (ConversationPhase::ActiveTask, TransitionTrigger::QuestionAsked),
            ConversationPhase::Clarification,
        );
        r.insert(
            (ConversationPhase::ActiveTask, TransitionTrigger::TimeoutExpired),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::ActiveTask, TransitionTrigger::UserLeft),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::ActiveTask, TransitionTrigger::ExplicitClose),
            ConversationPhase::Closing,
        );

        // Summarizing transitions
        r.insert(
            (ConversationPhase::Summarizing, TransitionTrigger::UserSatisfied),
            ConversationPhase::Closing,
        );
        r.insert(
            (ConversationPhase::Summarizing, TransitionTrigger::TaskMentioned),
            ConversationPhase::TaskDiscovery,
        );
        r.insert(
            (ConversationPhase::Summarizing, TransitionTrigger::ExplicitClose),
            ConversationPhase::Closing,
        );
        r.insert(
            (ConversationPhase::Summarizing, TransitionTrigger::TimeoutExpired),
            ConversationPhase::Abandoned,
        );
        r.insert(
            (ConversationPhase::Summarizing, TransitionTrigger::UserLeft),
            ConversationPhase::Abandoned,
        );

        r
    }

    /// Return the current phase.
    pub fn current_phase(&self) -> &ConversationPhase {
        &self.phase
    }

    /// Apply a trigger.  Returns the new phase if a transition occurred, or
    /// `None` if the machine is in a terminal state or no rule matched.
    pub fn trigger(
        &mut self,
        trigger: TransitionTrigger,
        now: u64,
    ) -> Option<ConversationPhase> {
        if self.phase.is_terminal() {
            return None;
        }
        let key = (self.phase.clone(), trigger.clone());
        let next = self.rules.get(&key)?.clone();

        // Record the transition (even self-loops, for history completeness).
        self.history.push(PhaseTransition {
            from: self.phase.clone(),
            to: next.clone(),
            trigger,
            timestamp: now,
        });

        self.phase = next.clone();
        self.phase_entered_at = now;
        Some(next)
    }

    /// Return `true` if the given trigger can fire from `from`.
    pub fn can_transition(
        from: &ConversationPhase,
        trigger: &TransitionTrigger,
    ) -> bool {
        // Build a fresh instance to check without mutating self.
        let rules = Self::build_rules();
        rules.contains_key(&(from.clone(), trigger.clone()))
    }

    /// Return the full transition history.
    pub fn history(&self) -> &[PhaseTransition] {
        &self.history
    }

    /// Milliseconds spent in the current phase as of `now`.
    pub fn time_in_current_phase(&self, now: u64) -> u64 {
        now.saturating_sub(self.phase_entered_at)
    }

    /// Heuristic trigger detection from raw user text.
    ///
    /// Rules (checked in order):
    /// - "bye" / "goodbye" / "close" → [`TransitionTrigger::ExplicitClose`]
    /// - "thank" / "thanks" → [`TransitionTrigger::UserSatisfied`]
    /// - "hello" / "hi" / "hey" → [`TransitionTrigger::UserGreeting`]
    /// - text contains "?" → [`TransitionTrigger::QuestionAsked`]
    /// - any other non-empty text → [`TransitionTrigger::TaskMentioned`]
    pub fn detect_trigger(user_text: &str) -> Option<TransitionTrigger> {
        let lower = user_text.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("bye") || lower.contains("goodbye") || lower.contains("close") {
            return Some(TransitionTrigger::ExplicitClose);
        }
        if lower.contains("thank") {
            return Some(TransitionTrigger::UserSatisfied);
        }
        if lower.contains("hello") || lower.contains("hi ") || lower.starts_with("hi")
            || lower.contains("hey")
        {
            return Some(TransitionTrigger::UserGreeting);
        }
        if lower.contains('?') {
            return Some(TransitionTrigger::QuestionAsked);
        }
        Some(TransitionTrigger::TaskMentioned)
    }
}

impl Default for ConversationStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_phase_is_opening() {
        let sm = ConversationStateMachine::new();
        assert_eq!(sm.current_phase(), &ConversationPhase::Opening);
    }

    #[test]
    fn test_terminal_phases() {
        assert!(ConversationPhase::Closing.is_terminal());
        assert!(ConversationPhase::Abandoned.is_terminal());
        assert!(!ConversationPhase::Opening.is_terminal());
        assert!(!ConversationPhase::ActiveTask.is_terminal());
    }

    #[test]
    fn test_greeting_stays_in_opening() {
        let mut sm = ConversationStateMachine::new();
        let result = sm.trigger(TransitionTrigger::UserGreeting, 100);
        assert!(result.is_some());
        assert_eq!(sm.current_phase(), &ConversationPhase::Opening);
    }

    #[test]
    fn test_task_mentioned_from_opening_goes_to_discovery() {
        let mut sm = ConversationStateMachine::new();
        let result = sm.trigger(TransitionTrigger::TaskMentioned, 200);
        assert_eq!(result, Some(ConversationPhase::TaskDiscovery));
        assert_eq!(sm.current_phase(), &ConversationPhase::TaskDiscovery);
    }

    #[test]
    fn test_clarification_from_discovery() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::TaskMentioned, 100);
        let result = sm.trigger(TransitionTrigger::QuestionAsked, 200);
        assert_eq!(result, Some(ConversationPhase::Clarification));
    }

    #[test]
    fn test_active_task_to_summarizing() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::TaskMentioned, 100); // -> TaskDiscovery
        sm.trigger(TransitionTrigger::TaskMentioned, 200); // -> ActiveTask
        let result = sm.trigger(TransitionTrigger::TaskCompleted, 300);
        assert_eq!(result, Some(ConversationPhase::Summarizing));
    }

    #[test]
    fn test_satisfied_in_summarizing_goes_to_closing() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::TaskMentioned, 100);
        sm.trigger(TransitionTrigger::TaskMentioned, 200);
        sm.trigger(TransitionTrigger::TaskCompleted, 300);
        let result = sm.trigger(TransitionTrigger::UserSatisfied, 400);
        assert_eq!(result, Some(ConversationPhase::Closing));
        assert!(sm.current_phase().is_terminal());
    }

    #[test]
    fn test_no_transition_from_terminal() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::ExplicitClose, 100);
        assert_eq!(sm.current_phase(), &ConversationPhase::Closing);
        let result = sm.trigger(TransitionTrigger::TaskMentioned, 200);
        assert!(result.is_none());
    }

    #[test]
    fn test_timeout_leads_to_abandoned() {
        let mut sm = ConversationStateMachine::new();
        let result = sm.trigger(TransitionTrigger::TimeoutExpired, 500);
        assert_eq!(result, Some(ConversationPhase::Abandoned));
        assert!(sm.current_phase().is_terminal());
    }

    #[test]
    fn test_history_recorded() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::TaskMentioned, 100);
        sm.trigger(TransitionTrigger::QuestionAsked, 200);
        assert_eq!(sm.history().len(), 2);
        assert_eq!(sm.history()[0].trigger, TransitionTrigger::TaskMentioned);
        assert_eq!(sm.history()[1].trigger, TransitionTrigger::QuestionAsked);
    }

    #[test]
    fn test_time_in_current_phase() {
        let mut sm = ConversationStateMachine::new();
        sm.trigger(TransitionTrigger::TaskMentioned, 1000);
        assert_eq!(sm.time_in_current_phase(1500), 500);
    }

    #[test]
    fn test_can_transition() {
        assert!(ConversationStateMachine::can_transition(
            &ConversationPhase::Opening,
            &TransitionTrigger::TaskMentioned
        ));
        assert!(!ConversationStateMachine::can_transition(
            &ConversationPhase::Closing,
            &TransitionTrigger::TaskMentioned
        ));
    }

    #[test]
    fn test_detect_trigger_greeting() {
        assert_eq!(
            ConversationStateMachine::detect_trigger("Hello there!"),
            Some(TransitionTrigger::UserGreeting)
        );
    }

    #[test]
    fn test_detect_trigger_question() {
        assert_eq!(
            ConversationStateMachine::detect_trigger("Can you help me?"),
            Some(TransitionTrigger::QuestionAsked)
        );
    }

    #[test]
    fn test_detect_trigger_satisfied() {
        assert_eq!(
            ConversationStateMachine::detect_trigger("Thank you so much!"),
            Some(TransitionTrigger::UserSatisfied)
        );
    }

    #[test]
    fn test_detect_trigger_close() {
        assert_eq!(
            ConversationStateMachine::detect_trigger("Goodbye!"),
            Some(TransitionTrigger::ExplicitClose)
        );
    }

    #[test]
    fn test_detect_trigger_task() {
        assert_eq!(
            ConversationStateMachine::detect_trigger("I need to process 1000 CSV files."),
            Some(TransitionTrigger::TaskMentioned)
        );
    }

    #[test]
    fn test_detect_trigger_empty() {
        assert_eq!(ConversationStateMachine::detect_trigger(""), None);
    }
}

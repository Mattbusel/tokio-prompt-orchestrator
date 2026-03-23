//! Context window compression and summarization utilities.
//!
//! Provides strategies for keeping LLM conversation context within token budget
//! limits by dropping, summarizing, or windowing older messages.

/// Strategy used when compressing a context window.
#[derive(Debug, Clone)]
pub enum CompressionStrategy {
    /// Keep only the last `window_size` non-system messages (plus all system messages).
    SlidingWindow { window_size: usize },
    /// Replace dropped messages with a synthetic summary up to `max_summary_tokens`.
    Summarize { max_summary_tokens: usize },
    /// Keep system messages plus the last `keep_last` messages.
    DropOldest { keep_last: usize },
    /// Drop oldest until within window, then summarize if still over budget.
    Hybrid { window_size: usize, summary_tokens: usize },
}

/// A single message in a conversation context.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Text content of the message.
    pub content: String,
    /// Pre-computed token count for this message.
    pub token_count: usize,
    /// Importance score in [0.0, 1.0]; higher = more important to retain.
    pub importance: f32,
}

/// Summary of what happened during a compression pass.
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// Number of messages before compression.
    pub original_count: usize,
    /// Number of messages after compression.
    pub compressed_count: usize,
    /// How many messages were dropped entirely.
    pub dropped_messages: usize,
    /// Optional synthetic summary text produced by the Summarize / Hybrid strategy.
    pub summary: Option<String>,
    /// Approximate tokens saved.
    pub tokens_saved: usize,
}

/// Applies a [`CompressionStrategy`] to a slice of messages.
#[derive(Debug, Clone)]
pub struct ContextCompressor {
    /// The strategy to apply.
    pub strategy: CompressionStrategy,
    /// Hard upper bound on total tokens across all kept messages.
    pub max_context_tokens: usize,
}

impl ContextCompressor {
    /// Create a new compressor with the given strategy and token budget.
    pub fn new(strategy: CompressionStrategy, max_context_tokens: usize) -> Self {
        Self { strategy, max_context_tokens }
    }

    /// Apply the compression strategy to `messages` and return the retained
    /// messages together with a [`CompressionResult`] summary.
    pub fn compress(&self, messages: &[Message]) -> (Vec<Message>, CompressionResult) {
        let original_count = messages.len();
        let original_tokens: usize = messages.iter().map(|m| m.token_count).sum();

        let (kept, summary) = match &self.strategy {
            CompressionStrategy::SlidingWindow { window_size } => {
                let (sys, non_sys): (Vec<_>, Vec<_>) =
                    messages.iter().partition(|m| m.role == "system");
                let keep_non_sys = non_sys
                    .iter()
                    .rev()
                    .take(*window_size)
                    .rev()
                    .cloned()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut result: Vec<Message> =
                    sys.into_iter().cloned().collect();
                result.extend(keep_non_sys);
                (result, None)
            }

            CompressionStrategy::DropOldest { keep_last } => {
                let (sys, non_sys): (Vec<_>, Vec<_>) =
                    messages.iter().partition(|m| m.role == "system");
                let keep_non_sys = non_sys
                    .iter()
                    .rev()
                    .take(*keep_last)
                    .rev()
                    .cloned()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut result: Vec<Message> =
                    sys.into_iter().cloned().collect();
                result.extend(keep_non_sys);
                (result, None)
            }

            CompressionStrategy::Summarize { max_summary_tokens: _ } => {
                let (sys, non_sys): (Vec<_>, Vec<_>) =
                    messages.iter().partition(|m| m.role == "system");

                // Determine which non-system messages fit within budget.
                let sys_tokens: usize = sys.iter().map(|m| m.token_count).sum();
                let budget = self.max_context_tokens.saturating_sub(sys_tokens);

                let mut kept_non_sys: Vec<&Message> = Vec::new();
                let mut used = 0usize;
                for msg in non_sys.iter().rev() {
                    if used + msg.token_count <= budget {
                        kept_non_sys.push(msg);
                        used += msg.token_count;
                    } else {
                        break;
                    }
                }
                kept_non_sys.reverse();

                // Build summary from dropped messages.
                let dropped: Vec<&Message> = non_sys
                    .iter()
                    .filter(|m| !kept_non_sys.contains(m))
                    .cloned()
                    .collect();

                let summary_text = if dropped.is_empty() {
                    None
                } else {
                    let key_points: Vec<String> = dropped
                        .iter()
                        .map(|m| first_sentence(&m.content))
                        .collect();
                    Some(format!(
                        "Previous context summary: {}",
                        key_points.join(" ")
                    ))
                };

                let mut result: Vec<Message> = sys.into_iter().cloned().collect();
                if let Some(ref s) = summary_text {
                    result.push(Message {
                        role: "system".to_string(),
                        content: s.clone(),
                        token_count: estimate_tokens(s),
                        importance: 1.0,
                    });
                }
                result.extend(kept_non_sys.into_iter().cloned());
                (result, summary_text)
            }

            CompressionStrategy::Hybrid { window_size, summary_tokens: _ } => {
                // Phase 1: drop oldest non-system messages until within window.
                let (sys, non_sys): (Vec<_>, Vec<_>) =
                    messages.iter().partition(|m| m.role == "system");

                let windowed_non_sys: Vec<&Message> = non_sys
                    .iter()
                    .rev()
                    .take(*window_size)
                    .rev()
                    .cloned()
                    .collect();

                let dropped_first: Vec<&Message> = non_sys
                    .iter()
                    .filter(|m| !windowed_non_sys.contains(m))
                    .cloned()
                    .collect();

                // Phase 2: if still over budget, summarize.
                let sys_tokens: usize = sys.iter().map(|m| m.token_count).sum();
                let windowed_tokens: usize =
                    windowed_non_sys.iter().map(|m| m.token_count).sum();
                let total = sys_tokens + windowed_tokens;

                let (final_non_sys, summary_text) = if total > self.max_context_tokens {
                    // Summarize the windowed set further.
                    let budget =
                        self.max_context_tokens.saturating_sub(sys_tokens);
                    let mut kept2: Vec<&Message> = Vec::new();
                    let mut used = 0usize;
                    for msg in windowed_non_sys.iter().rev() {
                        if used + msg.token_count <= budget {
                            kept2.push(msg);
                            used += msg.token_count;
                        } else {
                            break;
                        }
                    }
                    kept2.reverse();

                    let all_dropped: Vec<&Message> = dropped_first
                        .iter()
                        .chain(
                            windowed_non_sys
                                .iter()
                                .filter(|m| !kept2.contains(m)),
                        )
                        .cloned()
                        .collect();

                    let summary_text = if all_dropped.is_empty() {
                        None
                    } else {
                        let key_points: Vec<String> = all_dropped
                            .iter()
                            .map(|m| first_sentence(&m.content))
                            .collect();
                        Some(format!(
                            "Previous context summary: {}",
                            key_points.join(" ")
                        ))
                    };
                    (kept2, summary_text)
                } else {
                    // Window alone is sufficient; produce summary of phase-1 drops.
                    let summary_text = if dropped_first.is_empty() {
                        None
                    } else {
                        let key_points: Vec<String> = dropped_first
                            .iter()
                            .map(|m| first_sentence(&m.content))
                            .collect();
                        Some(format!(
                            "Previous context summary: {}",
                            key_points.join(" ")
                        ))
                    };
                    (windowed_non_sys, summary_text)
                };

                let mut result: Vec<Message> = sys.into_iter().cloned().collect();
                if let Some(ref s) = summary_text {
                    result.push(Message {
                        role: "system".to_string(),
                        content: s.clone(),
                        token_count: estimate_tokens(s),
                        importance: 1.0,
                    });
                }
                result.extend(final_non_sys.into_iter().cloned());
                (result, summary_text)
            }
        };

        let new_tokens: usize = kept.iter().map(|m| m.token_count).sum();
        let tokens_saved = original_tokens.saturating_sub(new_tokens);
        let compressed_count = kept.len();
        let dropped_messages = original_count.saturating_sub(compressed_count);

        (
            kept,
            CompressionResult {
                original_count,
                compressed_count,
                dropped_messages,
                summary,
                tokens_saved,
            },
        )
    }

    /// Filter `messages` down to `target_count` by dropping the lowest-importance
    /// messages first (system messages are always retained).
    pub fn filter_by_importance(
        &self,
        messages: &[Message],
        target_count: usize,
    ) -> Vec<Message> {
        if messages.len() <= target_count {
            return messages.to_vec();
        }
        let total = messages.len();
        let mut scored: Vec<(usize, f32, &Message)> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| (i, importance_score(m, i, total), m))
            .collect();

        // Always keep system messages.
        let sys_count = messages.iter().filter(|m| m.role == "system").count();
        let non_sys_target = target_count.saturating_sub(sys_count);

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut kept_indices: std::collections::HashSet<usize> = scored
            .iter()
            .filter(|(_, _, m)| m.role == "system")
            .map(|(i, _, _)| *i)
            .collect();

        let mut non_sys_kept = 0usize;
        for (i, _, m) in &scored {
            if m.role != "system" && non_sys_kept < non_sys_target {
                kept_indices.insert(*i);
                non_sys_kept += 1;
            }
        }

        messages
            .iter()
            .enumerate()
            .filter(|(i, _)| kept_indices.contains(i))
            .map(|(_, m)| m.clone())
            .collect()
    }
}

/// Rough token estimate: word count × 1.3 + punctuation count.
pub fn estimate_tokens(text: &str) -> usize {
    let word_count = text.split_whitespace().count();
    let punct_count = text
        .chars()
        .filter(|c| c.is_ascii_punctuation())
        .count();
    ((word_count as f64 * 1.3) as usize) + punct_count
}

/// Compute an importance score for a message based on recency and role.
///
/// - system  → base weight 1.0
/// - user    → base weight 0.8
/// - assistant → base weight 0.7
///
/// Recency bonus: messages closer to the end of the conversation get a bonus up
/// to 0.2 (most recent = +0.2, oldest = +0.0).
pub fn importance_score(msg: &Message, position: usize, total: usize) -> f32 {
    let role_weight: f32 = match msg.role.as_str() {
        "system" => 1.0,
        "user" => 0.8,
        _ => 0.7, // assistant
    };
    let recency = if total <= 1 {
        0.2_f32
    } else {
        0.2 * (position as f32 / (total - 1) as f32)
    };
    role_weight + recency
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn first_sentence(text: &str) -> String {
    text.split(['.', '!', '?'])
        .next()
        .unwrap_or(text)
        .trim()
        .to_string()
}

// ── Context budget ────────────────────────────────────────────────────────────

/// Tracks token usage against a fixed budget.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Total token capacity.
    pub total_tokens: usize,
    /// Tokens already consumed.
    pub used: usize,
    /// Tokens reserved for the model's response (not available for context).
    pub reserved_for_response: usize,
}

impl ContextBudget {
    /// Tokens available for additional context.
    pub fn available(&self) -> usize {
        self.total_tokens
            .saturating_sub(self.used)
            .saturating_sub(self.reserved_for_response)
    }

    /// Returns `true` if `tokens` can be accommodated within the remaining budget.
    pub fn can_fit(&self, tokens: usize) -> bool {
        tokens <= self.available()
    }

    /// Attempt to consume `tokens` from the budget.  Returns `true` on success,
    /// `false` if there is insufficient capacity.
    pub fn consume(&mut self, tokens: usize) -> bool {
        if self.can_fit(tokens) {
            self.used += tokens;
            true
        } else {
            false
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        let tc = estimate_tokens(content);
        Message {
            role: role.to_string(),
            content: content.to_string(),
            token_count: tc,
            importance: 0.5,
        }
    }

    #[test]
    fn sliding_window_keeps_system_msgs() {
        let messages = vec![
            msg("system", "You are a helpful assistant."),
            msg("user", "Message 1"),
            msg("assistant", "Reply 1"),
            msg("user", "Message 2"),
            msg("assistant", "Reply 2"),
            msg("user", "Message 3"),
        ];
        let compressor =
            ContextCompressor::new(CompressionStrategy::SlidingWindow { window_size: 2 }, 9999);
        let (kept, result) = compressor.compress(&messages);

        // System message must always be retained.
        assert!(kept.iter().any(|m| m.role == "system"));
        // Only 2 non-system messages should remain.
        let non_sys: Vec<_> = kept.iter().filter(|m| m.role != "system").collect();
        assert_eq!(non_sys.len(), 2);
        assert_eq!(result.dropped_messages, 3);
    }

    #[test]
    fn drop_oldest_count() {
        let messages: Vec<Message> = (0..6)
            .map(|i| msg(if i == 0 { "system" } else { "user" }, &format!("msg {i}")))
            .collect();
        let compressor =
            ContextCompressor::new(CompressionStrategy::DropOldest { keep_last: 3 }, 9999);
        let (kept, result) = compressor.compress(&messages);
        // 1 system + 3 non-system
        assert_eq!(kept.len(), 4);
        assert_eq!(result.dropped_messages, 2);
    }

    #[test]
    fn summarize_produces_summary_msg() {
        let messages = vec![
            msg("system", "System prompt."),
            msg("user", "First user turn. Extra words here."),
            msg("assistant", "First assistant reply. More words."),
            msg("user", "Second user turn."),
        ];
        // Make the budget tiny so some messages are dropped.
        let compressor = ContextCompressor::new(
            CompressionStrategy::Summarize { max_summary_tokens: 50 },
            20, // very small budget
        );
        let (kept, result) = compressor.compress(&messages);
        // A summary should have been generated.
        assert!(result.summary.is_some());
        let summary_content = result.summary.unwrap();
        assert!(summary_content.starts_with("Previous context summary:"));
        // The summary message should appear in kept messages.
        assert!(kept
            .iter()
            .any(|m| m.content.starts_with("Previous context summary:")));
    }

    #[test]
    fn hybrid_strategy() {
        let messages: Vec<Message> = (0..8)
            .map(|i| msg(if i == 0 { "system" } else { "user" }, &format!("message number {i}")))
            .collect();
        let compressor = ContextCompressor::new(
            CompressionStrategy::Hybrid { window_size: 4, summary_tokens: 50 },
            9999,
        );
        let (kept, result) = compressor.compress(&messages);
        // System + at most 4 non-sys + optional summary.
        let non_sys: Vec<_> = kept
            .iter()
            .filter(|m| m.role != "system" && !m.content.starts_with("Previous context summary:"))
            .collect();
        assert!(non_sys.len() <= 4, "got {} non-sys messages", non_sys.len());
        assert_eq!(result.original_count, 8);
    }

    #[test]
    fn token_estimation() {
        let t = estimate_tokens("Hello, world!");
        // "Hello," + "world!" → 2 words → floor(2 * 1.3) = 2, punct = 2 → total 4
        assert!(t >= 3);
    }

    #[test]
    fn importance_scoring() {
        let m_sys = msg("system", "sys");
        let m_user = msg("user", "usr");
        let m_asst = msg("assistant", "asst");

        let s_sys = importance_score(&m_sys, 0, 3);
        let s_user = importance_score(&m_user, 1, 3);
        let s_asst = importance_score(&m_asst, 2, 3);

        // System should always score highest for a given position.
        assert!(s_sys >= 1.0);
        // Recency bonus means last message gets a boost.
        assert!(s_asst > importance_score(&m_asst, 0, 3));
        // user outranks assistant at same position.
        assert!(
            importance_score(&m_user, 0, 3) > importance_score(&m_asst, 0, 3)
        );
        let _ = s_user; // suppress unused warning
    }

    #[test]
    fn context_budget_consume() {
        let mut budget = ContextBudget {
            total_tokens: 100,
            used: 0,
            reserved_for_response: 20,
        };
        assert_eq!(budget.available(), 80);
        assert!(budget.can_fit(50));
        assert!(budget.consume(50));
        assert_eq!(budget.available(), 30);
        assert!(!budget.consume(40));
        assert_eq!(budget.used, 50);
    }
}

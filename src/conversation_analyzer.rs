//! Conversation quality and engagement analysis.
//!
//! Provides metrics for analysing multi-turn conversations: turn-level
//! statistics, topic drift detection, engagement scoring, and overall quality
//! assessment.

use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Stopwords
// ---------------------------------------------------------------------------

fn stopwords() -> HashSet<&'static str> {
    [
        "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for",
        "of", "with", "by", "from", "is", "it", "its", "be", "was", "are",
        "were", "been", "has", "have", "had", "do", "does", "did", "will",
        "would", "could", "should", "may", "might", "shall", "can", "this",
        "that", "these", "those", "i", "me", "my", "we", "our", "you", "your",
        "he", "she", "they", "them", "his", "her", "their", "what", "which",
        "who", "whom", "not", "no", "so", "if", "as", "up", "out", "about",
        "into", "than", "then", "there", "when", "where", "how", "all", "also",
        "just", "more", "very", "s", "t", "re", "ll", "ve", "d", "m",
    ]
    .iter()
    .copied()
    .collect()
}

// ---------------------------------------------------------------------------
// Structs and enums
// ---------------------------------------------------------------------------

/// Per-turn metrics for a single conversation turn.
#[derive(Debug, Clone)]
pub struct TurnMetrics {
    /// Role of the speaker (e.g. `"user"` or `"assistant"`).
    pub role: String,
    /// Number of words in the turn.
    pub word_count: usize,
    /// Number of characters in the turn.
    pub char_count: usize,
    /// Number of interrogative patterns detected.
    pub question_count: usize,
    /// Time from the previous turn to this turn in milliseconds, if known.
    pub response_time_ms: Option<u64>,
    /// Sentiment score in the range `[-1.0, 1.0]`.
    pub sentiment_score: f64,
    /// Top keywords extracted from this turn.
    pub topic_keywords: Vec<String>,
}

/// Represents a detected topic drift between two consecutive windows of turns.
#[derive(Debug, Clone)]
pub struct TopicDrift {
    /// Topic label of the window before the drift.
    pub from_topic: String,
    /// Topic label of the window after the drift.
    pub to_topic: String,
    /// Index of the first turn in the later window.
    pub turn_index: usize,
    /// Jaccard distance (1 − similarity). Higher means more drift.
    pub drift_score: f64,
}

/// Aggregate engagement metrics for a whole conversation.
#[derive(Debug, Clone)]
pub struct EngagementMetrics {
    /// Total number of turns.
    pub total_turns: usize,
    /// Average word count per turn.
    pub avg_response_length: f64,
    /// Ratio of questions to total turns.
    pub question_answer_ratio: f64,
    /// Average Jaccard similarity between consecutive turns' keywords.
    pub topic_coherence: f64,
    /// Mean sentiment score across all turns.
    pub avg_sentiment: f64,
    /// Most frequent keywords across the conversation.
    pub dominant_topics: Vec<String>,
    /// Composite user engagement score in `[0.0, 1.0]`.
    pub user_engagement_score: f64,
}

/// High-level quality assessment for a conversation.
#[derive(Debug, Clone)]
pub struct ConversationQuality {
    /// How well-connected the topics are across turns.
    pub coherence: f64,
    /// Depth of content (approximated by average length and question density).
    pub depth: f64,
    /// Breadth of topics covered (unique keyword ratio).
    pub breadth: f64,
    /// Fraction of questions that appear to receive a follow-up response.
    pub resolution_rate: f64,
    /// Weighted combination of the other scores.
    pub overall_score: f64,
}

// ---------------------------------------------------------------------------
// ConversationAnalyzer
// ---------------------------------------------------------------------------

/// Stateless analyser for conversation quality and engagement.
pub struct ConversationAnalyzer;

impl ConversationAnalyzer {
    /// Create a new `ConversationAnalyzer`.
    pub fn new() -> Self {
        Self
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Analyse a single turn and return its metrics.
    ///
    /// `prev_text` is the text of the immediately preceding turn and is used
    /// only for future response-time calculations; the caller is responsible
    /// for supplying `response_time_ms` externally if known.
    pub fn analyze_turn(
        &self,
        text: &str,
        role: &str,
        _prev_text: Option<&str>,
    ) -> TurnMetrics {
        let words: Vec<&str> = text.split_whitespace().collect();
        let word_count = words.len();
        let char_count = text.len();
        let question_count = self.identify_questions(text);
        let sentiment_score = self.sentiment_score(text);
        let topic_keywords = self.extract_keywords(text, 8);

        TurnMetrics {
            role: role.to_string(),
            word_count,
            char_count,
            question_count,
            response_time_ms: None,
            sentiment_score,
            topic_keywords,
        }
    }

    /// Extract the top `top_n` keywords from `text` using TF weighting with
    /// stopword removal.
    pub fn extract_keywords(&self, text: &str, top_n: usize) -> Vec<String> {
        let stops = stopwords();
        let mut freq: HashMap<String, usize> = HashMap::new();

        for word in text.split_whitespace() {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphabetic())
                .collect::<String>()
                .to_lowercase();
            if clean.len() > 2 && !stops.contains(clean.as_str()) {
                *freq.entry(clean).or_insert(0) += 1;
            }
        }

        let mut pairs: Vec<(String, usize)> = freq.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        pairs.into_iter().take(top_n).map(|(k, _)| k).collect()
    }

    /// Detect topic drift across `turns` using a sliding window of size
    /// `window`.  Returns one [`TopicDrift`] entry per window boundary where
    /// drift is detected (Jaccard similarity < 0.5).
    pub fn detect_topic_drift(
        &self,
        turns: &[TurnMetrics],
        window: usize,
    ) -> Vec<TopicDrift> {
        if turns.len() < window * 2 || window == 0 {
            return Vec::new();
        }

        let mut drifts = Vec::new();

        for i in window..turns.len() {
            let prev_window = &turns[i.saturating_sub(window)..i];
            let curr_start = i;
            let curr_end = (i + window).min(turns.len());
            let curr_window = &turns[curr_start..curr_end];

            let prev_kw: HashSet<String> = prev_window
                .iter()
                .flat_map(|t| t.topic_keywords.iter().cloned())
                .collect();
            let curr_kw: HashSet<String> = curr_window
                .iter()
                .flat_map(|t| t.topic_keywords.iter().cloned())
                .collect();

            let similarity = jaccard_similarity(&prev_kw, &curr_kw);
            let drift_score = 1.0 - similarity;

            if drift_score > 0.5 {
                let from_topic = prev_kw
                    .iter()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let to_topic = curr_kw
                    .iter()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                drifts.push(TopicDrift {
                    from_topic,
                    to_topic,
                    turn_index: i,
                    drift_score,
                });
            }
        }

        drifts
    }

    /// Compute aggregate engagement metrics across all `turns`.
    pub fn compute_engagement(&self, turns: &[TurnMetrics]) -> EngagementMetrics {
        if turns.is_empty() {
            return EngagementMetrics {
                total_turns: 0,
                avg_response_length: 0.0,
                question_answer_ratio: 0.0,
                topic_coherence: 0.0,
                avg_sentiment: 0.0,
                dominant_topics: Vec::new(),
                user_engagement_score: 0.0,
            };
        }

        let total_turns = turns.len();
        let avg_response_length =
            turns.iter().map(|t| t.word_count as f64).sum::<f64>() / total_turns as f64;
        let total_questions: usize = turns.iter().map(|t| t.question_count).sum();
        let question_answer_ratio = total_questions as f64 / total_turns as f64;
        let topic_coherence = self.topic_coherence(turns);
        let avg_sentiment =
            turns.iter().map(|t| t.sentiment_score).sum::<f64>() / total_turns as f64;
        let dominant_topics = self.dominant_topics(turns, 5);

        // Engagement score: blend of length, coherence, and question density.
        let length_score = (avg_response_length / 100.0).min(1.0);
        let coherence_score = topic_coherence;
        let question_score = (question_answer_ratio / 2.0).min(1.0);
        let user_engagement_score =
            (length_score * 0.4 + coherence_score * 0.4 + question_score * 0.2).min(1.0);

        EngagementMetrics {
            total_turns,
            avg_response_length,
            question_answer_ratio,
            topic_coherence,
            avg_sentiment,
            dominant_topics,
            user_engagement_score,
        }
    }

    /// Assess the overall quality of a conversation.
    pub fn assess_quality(
        &self,
        turns: &[TurnMetrics],
        has_resolution: bool,
    ) -> ConversationQuality {
        if turns.is_empty() {
            return ConversationQuality {
                coherence: 0.0,
                depth: 0.0,
                breadth: 0.0,
                resolution_rate: 0.0,
                overall_score: 0.0,
            };
        }

        let coherence = self.topic_coherence(turns);

        let avg_words =
            turns.iter().map(|t| t.word_count as f64).sum::<f64>() / turns.len() as f64;
        let total_questions: usize = turns.iter().map(|t| t.question_count).sum();
        let question_density = total_questions as f64 / turns.len() as f64;
        let depth = ((avg_words / 150.0) * 0.7 + (question_density / 2.0) * 0.3).min(1.0);

        let all_keywords: HashSet<String> = turns
            .iter()
            .flat_map(|t| t.topic_keywords.iter().cloned())
            .collect();
        let total_keywords: usize = turns.iter().map(|t| t.topic_keywords.len()).sum();
        let breadth = if total_keywords > 0 {
            (all_keywords.len() as f64 / total_keywords as f64).min(1.0)
        } else {
            0.0
        };

        let resolution_rate = if has_resolution { 1.0 } else { 0.0 };

        let overall_score = coherence * 0.3 + depth * 0.3 + breadth * 0.2 + resolution_rate * 0.2;

        ConversationQuality {
            coherence,
            depth,
            breadth,
            resolution_rate,
            overall_score,
        }
    }

    /// Compute a simple sentiment score in `[-1.0, 1.0]` by counting positive
    /// and negative words.
    pub fn sentiment_score(&self, text: &str) -> f64 {
        let positive = [
            "good", "great", "excellent", "amazing", "wonderful", "fantastic",
            "helpful", "love", "like", "best", "perfect", "happy", "glad",
            "pleased", "superb", "outstanding", "brilliant", "thank", "thanks",
            "appreciate", "useful", "clear", "nice", "positive", "correct",
        ];
        let negative = [
            "bad", "terrible", "awful", "horrible", "worst", "hate", "dislike",
            "wrong", "error", "fail", "failed", "failure", "poor", "broken",
            "confusing", "confused", "unclear", "problem", "issue", "bug",
            "difficult", "hard", "annoying", "frustrating", "useless",
        ];

        let lower = text.to_lowercase();
        let words: Vec<&str> = lower.split_whitespace().collect();
        let total = words.len() as f64;

        if total == 0.0 {
            return 0.0;
        }

        let pos_count = words.iter().filter(|w| positive.contains(*w)).count() as f64;
        let neg_count = words.iter().filter(|w| negative.contains(*w)).count() as f64;

        ((pos_count - neg_count) / total * 10.0).clamp(-1.0, 1.0)
    }

    /// Compute average Jaccard similarity between consecutive turns' keyword
    /// sets.  Returns `1.0` for single-turn conversations.
    pub fn topic_coherence(&self, turns: &[TurnMetrics]) -> f64 {
        if turns.len() < 2 {
            return 1.0;
        }

        let mut total = 0.0;
        let mut count = 0usize;

        for pair in turns.windows(2) {
            let a: HashSet<String> = pair[0].topic_keywords.iter().cloned().collect();
            let b: HashSet<String> = pair[1].topic_keywords.iter().cloned().collect();
            total += jaccard_similarity(&a, &b);
            count += 1;
        }

        if count == 0 {
            1.0
        } else {
            total / count as f64
        }
    }

    /// Count interrogative patterns in `text`.
    pub fn identify_questions(&self, text: &str) -> usize {
        let lower = text.to_lowercase();
        let mut count = 0usize;

        // Count sentences ending with '?'
        count += text.chars().filter(|&c| c == '?').count();

        // Count interrogative starters (only if they don't already end with '?')
        let starters = [
            "what ", "why ", "how ", "when ", "where ", "who ", "which ",
            "could you", "can you", "would you", "do you", "did you",
            "have you", "is it", "are you", "is there",
        ];
        for line in lower.lines() {
            let trimmed = line.trim();
            if !trimmed.ends_with('?') {
                for starter in &starters {
                    if trimmed.starts_with(starter) {
                        count += 1;
                        break;
                    }
                }
            }
        }

        count
    }

    /// Analyse a full conversation given as `(role, text)` pairs and return
    /// engagement metrics, quality assessment, and detected topic drifts.
    pub fn analyze_full(
        &self,
        conversation: &[(String, String)],
    ) -> (EngagementMetrics, ConversationQuality, Vec<TopicDrift>) {
        let turns: Vec<TurnMetrics> = conversation
            .iter()
            .enumerate()
            .map(|(i, (role, text))| {
                let prev = if i > 0 {
                    Some(conversation[i - 1].1.as_str())
                } else {
                    None
                };
                self.analyze_turn(text, role, prev)
            })
            .collect();

        let engagement = self.compute_engagement(&turns);
        // Heuristic: assume the last turn resolves the conversation if the
        // last speaker is the assistant.
        let has_resolution = conversation
            .last()
            .map(|(role, _)| role == "assistant")
            .unwrap_or(false);
        let quality = self.assess_quality(&turns, has_resolution);
        let drifts = self.detect_topic_drift(&turns, 2);

        (engagement, quality, drifts)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn dominant_topics(&self, turns: &[TurnMetrics], top_n: usize) -> Vec<String> {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for turn in turns {
            for kw in &turn.topic_keywords {
                *freq.entry(kw.clone()).or_insert(0) += 1;
            }
        }
        let mut pairs: Vec<(String, usize)> = freq.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        pairs.into_iter().take(top_n).map(|(k, _)| k).collect()
    }
}

impl Default for ConversationAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        1.0
    } else {
        intersection / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sentiment_positive() {
        let a = ConversationAnalyzer::new();
        let score = a.sentiment_score("This is great and wonderful, thank you!");
        assert!(score > 0.0, "expected positive sentiment, got {score}");
    }

    #[test]
    fn test_sentiment_negative() {
        let a = ConversationAnalyzer::new();
        let score = a.sentiment_score("This is terrible and broken and awful");
        assert!(score < 0.0, "expected negative sentiment, got {score}");
    }

    #[test]
    fn test_identify_questions() {
        let a = ConversationAnalyzer::new();
        assert_eq!(a.identify_questions("How does this work? What is that?"), 2);
    }

    #[test]
    fn test_extract_keywords() {
        let a = ConversationAnalyzer::new();
        let kw = a.extract_keywords("the quick brown fox jumps over the lazy dog", 3);
        assert!(!kw.is_empty());
    }

    #[test]
    fn test_analyze_full() {
        let a = ConversationAnalyzer::new();
        let conv = vec![
            ("user".to_string(), "How do I install Rust?".to_string()),
            (
                "assistant".to_string(),
                "You can install Rust using rustup. It is a great tool.".to_string(),
            ),
        ];
        let (eng, qual, _drifts) = a.analyze_full(&conv);
        assert_eq!(eng.total_turns, 2);
        assert!(qual.overall_score >= 0.0);
    }
}

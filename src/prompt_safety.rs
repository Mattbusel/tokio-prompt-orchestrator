//! Content moderation, toxicity detection, and PII detection.

use std::collections::HashSet;

/// Categories of unsafe content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SafetyCategory {
    Hate,
    Violence,
    SelfHarm,
    SexualContent,
    Harassment,
    Spam,
    PII,
    Safe,
}

/// A score for a single safety category.
#[derive(Debug, Clone)]
pub struct SafetyScore {
    pub category: SafetyCategory,
    pub confidence: f64,
    pub triggered_phrases: Vec<String>,
}

/// A detected PII match within the input text.
#[derive(Debug, Clone)]
pub struct PiiMatch {
    pub pii_type: String,
    pub value: String,
    pub start: usize,
    pub end: usize,
}

/// Full result of a safety analysis.
#[derive(Debug, Clone)]
pub struct SafetyResult {
    pub input: String,
    pub scores: Vec<SafetyScore>,
    pub overall_risk: f64,
    pub blocked: bool,
    pub pii_detected: Vec<PiiMatch>,
}

/// Configuration for the content moderator.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub block_threshold: f64,
    pub categories_enabled: Vec<SafetyCategory>,
    pub redact_pii: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            block_threshold: 0.5,
            categories_enabled: vec![
                SafetyCategory::Hate,
                SafetyCategory::Violence,
                SafetyCategory::SelfHarm,
                SafetyCategory::SexualContent,
                SafetyCategory::Harassment,
                SafetyCategory::Spam,
                SafetyCategory::PII,
            ],
            redact_pii: true,
        }
    }
}

/// Content moderation engine.
pub struct ContentModerator {
    pub config: SafetyConfig,
    pub hate_phrases: Vec<String>,
    pub violence_phrases: Vec<String>,
    pub spam_indicators: Vec<String>,
}

impl ContentModerator {
    /// Create a new `ContentModerator` with pre-populated phrase lists.
    pub fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            hate_phrases: vec![
                "kill all".to_string(),
                "hate you".to_string(),
                "die".to_string(),
                "worthless".to_string(),
            ],
            violence_phrases: vec![
                "bomb".to_string(),
                "explode".to_string(),
                "shoot".to_string(),
                "attack".to_string(),
            ],
            spam_indicators: vec![
                "click here".to_string(),
                "buy now".to_string(),
                "limited offer".to_string(),
                "act now".to_string(),
                "free money".to_string(),
            ],
        }
    }

    /// Scan text for toxic content per category. Confidence = hit_count / word_count.
    pub fn detect_toxicity(&self, text: &str) -> Vec<SafetyScore> {
        let lower = text.to_lowercase();
        let word_count = text.split_whitespace().count().max(1);
        let mut scores = Vec::new();

        // Hate
        {
            let mut triggered = Vec::new();
            for phrase in &self.hate_phrases {
                if lower.contains(phrase.as_str()) {
                    triggered.push(phrase.clone());
                }
            }
            if !triggered.is_empty() {
                let confidence = (triggered.len() as f64 / word_count as f64).min(1.0);
                scores.push(SafetyScore {
                    category: SafetyCategory::Hate,
                    confidence,
                    triggered_phrases: triggered,
                });
            }
        }

        // Violence
        {
            let mut triggered = Vec::new();
            for phrase in &self.violence_phrases {
                if lower.contains(phrase.as_str()) {
                    triggered.push(phrase.clone());
                }
            }
            if !triggered.is_empty() {
                let confidence = (triggered.len() as f64 / word_count as f64).min(1.0);
                scores.push(SafetyScore {
                    category: SafetyCategory::Violence,
                    confidence,
                    triggered_phrases: triggered,
                });
            }
        }

        // Spam
        {
            let mut triggered = Vec::new();
            for phrase in &self.spam_indicators {
                if lower.contains(phrase.as_str()) {
                    triggered.push(phrase.clone());
                }
            }
            if !triggered.is_empty() {
                let confidence = (triggered.len() as f64 / word_count as f64).min(1.0);
                scores.push(SafetyScore {
                    category: SafetyCategory::Spam,
                    confidence,
                    triggered_phrases: triggered,
                });
            }
        }

        scores
    }

    /// Detect PII in the given text using pattern matching.
    pub fn detect_pii(text: &str) -> Vec<PiiMatch> {
        let mut matches = Vec::new();

        // Email: contains '@' and '.'
        for (start, word) in Self::word_spans(text) {
            let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '-' && c != '_');
            if w.contains('@') && w.contains('.') && w.len() > 3 {
                let end = start + word.len();
                matches.push(PiiMatch {
                    pii_type: "email".to_string(),
                    value: w.to_string(),
                    start,
                    end,
                });
            }
        }

        // Phone: 10+ consecutive digit sequence (may include spaces/dashes)
        {
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i].is_ascii_digit() {
                    let start = i;
                    let mut count = 0;
                    let mut end = i;
                    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'-' || bytes[end] == b' ') {
                        if bytes[end].is_ascii_digit() {
                            count += 1;
                        }
                        end += 1;
                    }
                    if count >= 10 {
                        let value = text[start..end].trim().to_string();
                        matches.push(PiiMatch {
                            pii_type: "phone".to_string(),
                            value,
                            start,
                            end,
                        });
                        i = end;
                        continue;
                    }
                }
                i += 1;
            }
        }

        // SSN: XXX-XX-XXXX pattern
        {
            let chars: Vec<char> = text.chars().collect();
            let s: String = chars.iter().collect();
            let mut search_start = 0;
            while search_start < s.len() {
                if let Some(pos) = Self::find_ssn(&s[search_start..]) {
                    let abs_pos = search_start + pos.0;
                    matches.push(PiiMatch {
                        pii_type: "ssn".to_string(),
                        value: pos.1.clone(),
                        start: abs_pos,
                        end: abs_pos + pos.1.len(),
                    });
                    search_start = abs_pos + pos.1.len();
                } else {
                    break;
                }
            }
        }

        // Credit card: 16-digit groups (4x4 separated by spaces or dashes)
        {
            let mut search = text;
            let mut offset = 0;
            loop {
                if let Some((pos, val)) = Self::find_credit_card(search) {
                    matches.push(PiiMatch {
                        pii_type: "credit_card".to_string(),
                        value: val.clone(),
                        start: offset + pos,
                        end: offset + pos + val.len(),
                    });
                    let advance = pos + val.len();
                    offset += advance;
                    search = &search[advance..];
                } else {
                    break;
                }
            }
        }

        // IP address: X.X.X.X pattern
        {
            let mut search = text;
            let mut offset = 0;
            loop {
                if let Some((pos, val)) = Self::find_ip(search) {
                    matches.push(PiiMatch {
                        pii_type: "ip_address".to_string(),
                        value: val.clone(),
                        start: offset + pos,
                        end: offset + pos + val.len(),
                    });
                    let advance = pos + val.len();
                    offset += advance;
                    search = &search[advance..];
                } else {
                    break;
                }
            }
        }

        matches
    }

    /// Replace matched PII spans with "[REDACTED]".
    pub fn redact_pii(text: &str, matches: &[PiiMatch]) -> String {
        if matches.is_empty() {
            return text.to_string();
        }
        // Sort by start position descending so we can replace without shifting indices
        let mut sorted: Vec<&PiiMatch> = matches.iter().collect();
        sorted.sort_by(|a, b| b.start.cmp(&a.start));

        let mut result = text.to_string();
        // Deduplicate overlapping ranges
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for m in sorted {
            if seen.contains(&(m.start, m.end)) {
                continue;
            }
            if m.end <= result.len() {
                seen.insert((m.start, m.end));
                result.replace_range(m.start..m.end, "[REDACTED]");
            }
        }
        result
    }

    /// Run all detectors and produce a `SafetyResult`.
    pub fn analyze(&self, text: &str) -> SafetyResult {
        let scores = self.detect_toxicity(text);
        let pii_detected = Self::detect_pii(text);
        let overall_risk = Self::overall_risk(&scores);
        let blocked = overall_risk >= self.config.block_threshold;
        SafetyResult {
            input: text.to_string(),
            scores,
            overall_risk,
            blocked,
            pii_detected,
        }
    }

    /// Compute overall risk as the maximum confidence across all triggered categories.
    pub fn overall_risk(scores: &[SafetyScore]) -> f64 {
        scores.iter().map(|s| s.confidence).fold(0.0_f64, f64::max)
    }

    // --- private helpers ---

    fn word_spans(text: &str) -> Vec<(usize, &str)> {
        let mut spans = Vec::new();
        let mut start = 0;
        let mut in_word = false;
        for (i, c) in text.char_indices() {
            if c.is_whitespace() {
                if in_word {
                    spans.push((start, &text[start..i]));
                    in_word = false;
                }
                start = i + c.len_utf8();
            } else {
                if !in_word {
                    start = i;
                    in_word = true;
                }
            }
        }
        if in_word {
            spans.push((start, &text[start..]));
        }
        spans
    }

    fn find_ssn(text: &str) -> Option<(usize, String)> {
        let bytes = text.as_bytes();
        for i in 0..bytes.len() {
            if i + 11 <= bytes.len() {
                let slice = &bytes[i..i + 11];
                // Pattern: DDD-DD-DDDD
                let ok = slice[0].is_ascii_digit()
                    && slice[1].is_ascii_digit()
                    && slice[2].is_ascii_digit()
                    && slice[3] == b'-'
                    && slice[4].is_ascii_digit()
                    && slice[5].is_ascii_digit()
                    && slice[6] == b'-'
                    && slice[7].is_ascii_digit()
                    && slice[8].is_ascii_digit()
                    && slice[9].is_ascii_digit()
                    && slice[10].is_ascii_digit();
                if ok {
                    // Ensure not part of a longer digit sequence
                    let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
                    let after_ok = i + 11 >= bytes.len() || !bytes[i + 11].is_ascii_digit();
                    if before_ok && after_ok {
                        let val = std::str::from_utf8(&bytes[i..i + 11]).unwrap().to_string();
                        return Some((i, val));
                    }
                }
            }
        }
        None
    }

    fn find_credit_card(text: &str) -> Option<(usize, String)> {
        let bytes = text.as_bytes();
        // Look for 16 consecutive digits, possibly split as 4-4-4-4 with space/dash
        for i in 0..bytes.len() {
            // Try compact 16 digits
            if i + 16 <= bytes.len() {
                let slice = &bytes[i..i + 16];
                if slice.iter().all(|b| b.is_ascii_digit()) {
                    let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
                    let after_ok = i + 16 >= bytes.len() || !bytes[i + 16].is_ascii_digit();
                    if before_ok && after_ok {
                        let val = std::str::from_utf8(slice).unwrap().to_string();
                        return Some((i, val));
                    }
                }
            }
            // Try 4-4-4-4 with separator
            if i + 19 <= bytes.len() {
                let sep = bytes[i + 4];
                if (sep == b' ' || sep == b'-')
                    && bytes[i..i + 4].iter().all(|b| b.is_ascii_digit())
                    && bytes[i + 5..i + 9].iter().all(|b| b.is_ascii_digit())
                    && bytes[i + 9] == sep
                    && bytes[i + 10..i + 14].iter().all(|b| b.is_ascii_digit())
                    && bytes[i + 14] == sep
                    && bytes[i + 15..i + 19].iter().all(|b| b.is_ascii_digit())
                {
                    let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
                    let after_ok = i + 19 >= bytes.len() || !bytes[i + 19].is_ascii_digit();
                    if before_ok && after_ok {
                        let val = std::str::from_utf8(&bytes[i..i + 19]).unwrap().to_string();
                        return Some((i, val));
                    }
                }
            }
        }
        None
    }

    fn find_ip(text: &str) -> Option<(usize, String)> {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                // Try to parse X.X.X.X
                let start = i;
                let mut parts: Vec<u8> = Vec::new();
                let mut cur_num: Option<u32> = None;
                let mut j = i;
                let mut dot_count = 0;
                while j < bytes.len() {
                    if bytes[j].is_ascii_digit() {
                        let d = (bytes[j] - b'0') as u32;
                        cur_num = Some(cur_num.unwrap_or(0) * 10 + d);
                        if cur_num.unwrap_or(0) > 255 {
                            break;
                        }
                        j += 1;
                    } else if bytes[j] == b'.' && dot_count < 3 {
                        if let Some(n) = cur_num {
                            parts.push(n as u8);
                            cur_num = None;
                            dot_count += 1;
                            j += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                if let Some(n) = cur_num {
                    parts.push(n as u8);
                }
                if parts.len() == 4 {
                    let before_ok = start == 0 || !bytes[start - 1].is_ascii_digit() && bytes[start - 1] != b'.';
                    let after_ok = j >= bytes.len() || !bytes[j].is_ascii_digit() && bytes[j] != b'.';
                    if before_ok && after_ok {
                        let val = text[start..j].to_string();
                        return Some((start, val));
                    }
                }
                i = j.max(start + 1);
            } else {
                i += 1;
            }
        }
        None
    }
}

/// A safety filter that wraps a `ContentModerator` and supports an allow-list.
pub struct SafetyFilter {
    pub moderator: ContentModerator,
    pub allow_list: Vec<String>,
}

impl SafetyFilter {
    /// Create a new `SafetyFilter`.
    pub fn new(moderator: ContentModerator, allow_list: Vec<String>) -> Self {
        Self { moderator, allow_list }
    }

    /// Redact PII from `text` and return the processed text along with the full `SafetyResult`.
    pub fn check_and_redact(&self, text: &str) -> (String, SafetyResult) {
        let result = self.moderator.analyze(text);
        let processed = if self.moderator.config.redact_pii {
            ContentModerator::redact_pii(text, &result.pii_detected)
        } else {
            text.to_string()
        };
        (processed, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_moderator() -> ContentModerator {
        ContentModerator::new(SafetyConfig::default())
    }

    #[test]
    fn test_hate_phrase_detection() {
        let m = default_moderator();
        let scores = m.detect_toxicity("I hate you so much, you are worthless.");
        let hate = scores.iter().find(|s| s.category == SafetyCategory::Hate);
        assert!(hate.is_some(), "expected hate category to be triggered");
        assert!(hate.unwrap().confidence > 0.0);
    }

    #[test]
    fn test_spam_detection() {
        let m = default_moderator();
        let scores = m.detect_toxicity("Click here and buy now for free money!");
        let spam = scores.iter().find(|s| s.category == SafetyCategory::Spam);
        assert!(spam.is_some(), "expected spam category to be triggered");
    }

    #[test]
    fn test_email_pii_found() {
        let matches = ContentModerator::detect_pii("Contact us at user@example.com for details.");
        let email = matches.iter().find(|m| m.pii_type == "email");
        assert!(email.is_some(), "expected email PII to be detected");
        assert!(email.unwrap().value.contains('@'));
    }

    #[test]
    fn test_ssn_pattern_matched() {
        let matches = ContentModerator::detect_pii("SSN is 123-45-6789 on file.");
        let ssn = matches.iter().find(|m| m.pii_type == "ssn");
        assert!(ssn.is_some(), "expected SSN PII to be detected");
        assert_eq!(ssn.unwrap().value, "123-45-6789");
    }

    #[test]
    fn test_pii_redaction() {
        let text = "Email user@example.com or call 1234567890 today.";
        let matches = ContentModerator::detect_pii(text);
        let redacted = ContentModerator::redact_pii(text, &matches);
        assert!(!redacted.contains("user@example.com"), "email should be redacted");
    }

    #[test]
    fn test_safe_text_passes() {
        let m = default_moderator();
        let result = m.analyze("The weather is nice today. I enjoyed a walk in the park.");
        assert!(!result.blocked, "safe text should not be blocked");
        assert_eq!(result.overall_risk, 0.0);
    }
}

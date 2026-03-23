//! Route prompts to different handlers based on content and intent.
//!
//! Rules are evaluated in descending priority order; the first matching rule
//! determines the [`RouteTarget`].  If no rule matches, a built-in default
//! rule that always routes to `DirectModel("default")` is used.

/// Where a matched prompt should be sent.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteTarget {
    /// Expand via a named template.
    Template(String),
    /// Send directly to the named model.
    DirectModel(String),
    /// Run through a named sequence of pipeline stages.
    Pipeline(Vec<String>),
    /// Reject the prompt with an explanation.
    Reject { reason: String },
}

/// A condition that can be tested against a prompt string and optional intent.
#[derive(Debug, Clone)]
pub enum RoutingCondition {
    /// Prompt contains the given keyword (case-insensitive).
    ContainsKeyword(String),
    /// Prompt is longer than N characters.
    LongerThan(usize),
    /// Prompt is shorter than N characters.
    ShorterThan(usize),
    /// Prompt matches a glob pattern (`*` = any substring, `?` = any char).
    MatchesRegex(String),
    /// The supplied intent string equals this value (case-insensitive).
    HasIntent(String),
    /// Always evaluates to `true`.
    Always,
}

/// A single routing rule with priority and conditions.
#[derive(Debug, Clone)]
pub struct RoutingRule {
    /// Human-readable name for logging and debugging.
    pub name: String,
    /// Higher values are evaluated first.
    pub priority: u32,
    /// Conditions that must be satisfied.
    pub conditions: Vec<RoutingCondition>,
    /// Where to send the prompt if this rule fires.
    pub target: RouteTarget,
    /// `true` → all conditions must match (AND); `false` → any condition (OR).
    pub match_all: bool,
}

/// The result of routing a prompt.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// Name of the rule that fired.
    pub rule_name: String,
    /// Resolved target.
    pub target: RouteTarget,
    /// Confidence in [0.0, 1.0].
    pub confidence: f64,
    /// Which condition descriptions contributed to the match.
    pub matched_conditions: Vec<String>,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Routes prompts to targets by evaluating a sorted list of [`RoutingRule`]s.
pub struct PromptRouter {
    rules: Vec<RoutingRule>,
}

impl PromptRouter {
    /// Create an empty router (no rules).
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a rule; the internal list is kept sorted by priority descending.
    pub fn add_rule(&mut self, rule: RoutingRule) {
        self.rules.push(rule);
        self.rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Route `prompt` (with optional `intent`) and return the first matching
    /// [`RoutingDecision`].  Falls back to `DirectModel("default")` if no rule
    /// matches.
    pub fn route(&self, prompt: &str, intent: Option<&str>) -> RoutingDecision {
        for rule in &self.rules {
            let (matched, matched_conds) =
                evaluate_rule(rule, prompt, intent);
            if matched {
                let confidence = if rule.conditions.is_empty() {
                    0.5
                } else {
                    matched_conds.len() as f64 / rule.conditions.len() as f64
                };
                return RoutingDecision {
                    rule_name: rule.name.clone(),
                    target: rule.target.clone(),
                    confidence,
                    matched_conditions: matched_conds,
                };
            }
        }

        // Default fallback
        RoutingDecision {
            rule_name: "default".to_string(),
            target: RouteTarget::DirectModel("default".to_string()),
            confidence: 1.0,
            matched_conditions: vec!["Always (fallback)".to_string()],
        }
    }

    /// For every rule, return `(rule_name, matched, reason_strings)`.
    pub fn explain_routing(
        &self,
        prompt: &str,
    ) -> Vec<(String, bool, Vec<String>)> {
        self.rules
            .iter()
            .map(|rule| {
                let (matched, conds) = evaluate_rule(rule, prompt, None);
                (rule.name.clone(), matched, conds)
            })
            .collect()
    }
}

impl Default for PromptRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Condition evaluation ──────────────────────────────────────────────────────

/// Evaluate a single condition; returns `(matched, description)`.
pub fn evaluate_condition(
    condition: &RoutingCondition,
    prompt: &str,
    intent: Option<&str>,
) -> bool {
    match condition {
        RoutingCondition::ContainsKeyword(kw) => {
            prompt.to_lowercase().contains(&kw.to_lowercase())
        }
        RoutingCondition::LongerThan(n) => prompt.len() > *n,
        RoutingCondition::ShorterThan(n) => prompt.len() < *n,
        RoutingCondition::MatchesRegex(pattern) => glob_match(pattern, prompt),
        RoutingCondition::HasIntent(expected) => intent
            .map(|i| i.to_lowercase() == expected.to_lowercase())
            .unwrap_or(false),
        RoutingCondition::Always => true,
    }
}

fn condition_description(condition: &RoutingCondition) -> String {
    match condition {
        RoutingCondition::ContainsKeyword(kw) => format!("contains keyword '{kw}'"),
        RoutingCondition::LongerThan(n) => format!("longer than {n} chars"),
        RoutingCondition::ShorterThan(n) => format!("shorter than {n} chars"),
        RoutingCondition::MatchesRegex(p) => format!("matches glob '{p}'"),
        RoutingCondition::HasIntent(i) => format!("has intent '{i}'"),
        RoutingCondition::Always => "always".to_string(),
    }
}

/// Evaluate an entire rule against `prompt` + `intent`.
/// Returns `(overall_match, list_of_descriptions_for_matched_conditions)`.
fn evaluate_rule(
    rule: &RoutingRule,
    prompt: &str,
    intent: Option<&str>,
) -> (bool, Vec<String>) {
    let mut matched_conds: Vec<String> = Vec::new();

    if rule.conditions.is_empty() {
        // No conditions → always fire.
        return (true, vec!["(no conditions)".to_string()]);
    }

    for cond in &rule.conditions {
        if evaluate_condition(cond, prompt, intent) {
            matched_conds.push(condition_description(cond));
        }
    }

    let overall = if rule.match_all {
        matched_conds.len() == rule.conditions.len()
    } else {
        !matched_conds.is_empty()
    };

    (overall, matched_conds)
}

// ── Glob matching ─────────────────────────────────────────────────────────────

/// Minimal glob-style matching: `*` matches any substring, `?` matches any
/// single character.  Case-insensitive.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some('*'), _) => {
            // Star: match zero characters or consume one text character.
            glob_match_inner(&pattern[1..], text)
                || (!text.is_empty() && glob_match_inner(pattern, &text[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pattern[1..], &text[1..]),
        (Some('?'), None) => false,
        (Some(p), Some(t)) => p == t && glob_match_inner(&pattern[1..], &text[1..]),
        (Some(_), None) => false,
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Fluent builder for [`PromptRouter`].
pub struct PromptRouterBuilder {
    pending_conditions: Vec<RoutingCondition>,
    pending_name: String,
    pending_priority: u32,
    pending_match_all: bool,
    rules: Vec<RoutingRule>,
}

impl PromptRouterBuilder {
    /// Start a new builder.
    pub fn new() -> Self {
        Self {
            pending_conditions: Vec::new(),
            pending_name: "rule".to_string(),
            pending_priority: 0,
            pending_match_all: false,
            rules: Vec::new(),
        }
    }

    /// Name the next rule.
    pub fn named(mut self, name: &str) -> Self {
        self.pending_name = name.to_string();
        self
    }

    /// Set priority for the next rule.
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.pending_priority = priority;
        self
    }

    /// Require all conditions (AND mode).
    pub fn all_of(mut self) -> Self {
        self.pending_match_all = true;
        self
    }

    /// Add a condition to the pending rule.
    pub fn when(mut self, condition: RoutingCondition) -> Self {
        self.pending_conditions.push(condition);
        self
    }

    /// Commit the pending rule with the given target and reset pending state.
    pub fn route_to(mut self, target: RouteTarget) -> Self {
        self.rules.push(RoutingRule {
            name: self.pending_name.clone(),
            priority: self.pending_priority,
            conditions: std::mem::take(&mut self.pending_conditions),
            target,
            match_all: self.pending_match_all,
        });
        // Reset pending state.
        self.pending_name = "rule".to_string();
        self.pending_priority = 0;
        self.pending_match_all = false;
        self
    }

    /// Consume the builder and return a [`PromptRouter`].
    pub fn build(self) -> PromptRouter {
        let mut router = PromptRouter::new();
        for rule in self.rules {
            router.add_rule(rule);
        }
        router
    }
}

impl Default for PromptRouterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword_rule(kw: &str, priority: u32, target: RouteTarget) -> RoutingRule {
        RoutingRule {
            name: format!("kw:{kw}"),
            priority,
            conditions: vec![RoutingCondition::ContainsKeyword(kw.to_string())],
            target,
            match_all: false,
        }
    }

    #[test]
    fn keyword_match_routing() {
        let mut router = PromptRouter::new();
        router.add_rule(keyword_rule("summarize", 10, RouteTarget::Template("summary".to_string())));

        let decision = router.route("Please summarize this document.", None);
        assert_eq!(decision.rule_name, "kw:summarize");
        match decision.target {
            RouteTarget::Template(t) => assert_eq!(t, "summary"),
            other => panic!("unexpected target: {other:?}"),
        }
        assert!(!decision.matched_conditions.is_empty());
    }

    #[test]
    fn priority_ordering() {
        let mut router = PromptRouter::new();
        router.add_rule(keyword_rule(
            "code",
            5,
            RouteTarget::DirectModel("low-priority-model".to_string()),
        ));
        router.add_rule(keyword_rule(
            "code",
            20,
            RouteTarget::DirectModel("high-priority-model".to_string()),
        ));

        let decision = router.route("write some code please", None);
        match &decision.target {
            RouteTarget::DirectModel(m) => assert_eq!(m, "high-priority-model"),
            other => panic!("unexpected target: {other:?}"),
        }
    }

    #[test]
    fn and_vs_or_conditions() {
        let and_rule = RoutingRule {
            name: "and-rule".to_string(),
            priority: 10,
            conditions: vec![
                RoutingCondition::ContainsKeyword("hello".to_string()),
                RoutingCondition::ContainsKeyword("world".to_string()),
            ],
            target: RouteTarget::DirectModel("and-model".to_string()),
            match_all: true,
        };
        let or_rule = RoutingRule {
            name: "or-rule".to_string(),
            priority: 5,
            conditions: vec![
                RoutingCondition::ContainsKeyword("hello".to_string()),
                RoutingCondition::ContainsKeyword("world".to_string()),
            ],
            target: RouteTarget::DirectModel("or-model".to_string()),
            match_all: false,
        };

        let mut router = PromptRouter::new();
        router.add_rule(and_rule);
        router.add_rule(or_rule);

        // Only "hello" → AND rule should fail, OR rule should fire.
        let decision = router.route("hello there", None);
        assert_eq!(decision.rule_name, "or-rule");

        // Both keywords → AND rule fires (higher priority).
        let decision2 = router.route("hello world", None);
        assert_eq!(decision2.rule_name, "and-rule");
    }

    #[test]
    fn no_match_falls_to_default() {
        let router = PromptRouter::new();
        let decision = router.route("some random prompt", None);
        assert_eq!(decision.rule_name, "default");
        match decision.target {
            RouteTarget::DirectModel(m) => assert_eq!(m, "default"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn reject_rule() {
        let mut router = PromptRouter::new();
        router.add_rule(RoutingRule {
            name: "reject-profanity".to_string(),
            priority: 100,
            conditions: vec![RoutingCondition::ContainsKeyword("badword".to_string())],
            target: RouteTarget::Reject { reason: "content policy".to_string() },
            match_all: false,
        });

        let decision = router.route("this contains badword unfortunately", None);
        match decision.target {
            RouteTarget::Reject { reason } => assert_eq!(reason, "content policy"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn builder_api() {
        let router = PromptRouterBuilder::new()
            .named("summary-rule")
            .with_priority(10)
            .when(RoutingCondition::ContainsKeyword("summarize".to_string()))
            .route_to(RouteTarget::Template("summary".to_string()))
            .named("reject-rule")
            .with_priority(50)
            .when(RoutingCondition::ContainsKeyword("spam".to_string()))
            .route_to(RouteTarget::Reject { reason: "spam detected".to_string() })
            .build();

        let d = router.route("please summarize", None);
        assert_eq!(d.rule_name, "summary-rule");

        let d2 = router.route("buy my spam product", None);
        assert_eq!(d2.rule_name, "reject-rule");
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("hello*", "hello world"));
        assert!(glob_match("*world", "hello world"));
        assert!(glob_match("hell? world", "hello world"));
        assert!(!glob_match("hell? world", "hell world"));
        assert!(glob_match("*", "anything at all"));
    }
}

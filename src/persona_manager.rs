//! Persona management for AI assistants.
//!
//! Provides [`PersonaManager`] to register, activate, and apply distinct AI
//! personas with configurable voice, style constraints, and system prompts.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PersonaVoice
// ---------------------------------------------------------------------------

/// The conversational voice / style register for a persona.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PersonaVoice {
    /// Formal, professional language with precise vocabulary.
    Formal,
    /// Relaxed, friendly language with contractions and colloquialisms.
    Casual,
    /// Jargon-heavy, detail-oriented language suited for developers.
    Technical,
    /// Imaginative, expressive language that bends conventions.
    Creative,
    /// Warm, supportive language that acknowledges feelings first.
    Empathetic,
    /// Confident, decisive language that commands attention.
    Authoritative,
}

impl PersonaVoice {
    /// Human-readable notes about what this voice implies for generation.
    pub fn style_notes(&self) -> &str {
        match self {
            PersonaVoice::Formal => {
                "Use complete sentences, avoid contractions, prefer Latinate vocabulary, \
                 and maintain a respectful, impersonal tone."
            }
            PersonaVoice::Casual => {
                "Use contractions freely, short sentences, everyday words, \
                 and a friendly conversational tone."
            }
            PersonaVoice::Technical => {
                "Prefer precise technical terms, include code snippets where relevant, \
                 cite specifications, and assume an expert audience."
            }
            PersonaVoice::Creative => {
                "Employ vivid metaphors, varied sentence rhythm, unexpected word choices, \
                 and embrace ambiguity to spark imagination."
            }
            PersonaVoice::Empathetic => {
                "Acknowledge emotions before facts, validate the user's perspective, \
                 use 'I understand' and similar phrases, and keep a gentle pace."
            }
            PersonaVoice::Authoritative => {
                "State conclusions first, back them with evidence, avoid hedging language, \
                 and project confidence in recommendations."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PersonaConstraints
// ---------------------------------------------------------------------------

/// Behavioural guardrails for a [`Persona`].
#[derive(Debug, Clone)]
pub struct PersonaConstraints {
    /// Hard cap on the character length of any response.
    pub max_response_length: usize,
    /// Topics the persona must refuse to engage with.
    pub forbidden_topics: Vec<String>,
    /// Strings appended verbatim to every response.
    pub required_disclaimers: Vec<String>,
    /// When `true`, the persona must structure output as bullet points.
    pub always_use_bullet_points: bool,
    /// Suggested sampling temperature hint `[0.0, 1.0]`.
    pub temperature_hint: f64,
}

impl Default for PersonaConstraints {
    fn default() -> Self {
        Self {
            max_response_length: 4096,
            forbidden_topics: Vec::new(),
            required_disclaimers: Vec::new(),
            always_use_bullet_points: false,
            temperature_hint: 0.7,
        }
    }
}

// ---------------------------------------------------------------------------
// Persona
// ---------------------------------------------------------------------------

/// A fully-specified AI persona.
#[derive(Debug, Clone)]
pub struct Persona {
    /// Unique identifier used to look up the persona.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Short description of what this persona is intended for.
    pub description: String,
    /// System prompt injected before every user message.
    pub system_prompt: String,
    /// Voice / style register.
    pub voice: PersonaVoice,
    /// Behavioural constraints.
    pub constraints: PersonaConstraints,
    /// Whether this persona is currently selected as the active one.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// PersonaManager
// ---------------------------------------------------------------------------

/// Registry and runtime controller for AI personas.
///
/// # Example
/// ```
/// use tokio_prompt_orchestrator::persona_manager::{PersonaManager, default_personas};
///
/// let mut mgr = PersonaManager::default();
/// for p in default_personas() { mgr.register(p); }
/// assert!(mgr.activate("assistant"));
/// let persona = mgr.active_persona().unwrap();
/// let prompt = mgr.apply_persona("What is Rust?", persona);
/// assert!(prompt.contains("What is Rust?"));
/// ```
#[derive(Debug, Default)]
pub struct PersonaManager {
    personas: HashMap<String, Persona>,
    active_id: Option<String>,
}

impl PersonaManager {
    /// Register a new persona. If a persona with the same id already exists it
    /// is replaced.
    pub fn register(&mut self, persona: Persona) {
        self.personas.insert(persona.id.clone(), persona);
    }

    /// Activate the persona with the given `id`.
    ///
    /// Returns `true` if the persona was found and activated, `false` otherwise.
    pub fn activate(&mut self, id: &str) -> bool {
        if !self.personas.contains_key(id) {
            return false;
        }
        // Deactivate all others.
        for p in self.personas.values_mut() {
            p.active = false;
        }
        if let Some(p) = self.personas.get_mut(id) {
            p.active = true;
        }
        self.active_id = Some(id.to_string());
        true
    }

    /// Return a reference to the currently active persona, if any.
    pub fn active_persona(&self) -> Option<&Persona> {
        self.active_id
            .as_deref()
            .and_then(|id| self.personas.get(id))
    }

    /// Build the final prompt string by prepending the system context and
    /// appending required disclaimers.
    ///
    /// Format:
    /// ```text
    /// [SYSTEM]: <system_prompt>
    /// [STYLE]: <style_notes>
    ///
    /// <prompt>
    ///
    /// <disclaimer_1>
    /// <disclaimer_2>
    /// ```
    pub fn apply_persona(&self, prompt: &str, persona: &Persona) -> String {
        let mut out = String::new();
        out.push_str("[SYSTEM]: ");
        out.push_str(&persona.system_prompt);
        out.push('\n');
        out.push_str("[STYLE]: ");
        out.push_str(persona.voice.style_notes());
        out.push_str("\n\n");
        out.push_str(prompt);
        for disclaimer in &persona.constraints.required_disclaimers {
            out.push('\n');
            out.push_str(disclaimer);
        }
        out
    }

    /// Validate a model response against persona constraints.
    ///
    /// Returns a list of human-readable violation messages (empty = valid).
    pub fn validate_response(&self, response: &str, persona: &Persona) -> Vec<String> {
        let mut violations = Vec::new();

        if response.len() > persona.constraints.max_response_length {
            violations.push(format!(
                "Response length {} exceeds max_response_length {}",
                response.len(),
                persona.constraints.max_response_length
            ));
        }

        let lower = response.to_lowercase();
        for topic in &persona.constraints.forbidden_topics {
            if lower.contains(&topic.to_lowercase()) {
                violations.push(format!("Response contains forbidden topic: '{topic}'"));
            }
        }

        if persona.constraints.always_use_bullet_points {
            let has_bullets = response.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("- ") || t.starts_with("* ") || t.starts_with("• ")
            });
            if !has_bullets {
                violations.push(
                    "Response must use bullet points (always_use_bullet_points = true)".to_string(),
                );
            }
        }

        violations
    }

    /// Create a blended persona by interpolating the numeric constraints of
    /// persona `a_id` (weight `alpha`) and persona `b_id` (weight `1 - alpha`).
    ///
    /// The resulting persona's system prompt is the concatenation of both
    /// system prompts separated by a newline. The voice of persona `a` is used.
    /// Returns `None` if either id is not found or `alpha` is outside `[0, 1]`.
    pub fn blend_personas(&self, a_id: &str, b_id: &str, alpha: f64) -> Option<Persona> {
        if !(0.0..=1.0).contains(&alpha) {
            return None;
        }
        let a = self.personas.get(a_id)?;
        let b = self.personas.get(b_id)?;
        let beta = 1.0 - alpha;

        let blended_max_len = (a.constraints.max_response_length as f64 * alpha
            + b.constraints.max_response_length as f64 * beta)
            .round() as usize;
        let blended_temp = a.constraints.temperature_hint * alpha
            + b.constraints.temperature_hint * beta;

        let mut forbidden = a.constraints.forbidden_topics.clone();
        for t in &b.constraints.forbidden_topics {
            if !forbidden.contains(t) {
                forbidden.push(t.clone());
            }
        }
        let mut disclaimers = a.constraints.required_disclaimers.clone();
        for d in &b.constraints.required_disclaimers {
            if !disclaimers.contains(d) {
                disclaimers.push(d.clone());
            }
        }

        Some(Persona {
            id: format!("{a_id}_{b_id}_blend"),
            name: format!("{} + {} Blend", a.name, b.name),
            description: format!(
                "Blended persona: {} ({:.0}%) and {} ({:.0}%)",
                a.name,
                alpha * 100.0,
                b.name,
                beta * 100.0
            ),
            system_prompt: format!("{}\n{}", a.system_prompt, b.system_prompt),
            voice: a.voice.clone(),
            constraints: PersonaConstraints {
                max_response_length: blended_max_len,
                forbidden_topics: forbidden,
                required_disclaimers: disclaimers,
                always_use_bullet_points: a.constraints.always_use_bullet_points
                    || b.constraints.always_use_bullet_points,
                temperature_hint: blended_temp,
            },
            active: false,
        })
    }

    /// Return all personas that match the given voice.
    pub fn list_by_voice(&self, voice: &PersonaVoice) -> Vec<&Persona> {
        let mut result: Vec<&Persona> = self
            .personas
            .values()
            .filter(|p| &p.voice == voice)
            .collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        result
    }
}

// ---------------------------------------------------------------------------
// Pre-built personas
// ---------------------------------------------------------------------------

/// Return the standard set of pre-built personas.
///
/// Personas included:
/// - `assistant` — general-purpose formal assistant
/// - `code_helper` — technical coding assistant
/// - `creative_writer` — imaginative creative writing partner
/// - `fact_checker` — authoritative fact-checking advisor
pub fn default_personas() -> Vec<Persona> {
    vec![
        Persona {
            id: "assistant".to_string(),
            name: "Assistant".to_string(),
            description: "General-purpose helpful assistant with a formal voice.".to_string(),
            system_prompt: "You are a helpful, harmless, and honest AI assistant. \
                            Answer questions accurately and concisely."
                .to_string(),
            voice: PersonaVoice::Formal,
            constraints: PersonaConstraints {
                max_response_length: 4096,
                forbidden_topics: vec!["illegal activities".to_string()],
                required_disclaimers: vec![],
                always_use_bullet_points: false,
                temperature_hint: 0.7,
            },
            active: false,
        },
        Persona {
            id: "code_helper".to_string(),
            name: "CodeHelper".to_string(),
            description: "Expert coding assistant focused on correctness and best practices."
                .to_string(),
            system_prompt: "You are an expert software engineer. Provide correct, idiomatic code \
                            with explanations. Prefer Rust, Python, and TypeScript unless asked otherwise."
                .to_string(),
            voice: PersonaVoice::Technical,
            constraints: PersonaConstraints {
                max_response_length: 8192,
                forbidden_topics: vec![],
                required_disclaimers: vec![
                    "Note: always test generated code before deploying to production.".to_string(),
                ],
                always_use_bullet_points: false,
                temperature_hint: 0.2,
            },
            active: false,
        },
        Persona {
            id: "creative_writer".to_string(),
            name: "CreativeWriter".to_string(),
            description: "Imaginative creative writing partner.".to_string(),
            system_prompt: "You are a creative writing collaborator. Embrace metaphor, subtext, \
                            and narrative tension. Help the user craft compelling stories."
                .to_string(),
            voice: PersonaVoice::Creative,
            constraints: PersonaConstraints {
                max_response_length: 6000,
                forbidden_topics: vec!["graphic violence".to_string(), "hate speech".to_string()],
                required_disclaimers: vec![],
                always_use_bullet_points: false,
                temperature_hint: 0.9,
            },
            active: false,
        },
        Persona {
            id: "fact_checker".to_string(),
            name: "FactChecker".to_string(),
            description: "Authoritative fact-checking advisor with citations.".to_string(),
            system_prompt: "You are a rigorous fact-checker. Cite sources, distinguish between \
                            established fact and opinion, and flag uncertainty explicitly."
                .to_string(),
            voice: PersonaVoice::Authoritative,
            constraints: PersonaConstraints {
                max_response_length: 3000,
                forbidden_topics: vec![],
                required_disclaimers: vec![
                    "Disclaimer: verify all claims with primary sources.".to_string(),
                ],
                always_use_bullet_points: true,
                temperature_hint: 0.1,
            },
            active: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> PersonaManager {
        let mut mgr = PersonaManager::default();
        for p in default_personas() {
            mgr.register(p);
        }
        mgr
    }

    #[test]
    fn test_register_and_activate() {
        let mut mgr = make_manager();
        assert!(mgr.active_persona().is_none());
        assert!(mgr.activate("assistant"));
        assert_eq!(mgr.active_persona().unwrap().id, "assistant");
    }

    #[test]
    fn test_activate_unknown_returns_false() {
        let mut mgr = make_manager();
        assert!(!mgr.activate("nonexistent"));
        assert!(mgr.active_persona().is_none());
    }

    #[test]
    fn test_only_one_active_at_a_time() {
        let mut mgr = make_manager();
        mgr.activate("assistant");
        mgr.activate("code_helper");
        let active: Vec<&Persona> = mgr.personas.values().filter(|p| p.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "code_helper");
    }

    #[test]
    fn test_apply_persona_contains_system_prompt() {
        let mgr = make_manager();
        let p = mgr.personas.get("fact_checker").unwrap();
        let out = mgr.apply_persona("Is the Earth flat?", p);
        assert!(out.contains("[SYSTEM]:"));
        assert!(out.contains("Is the Earth flat?"));
        assert!(out.contains("Disclaimer: verify all claims"));
    }

    #[test]
    fn test_validate_response_length_violation() {
        let mgr = make_manager();
        let p = mgr.personas.get("fact_checker").unwrap();
        let long = "x".repeat(4000);
        let violations = mgr.validate_response(&long, p);
        assert!(violations.iter().any(|v| v.contains("exceeds max_response_length")));
    }

    #[test]
    fn test_validate_response_forbidden_topic() {
        let mgr = make_manager();
        let p = mgr.personas.get("assistant").unwrap();
        let violations = mgr.validate_response("Here is how to do illegal activities cheaply", p);
        assert!(violations.iter().any(|v| v.contains("forbidden topic")));
    }

    #[test]
    fn test_validate_response_bullet_point_violation() {
        let mgr = make_manager();
        let p = mgr.personas.get("fact_checker").unwrap();
        let violations = mgr.validate_response("The earth is round.", p);
        assert!(violations.iter().any(|v| v.contains("bullet points")));
    }

    #[test]
    fn test_validate_response_with_bullets_passes() {
        let mgr = make_manager();
        let p = mgr.personas.get("fact_checker").unwrap();
        let violations = mgr.validate_response("- The earth is an oblate spheroid.", p);
        assert!(!violations.iter().any(|v| v.contains("bullet points")));
    }

    #[test]
    fn test_blend_personas() {
        let mgr = make_manager();
        let blended = mgr.blend_personas("assistant", "code_helper", 0.5).unwrap();
        assert_eq!(blended.id, "assistant_code_helper_blend");
        // Blended max_response_length should be midpoint of 4096 and 8192.
        assert_eq!(blended.constraints.max_response_length, 6144);
        // Both system prompts should be present.
        assert!(blended.system_prompt.contains("helpful"));
        assert!(blended.system_prompt.contains("software engineer"));
    }

    #[test]
    fn test_blend_personas_invalid_alpha() {
        let mgr = make_manager();
        assert!(mgr.blend_personas("assistant", "code_helper", 1.5).is_none());
        assert!(mgr.blend_personas("assistant", "code_helper", -0.1).is_none());
    }

    #[test]
    fn test_blend_personas_unknown_id() {
        let mgr = make_manager();
        assert!(mgr.blend_personas("assistant", "unknown", 0.5).is_none());
    }

    #[test]
    fn test_list_by_voice() {
        let mgr = make_manager();
        let technical = mgr.list_by_voice(&PersonaVoice::Technical);
        assert!(technical.iter().any(|p| p.id == "code_helper"));
        // No other default persona has Technical voice.
        assert_eq!(technical.len(), 1);
    }

    #[test]
    fn test_style_notes_non_empty() {
        for voice in [
            PersonaVoice::Formal,
            PersonaVoice::Casual,
            PersonaVoice::Technical,
            PersonaVoice::Creative,
            PersonaVoice::Empathetic,
            PersonaVoice::Authoritative,
        ] {
            assert!(!voice.style_notes().is_empty());
        }
    }
}

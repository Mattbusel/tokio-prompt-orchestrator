//! Multi-modal content handling for prompts and responses.
//!
//! [`MultiModalContent`] holds an ordered sequence of [`ContentPart`] values
//! that can represent text, images, code snippets, data tables, and audio
//! transcripts. A fluent [`ContentBuilder`] and a JSON
//! [`ContentSerializer`] are also provided.

use serde::{Deserialize, Serialize};

/// A single piece of content within a multi-modal message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text(String),

    /// An image referenced by URL.
    ImageUrl {
        /// Fully-qualified image URL.
        url: String,
        /// Optional alt-text description.
        alt_text: Option<String>,
        /// Optional pixel width.
        width: Option<u32>,
        /// Optional pixel height.
        height: Option<u32>,
    },

    /// A fenced code block with optional filename metadata.
    CodeSnippet {
        /// Programming language identifier (e.g. `"rust"`, `"python"`).
        language: String,
        /// The source code.
        code: String,
        /// Optional source file name.
        filename: Option<String>,
    },

    /// A tabular dataset with headers and rows.
    DataTable {
        /// Column header labels.
        headers: Vec<String>,
        /// Data rows; each row is a vector of string cells.
        rows: Vec<Vec<String>>,
    },

    /// A transcribed audio segment.
    AudioTranscript {
        /// Transcribed text.
        text: String,
        /// BCP-47 language tag (e.g. `"en-US"`).
        language: String,
        /// Transcription confidence in `[0.0, 1.0]`.
        confidence: f64,
    },
}

/// An ordered collection of [`ContentPart`] values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiModalContent {
    /// Ordered parts comprising this message.
    pub parts: Vec<ContentPart>,
}

impl MultiModalContent {
    /// Create an empty content object.
    pub fn new() -> Self {
        Self::default()
    }

    /// Concatenate all [`ContentPart::Text`] and [`ContentPart::AudioTranscript`]
    /// parts into a single string separated by spaces.
    pub fn text_only(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                ContentPart::AudioTranscript { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Rough token estimate for the entire content.
    ///
    /// - **Text / AudioTranscript**: `word_count / 0.75` (≈ 1.33 tokens/word)
    /// - **ImageUrl**: 85 tokens (fixed OpenAI estimate)
    /// - **CodeSnippet**: `code.len() / 4` (≈ 4 chars/token)
    /// - **DataTable**: sum of all cell lengths divided by 4
    pub fn token_estimate(&self) -> usize {
        self.parts.iter().map(|p| match p {
            ContentPart::Text(t) => {
                let words = t.split_whitespace().count();
                ((words as f64) / 0.75).ceil() as usize
            }
            ContentPart::AudioTranscript { text, .. } => {
                let words = text.split_whitespace().count();
                ((words as f64) / 0.75).ceil() as usize
            }
            ContentPart::ImageUrl { .. } => 85,
            ContentPart::CodeSnippet { code, .. } => (code.len() / 4).max(1),
            ContentPart::DataTable { headers, rows } => {
                let header_chars: usize = headers.iter().map(|h| h.len()).sum();
                let row_chars: usize = rows.iter().flat_map(|r| r.iter()).map(|c| c.len()).sum();
                ((header_chars + row_chars) / 4).max(1)
            }
        }).sum()
    }

    /// Returns `true` if any part is an [`ContentPart::ImageUrl`].
    pub fn has_images(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. }))
    }

    /// Returns `true` if any part is a [`ContentPart::CodeSnippet`].
    pub fn has_code(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, ContentPart::CodeSnippet { .. }))
    }

    /// Return references to all [`ContentPart::DataTable`] parts.
    pub fn tables(&self) -> Vec<(&Vec<String>, &Vec<Vec<String>>)> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::DataTable { headers, rows } => Some((headers, rows)),
                _ => None,
            })
            .collect()
    }
}

/// Fluent builder for [`MultiModalContent`].
///
/// # Example
/// ```
/// use tokio_prompt_orchestrator::multi_modal::ContentBuilder;
///
/// let content = ContentBuilder::new()
///     .add_text("Hello!")
///     .add_image("https://example.com/img.png", Some("a cat"), None, None)
///     .build();
///
/// assert!(content.has_images());
/// ```
#[derive(Debug, Default)]
pub struct ContentBuilder {
    parts: Vec<ContentPart>,
}

impl ContentBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a plain-text part.
    pub fn add_text(mut self, text: impl Into<String>) -> Self {
        self.parts.push(ContentPart::Text(text.into()));
        self
    }

    /// Append an image-URL part.
    pub fn add_image(
        mut self,
        url: impl Into<String>,
        alt_text: Option<impl Into<String>>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Self {
        self.parts.push(ContentPart::ImageUrl {
            url: url.into(),
            alt_text: alt_text.map(|a| a.into()),
            width,
            height,
        });
        self
    }

    /// Append a code-snippet part.
    pub fn add_code(
        mut self,
        language: impl Into<String>,
        code: impl Into<String>,
        filename: Option<impl Into<String>>,
    ) -> Self {
        self.parts.push(ContentPart::CodeSnippet {
            language: language.into(),
            code: code.into(),
            filename: filename.map(|f| f.into()),
        });
        self
    }

    /// Append a data-table part.
    pub fn add_table(
        mut self,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    ) -> Self {
        self.parts.push(ContentPart::DataTable { headers, rows });
        self
    }

    /// Append an audio-transcript part.
    pub fn add_audio(
        mut self,
        text: impl Into<String>,
        language: impl Into<String>,
        confidence: f64,
    ) -> Self {
        self.parts.push(ContentPart::AudioTranscript {
            text: text.into(),
            language: language.into(),
            confidence,
        });
        self
    }

    /// Consume the builder and return the [`MultiModalContent`].
    pub fn build(self) -> MultiModalContent {
        MultiModalContent { parts: self.parts }
    }
}

/// Serialize and deserialize [`MultiModalContent`] to/from JSON strings.
///
/// Uses `serde_json` which is already a dependency of this crate.
pub struct ContentSerializer;

impl ContentSerializer {
    /// Serialize `content` to a compact JSON string.
    ///
    /// Returns an error string if serialization fails (this should not happen
    /// for well-formed content).
    pub fn serialize(content: &MultiModalContent) -> Result<String, String> {
        serde_json::to_string(content).map_err(|e| e.to_string())
    }

    /// Deserialize a [`MultiModalContent`] from a JSON string previously
    /// produced by [`ContentSerializer::serialize`].
    pub fn deserialize(json: &str) -> Result<MultiModalContent, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ContentPart variants ──────────────────────────────────────────────────

    #[test]
    fn text_only_collects_text_and_audio() {
        let content = ContentBuilder::new()
            .add_text("Hello")
            .add_image("https://x.com/img.png", None::<String>, None, None)
            .add_audio("world", "en-US", 0.99)
            .build();
        assert_eq!(content.text_only(), "Hello world");
    }

    #[test]
    fn text_only_empty_when_no_text_parts() {
        let content = ContentBuilder::new()
            .add_image("https://x.com/img.png", None::<String>, None, None)
            .build();
        assert_eq!(content.text_only(), "");
    }

    // ── has_images / has_code ─────────────────────────────────────────────────

    #[test]
    fn has_images_true_when_image_present() {
        let c = ContentBuilder::new()
            .add_image("https://x.com/a.png", None::<String>, None, None)
            .build();
        assert!(c.has_images());
    }

    #[test]
    fn has_images_false_when_no_images() {
        let c = ContentBuilder::new().add_text("hello").build();
        assert!(!c.has_images());
    }

    #[test]
    fn has_code_true_when_code_present() {
        let c = ContentBuilder::new()
            .add_code("rust", "fn main() {}", None::<String>)
            .build();
        assert!(c.has_code());
    }

    #[test]
    fn has_code_false_when_no_code() {
        let c = ContentBuilder::new().add_text("hello").build();
        assert!(!c.has_code());
    }

    // ── tables ────────────────────────────────────────────────────────────────

    #[test]
    fn tables_returns_all_data_tables() {
        let c = ContentBuilder::new()
            .add_text("intro")
            .add_table(
                vec!["Name".into(), "Age".into()],
                vec![vec!["Alice".into(), "30".into()]],
            )
            .add_table(
                vec!["X".into()],
                vec![],
            )
            .build();
        assert_eq!(c.tables().len(), 2);
    }

    #[test]
    fn tables_empty_when_no_tables() {
        let c = ContentBuilder::new().add_text("hello").build();
        assert!(c.tables().is_empty());
    }

    // ── token_estimate ────────────────────────────────────────────────────────

    #[test]
    fn token_estimate_text() {
        // "one two three four" → 4 words → ceil(4 / 0.75) = 6
        let c = ContentBuilder::new()
            .add_text("one two three four")
            .build();
        assert_eq!(c.token_estimate(), 6);
    }

    #[test]
    fn token_estimate_image() {
        let c = ContentBuilder::new()
            .add_image("https://x.com/img.png", None::<String>, None, None)
            .build();
        assert_eq!(c.token_estimate(), 85);
    }

    #[test]
    fn token_estimate_code() {
        // code with 8 chars → 8 / 4 = 2 tokens
        let c = ContentBuilder::new()
            .add_code("python", "12345678", None::<String>)
            .build();
        assert_eq!(c.token_estimate(), 2);
    }

    #[test]
    fn token_estimate_combined() {
        let c = ContentBuilder::new()
            .add_text("hello world")  // 2 words → ceil(2/0.75) = 3
            .add_image("https://x.com/a.png", None::<String>, None, None) // 85
            .build();
        assert_eq!(c.token_estimate(), 3 + 85);
    }

    // ── ContentBuilder ────────────────────────────────────────────────────────

    #[test]
    fn builder_produces_correct_part_count() {
        let c = ContentBuilder::new()
            .add_text("t")
            .add_image("u", None::<String>, Some(800), Some(600))
            .add_code("rs", "fn f() {}", Some("lib.rs"))
            .add_table(vec!["H".into()], vec![])
            .add_audio("transcript", "en", 0.9)
            .build();
        assert_eq!(c.parts.len(), 5);
    }

    #[test]
    fn builder_image_stores_dimensions() {
        let c = ContentBuilder::new()
            .add_image("https://x.com/a.png", Some("alt"), Some(1920), Some(1080))
            .build();
        if let ContentPart::ImageUrl { width, height, alt_text, .. } = &c.parts[0] {
            assert_eq!(*width, Some(1920));
            assert_eq!(*height, Some(1080));
            assert_eq!(alt_text.as_deref(), Some("alt"));
        } else {
            panic!("expected ImageUrl part");
        }
    }

    // ── ContentSerializer ─────────────────────────────────────────────────────

    #[test]
    fn serialize_deserialize_roundtrip() {
        let original = ContentBuilder::new()
            .add_text("Hello world")
            .add_image("https://x.com/img.png", Some("desc"), Some(640), Some(480))
            .add_code("rust", "fn main() {}", Some("main.rs"))
            .add_table(
                vec!["Col1".into(), "Col2".into()],
                vec![vec!["A".into(), "B".into()]],
            )
            .add_audio("audio text", "en-GB", 0.95)
            .build();

        let json = ContentSerializer::serialize(&original).expect("serialize must succeed");
        let recovered = ContentSerializer::deserialize(&json).expect("deserialize must succeed");
        assert_eq!(original, recovered);
    }

    #[test]
    fn deserialize_invalid_json_returns_error() {
        assert!(ContentSerializer::deserialize("{not valid json}").is_err());
    }

    #[test]
    fn serialize_empty_content() {
        let c = MultiModalContent::new();
        let json = ContentSerializer::serialize(&c).expect("serialize must succeed");
        let back = ContentSerializer::deserialize(&json).expect("deserialize must succeed");
        assert_eq!(c, back);
    }
}

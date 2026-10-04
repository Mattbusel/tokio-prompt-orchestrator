//! Retrieval for the pipeline's first stage: ground prompts in your own
//! documents.
//!
//! With a [`Retriever`] (pass it in [`PipelineOptions`](crate::PipelineOptions)),
//! the Retrieve stage looks up the passages most relevant to each prompt and
//! the Assemble stage puts them in front of the question, so the model
//! answers from your material. Without one, prompts reach the model exactly
//! as written.
//!
//! Implementations:
//!
//! | Feature | Type | Searches |
//! |---|---|---|
//! | `tantivy` | `TantivyRetriever` | a folder of Markdown and text files, with BM25 full-text search, in memory |
//!
//! Or implement the trait over your own search: a vector database, Elasticsearch,
//! Postgres full-text search, an internal API.

use async_trait::async_trait;

use crate::OrchestratorError;

/// One retrieved piece of text.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// The text put in front of the question.
    pub text: String,
    /// Where it came from (a file path, URL or document id), shown to the
    /// model so it can cite it.
    pub source: Option<String>,
    /// Relevance score from the retriever; higher is more relevant. Only
    /// comparable between passages from the same retriever.
    pub score: f32,
}

/// Finds the passages most relevant to a prompt.
#[async_trait]
pub trait Retriever: Send + Sync + std::fmt::Debug {
    /// Up to `limit` passages for `query`, most relevant first. An empty
    /// result is fine: the prompt is then sent without context.
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<Passage>, OrchestratorError>;
}

/// The context block the Assemble stage puts before the question: numbered
/// passages with their sources, so the model can cite them.
///
/// ```
/// use tokio_prompt_orchestrator::retrieval::{format_context, Passage};
///
/// let ctx = format_context(&[Passage {
///     text: "Refunds are issued within 14 days.".into(),
///     source: Some("policy.md".into()),
///     score: 3.2,
/// }]);
/// assert_eq!(ctx, "[1] (policy.md) Refunds are issued within 14 days.");
/// ```
pub fn format_context(passages: &[Passage]) -> String {
    passages
        .iter()
        .enumerate()
        .map(|(i, p)| match &p.source {
            Some(source) => format!("[{}] ({}) {}", i + 1, source, p.text.trim()),
            None => format!("[{}] {}", i + 1, p.text.trim()),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The prompt the model receives when retrieval found context.
pub(crate) fn grounded_prompt(context: &str, question: &str) -> String {
    format!(
        "Use the following excerpts to answer. If they do not contain the answer, say so.\n\n{context}\n\nQuestion: {question}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_numbers_passages_and_keeps_sources() {
        let ctx = format_context(&[
            Passage {
                text: " a \n".into(),
                source: Some("x.md".into()),
                score: 2.0,
            },
            Passage {
                text: "b".into(),
                source: None,
                score: 1.0,
            },
        ]);
        assert_eq!(ctx, "[1] (x.md) a\n\n[2] b");
    }

    #[test]
    fn grounded_prompt_ends_with_the_question() {
        let p = grounded_prompt("[1] a", "What is a?");
        assert!(p.starts_with("Use the following excerpts"));
        assert!(p.ends_with("Question: What is a?"));
    }
}

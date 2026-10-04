//! [`Retriever`] over a folder of documents, with [tantivy](https://docs.rs/tantivy)
//! BM25 full-text search.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED,
};
use tantivy::{doc, Index, IndexReader, TantivyDocument};

use crate::retrieval::{Passage, Retriever};
use crate::OrchestratorError;

/// File extensions [`TantivyRetriever::index_dir`] reads.
pub const INDEXED_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "rst", "text", "adoc", "org"];

/// Target passage length in characters. Paragraphs are merged up to this
/// size so each passage carries enough context to answer from.
const PASSAGE_CHARS: usize = 900;

/// Searches a set of documents with BM25 (the ranking used by Elasticsearch
/// and Lucene), with English stemming so "refunds" finds "refund".
///
/// Documents are split into passages of a few paragraphs and indexed in
/// memory; nothing is written to disk. Suited to a folder of notes, docs or
/// policies (up to tens of thousands of passages). For larger corpora or
/// meaning-based search, implement [`Retriever`] over a search service.
///
/// ```no_run
/// use std::sync::Arc;
/// use tokio_prompt_orchestrator::{
///     integrations::TantivyRetriever, spawn_pipeline_with, EchoWorker, PipelineOptions,
/// };
///
/// # fn main() -> Result<(), tokio_prompt_orchestrator::OrchestratorError> {
/// # tokio_test::block_on(async {
/// let docs = TantivyRetriever::index_dir("./docs")?;
/// println!("indexed {} passages", docs.passage_count());
/// let handles = spawn_pipeline_with(
///     Arc::new(EchoWorker::new()),
///     PipelineOptions::with_retriever(Arc::new(docs)),
/// );
/// # Ok::<_, tokio_prompt_orchestrator::OrchestratorError>(())
/// # })?;
/// # Ok(())
/// # }
/// ```
///
/// Requires the `tantivy` feature (Rust 1.90 or newer).
#[derive(Clone)]
pub struct TantivyRetriever {
    index: Index,
    reader: IndexReader,
    body: Field,
    source: Field,
    passages: usize,
}

impl std::fmt::Debug for TantivyRetriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyRetriever")
            .field("passages", &self.passages)
            .finish_non_exhaustive()
    }
}

fn index_error(e: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::ConfigError(format!("tantivy: {e}"))
}

impl TantivyRetriever {
    /// Index every Markdown and text file under `dir` (recursively; see
    /// [`INDEXED_EXTENSIONS`]). Hidden directories and `target`/`node_modules`
    /// are skipped.
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] if `dir` cannot be read or the
    /// index cannot be built.
    pub fn index_dir(dir: impl AsRef<Path>) -> Result<Self, OrchestratorError> {
        let dir = dir.as_ref();
        let mut files = Vec::new();
        collect_files(dir, &mut files).map_err(|e| {
            OrchestratorError::ConfigError(format!("reading {}: {e}", dir.display()))
        })?;
        files.sort();
        let mut documents = Vec::with_capacity(files.len());
        for path in files {
            // Unreadable or non-UTF-8 files are skipped, not fatal.
            if let Ok(text) = std::fs::read_to_string(&path) {
                let name = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                documents.push((name.replace('\\', "/"), text));
            }
        }
        Self::from_documents(documents)
    }

    /// Index `(source, text)` pairs you already have in memory.
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] if the index cannot be built.
    pub fn from_documents<S, T>(
        documents: impl IntoIterator<Item = (S, T)>,
    ) -> Result<Self, OrchestratorError>
    where
        S: Into<String>,
        T: AsRef<str>,
    {
        let mut schema = Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_tokenizer("en_stem")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let body = schema.add_text_field(
            "body",
            TextOptions::default()
                .set_indexing_options(indexing)
                .set_stored(),
        );
        let source = schema.add_text_field("source", STORED);
        let index = Index::create_in_ram(schema.build());
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, 50_000_000)
            .map_err(index_error)?;

        let mut passages = 0;
        for (name, text) in documents {
            let name = name.into();
            for passage in split_passages(text.as_ref()) {
                writer
                    .add_document(doc!(body => passage, source => name.clone()))
                    .map_err(index_error)?;
                passages += 1;
            }
        }
        writer.commit().map_err(index_error)?;
        let reader = index.reader().map_err(index_error)?;
        reader.reload().map_err(index_error)?;
        Ok(Self {
            index,
            reader,
            body,
            source,
            passages,
        })
    }

    /// How many passages are indexed.
    pub fn passage_count(&self) -> usize {
        self.passages
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Passage>, OrchestratorError> {
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.body]);
        // Lenient: a prompt is free text, so quotes, colons and question
        // marks must not be read as query syntax errors.
        let (query, _errors) = parser.parse_query_lenient(query);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|e| OrchestratorError::Other(format!("tantivy search: {e}")))?;
        let mut passages = Vec::with_capacity(hits.len());
        for (score, address) in hits {
            let doc: TantivyDocument = searcher
                .doc(address)
                .map_err(|e| OrchestratorError::Other(format!("tantivy doc: {e}")))?;
            let text = doc
                .get_first(self.body)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let source = doc.get_first(self.source).and_then(|v| v.as_str());
            passages.push(Passage {
                text: text.to_string(),
                source: source.map(str::to_string),
                score,
            });
        }
        Ok(passages)
    }
}

#[async_trait]
impl Retriever for TantivyRetriever {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<Passage>, OrchestratorError> {
        let this = self.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || this.search(&query, limit))
            .await
            .map_err(|e| OrchestratorError::Other(format!("search task failed: {e}")))?
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_files(&path, out)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| INDEXED_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Split a document into passages: paragraphs (blank-line separated) merged
/// up to about [`PASSAGE_CHARS`]; a single longer paragraph stays whole.
fn split_passages(text: &str) -> Vec<String> {
    let mut passages = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if !current.is_empty() && current.len() + paragraph.len() + 2 > PASSAGE_CHARS {
            passages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.is_empty() {
        passages.push(current);
    }
    passages
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn corpus() -> TantivyRetriever {
        TantivyRetriever::from_documents([
            ("refunds.md", "# Refunds\n\nRefunds are issued within 14 days of a return.\n\nGift cards cannot be refunded."),
            ("shipping.md", "# Shipping\n\nOrders ship in 2 business days. Express shipping costs $12."),
            ("security.md", "# Passwords\n\nPasswords must be at least 12 characters and are reset by email."),
        ])
        .unwrap()
    }

    #[tokio::test]
    async fn finds_the_relevant_document_first() {
        let hits = corpus()
            .retrieve("How long do refunds take?", 3)
            .await
            .unwrap();
        assert_eq!(hits[0].source.as_deref(), Some("refunds.md"));
        assert!(hits[0].text.contains("14 days"));
    }

    #[tokio::test]
    async fn stemming_matches_word_forms() {
        // "shipped" and "ship" share a stem.
        let hits = corpus()
            .retrieve("when is my order shipped", 1)
            .await
            .unwrap();
        assert_eq!(hits[0].source.as_deref(), Some("shipping.md"));
    }

    #[tokio::test]
    async fn prompt_punctuation_is_not_query_syntax() {
        let hits = corpus()
            .retrieve(r#"password: "reset"? (AND) OR -- [x]"#, 2)
            .await
            .unwrap();
        assert_eq!(hits[0].source.as_deref(), Some("security.md"));
    }

    #[tokio::test]
    async fn unrelated_query_returns_nothing() {
        assert!(corpus()
            .retrieve("quantum chromodynamics", 3)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn passages_merge_short_paragraphs_and_split_long_documents() {
        let para = "word ".repeat(100); // 500 chars
        let text = format!("{para}\n\n{para}\n\nshort");
        let parts = split_passages(&text);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[1].ends_with("short"));
    }

    #[test]
    fn index_dir_reads_markdown_recursively_and_skips_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("a.md"), "alpha").unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "beta").unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join(".git/d.md"), "hidden").unwrap();
        let r = TantivyRetriever::index_dir(dir.path()).unwrap();
        assert_eq!(r.passage_count(), 2);
    }
}

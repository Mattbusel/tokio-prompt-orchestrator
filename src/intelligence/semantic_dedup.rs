#![cfg(feature = "intelligence")]
//! Backward-compatible re-export of the lexical deduplication module.
//!
//! This module has been superseded by [`super::lexical_dedup`].  All public
//! types are re-exported here so that existing `use intelligence::semantic_dedup::*`
//! imports continue to compile without changes.
//!
//! **Prefer `intelligence::lexical_dedup`** for new code.  The struct
//! `SemanticDedup` is now a deprecated type alias for `LexicalDedup`.

pub use super::lexical_dedup::{
    EmbeddingEntry, LexicalDedup, SemanticDedupConfig, SemanticDedupError, SimilarityMatch,
};

/// Deprecated type alias — use [`LexicalDedup`] instead.
#[deprecated(note = "Renamed to LexicalDedup in intelligence::lexical_dedup")]
pub type SemanticDedup = LexicalDedup;

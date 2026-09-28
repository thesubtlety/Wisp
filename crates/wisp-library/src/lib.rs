//! SQLite-backed meeting knowledge base for Wisp.
//!
//! Persists each finished meeting (metadata + finalized transcript segments) and indexes it for
//! full-text search. SQLite is the source of truth; Markdown is rendered on demand from the same
//! [`wisp_core`] domain types (via [`wisp_core::export::format_markdown`]). The store is engine- and
//! shell-agnostic — the desktop app is one caller, a CLI or sync agent could be another.
//!
//! Full-text search is always on; semantic and hybrid search are optional, enabled by configuring
//! an [`Embedder`]. [`Library::retrieve`] returns small, addressable snippets from a project's
//! sources and meetings for reasoning.

mod embed;
mod record;
mod retention;
mod retrieve;
mod store;

pub use embed::Embedder;
pub use record::{Note, NoteSummary, Project, SearchHit, Segment, Source};
pub use retention::{
    PruneReport, RetentionPolicy, SourceInput, SourceKind, Upsert, DEFAULT_TEMP_SOURCE_DAYS,
    DEFAULT_TRANSCRIPT_DAYS,
};
pub use retrieve::{
    meeting_ref, source_ref, RetrievalQuery, Snippet, SnippetOrigin, SNIPPET_CHARS,
};
pub use store::Library;

/// An error from the meeting library.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// The underlying SQLite database returned an error.
    #[error("library database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// An [`Embedder`] failed to produce vectors (model load or inference error).
    #[error("embedding error: {0}")]
    Embed(String),
}

/// Result of a library operation.
pub type Result<T> = std::result::Result<T, LibraryError>;

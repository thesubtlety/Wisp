//! Plain data records returned by the [`crate::Library`]. They derive `Serialize`/`Deserialize` so
//! the Tauri shell can hand them straight to the UI without a second set of DTOs.

use serde::{Deserialize, Serialize};

/// A stored meeting's metadata (one `meeting` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    /// Session start, epoch milliseconds. The shell formats it for display.
    pub started_at_ms: i64,
    /// Length of the meeting in milliseconds (latest segment end).
    pub duration_ms: i64,
    pub language: Option<String>,
    /// The transcription model/engine used.
    pub engine: Option<String>,
    /// An AI-generated summary, when one was produced. Kept when the transcript expires.
    pub summary: Option<String>,
    pub segment_count: i64,
    /// The project this meeting belongs to, if any.
    pub project_id: Option<String>,
    /// When the transcript expires (epoch ms); `None` keeps it.
    pub transcript_expires_at_ms: Option<i64>,
    /// When the transcript was deleted by retention (epoch ms); the meeting and summary remain.
    pub transcript_pruned_at_ms: Option<i64>,
}

/// A project: a named scope for meetings and their source documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
}

/// A project source document (one `source` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub project_id: String,
    /// `"file"` (indexed where it lives), `"pasted"`, or `"managed"` (an app-owned copy).
    pub kind: String,
    /// What the UI shows: a file name or a short title.
    pub label: String,
    /// The user's own file, when indexed in place. Never modified or deleted.
    pub origin_path: Option<String>,
    /// The app-managed copy, deleted when the source expires or is removed.
    pub managed_path: Option<String>,
    pub sha256: String,
    pub added_at_ms: i64,
    /// When the source (and its chunks and vectors) expires; `None` keeps it.
    pub expires_at_ms: Option<i64>,
    pub chunk_count: i64,
}

/// A finalized transcript line within a meeting (one `segment` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Position within the meeting (0-based).
    pub idx: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Diarized speaker id, when assigned.
    pub speaker: Option<i64>,
    /// Audio source: `"mic"`, `"system"`, `"file"`, or `"unknown"`.
    pub source: String,
    pub text: String,
}

/// A row in the Library list: a meeting plus a short transcript preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub started_at_ms: i64,
    pub duration_ms: i64,
    pub language: Option<String>,
    pub engine: Option<String>,
    /// First stretch of transcript, for the list card.
    pub preview: String,
}

/// A full-text search result — one per matching meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub meeting_id: String,
    pub title: String,
    pub started_at_ms: i64,
    /// The best-matching transcript snippet, with matched terms wrapped in `«…»`.
    pub snippet: String,
    /// FTS5 BM25 relevance (more negative = better); exposed so the shell can merge/sort across modes.
    pub score: f64,
}

//! Projects, project sources, and retention.
//!
//! Every transcript and temporary source gets an expiry when it's written. [`Library::prune`]
//! deletes what has expired for real: rows, their full-text entries, their vectors, and any
//! app-managed file (but only inside the managed directory the caller names). A meeting whose
//! transcript expires keeps its row, title and summary. Files the user indexed in place are never
//! touched. The library stays clock-free: callers pass the current time.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::embed;
use crate::record::{Project, Source};
use crate::store::{Library, SOURCE_CHUNK_CHARS};
use crate::Result;

/// How long a meeting transcript is kept, by default.
pub const DEFAULT_TRANSCRIPT_DAYS: u32 = 90;
/// How long pasted text and app-managed copies are kept, by default.
pub const DEFAULT_TEMP_SOURCE_DAYS: u32 = 30;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How long each class of data is kept. `None` keeps it until deleted by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub transcript_days: Option<u32>,
    pub temp_source_days: Option<u32>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            transcript_days: Some(DEFAULT_TRANSCRIPT_DAYS),
            temp_source_days: Some(DEFAULT_TEMP_SOURCE_DAYS),
        }
    }
}

impl RetentionPolicy {
    /// When a transcript for a meeting that started at `started_at_ms` expires.
    pub fn transcript_expiry(&self, started_at_ms: i64) -> Option<i64> {
        self.transcript_days
            .map(|d| started_at_ms.saturating_add(i64::from(d) * DAY_MS))
    }

    /// When a source of `kind` added at `added_at_ms` expires. Files indexed in place don't.
    pub fn source_expiry(&self, kind: SourceKind, added_at_ms: i64) -> Option<i64> {
        match kind {
            SourceKind::File => None,
            SourceKind::Pasted | SourceKind::Managed | SourceKind::Screenshot => self
                .temp_source_days
                .map(|d| added_at_ms.saturating_add(i64::from(d) * DAY_MS)),
        }
    }
}

/// Where a source's text came from, which decides its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A file in the user's own folders, indexed where it lives. Never modified or deleted.
    File,
    /// Text pasted into the app. Temporary.
    Pasted,
    /// A copy the app owns (an import). Temporary; the file is deleted too.
    Managed,
    /// A screenshot the user captured or pasted as context: the app-owned image, indexed by its
    /// description. Temporary like other copies; the image is deleted too.
    Screenshot,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::File => "file",
            SourceKind::Pasted => "pasted",
            SourceKind::Managed => "managed",
            SourceKind::Screenshot => "screenshot",
        }
    }
}

/// A source to add to a project.
#[derive(Debug, Clone)]
pub struct SourceInput {
    pub kind: SourceKind,
    pub label: String,
    /// The extracted text to index.
    pub text: String,
    /// The user's file, for [`SourceKind::File`].
    pub origin_path: Option<PathBuf>,
    /// The app-owned copy, for [`SourceKind::Managed`].
    pub managed_path: Option<PathBuf>,
    pub added_at_ms: i64,
}

/// What [`Library::upsert_file_source`] did, with the source's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Added(i64),
    Updated(i64),
    Unchanged(i64),
}

/// What a [`Library::prune`] (or a project deletion) removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Meetings whose transcript (segments, search index, vectors) was deleted.
    pub transcripts_expired: usize,
    /// Sources deleted, with their chunks and vectors.
    pub sources_expired: usize,
    /// Meetings deleted outright (project deletion only).
    pub meetings_deleted: usize,
    /// App-managed files deleted.
    pub files_deleted: usize,
    /// Managed paths left alone because they weren't inside the managed directory.
    pub files_refused: Vec<PathBuf>,
}

impl Library {
    /// Replaces the retention policy used for data written from now on.
    pub fn set_retention(&mut self, policy: RetentionPolicy) {
        self.retention = policy;
    }

    /// The retention policy in effect.
    pub fn retention(&self) -> RetentionPolicy {
        self.retention
    }

    /// Adopts `policy` for existing data too: every transcript not yet pruned gets its expiry
    /// recomputed from the meeting's start (or cleared, to keep it), and every pasted or managed
    /// source from when it was added. Files indexed in place never expire. Nothing is deleted
    /// here; the next [`Library::prune`] does that. Returns (meetings, sources) updated.
    pub fn restamp_expiries(&mut self, policy: RetentionPolicy) -> Result<(usize, usize)> {
        self.retention = policy;
        let transcript_ms = policy.transcript_days.map(|d| i64::from(d) * DAY_MS);
        let source_ms = policy.temp_source_days.map(|d| i64::from(d) * DAY_MS);
        let tx = self.conn.transaction()?;
        let meetings = tx.execute(
            "UPDATE meeting SET transcript_expires_at_ms =
                 CASE WHEN ?1 IS NULL THEN NULL ELSE started_at_ms + ?1 END
             WHERE transcript_pruned_at_ms IS NULL",
            [transcript_ms],
        )?;
        let sources = tx.execute(
            "UPDATE source SET expires_at_ms =
                 CASE WHEN ?1 IS NULL THEN NULL ELSE added_at_ms + ?1 END
             WHERE kind IN ('pasted', 'managed', 'screenshot')",
            [source_ms],
        )?;
        tx.commit()?;
        Ok((meetings, sources))
    }

    /// How many transcripts and temporary sources `policy` would have expired by `now_ms`: what a
    /// prune right after adopting it would delete. For confirming before shortening retention.
    pub fn preview_policy(&self, policy: RetentionPolicy, now_ms: i64) -> Result<(usize, usize)> {
        let transcripts = match policy.transcript_days {
            None => 0,
            Some(d) => self.conn.query_row(
                "SELECT count(*) FROM meeting
                 WHERE transcript_pruned_at_ms IS NULL AND segment_count > 0
                   AND started_at_ms + ?1 <= ?2",
                rusqlite::params![i64::from(d) * DAY_MS, now_ms],
                |r| r.get::<_, i64>(0),
            )? as usize,
        };
        let sources = match policy.temp_source_days {
            None => 0,
            Some(d) => self.conn.query_row(
                "SELECT count(*) FROM source
                 WHERE kind IN ('pasted', 'managed', 'screenshot') AND added_at_ms + ?1 <= ?2",
                rusqlite::params![i64::from(d) * DAY_MS, now_ms],
                |r| r.get::<_, i64>(0),
            )? as usize,
        };
        Ok((transcripts, sources))
    }

    /// Creates a project. `id` is caller-generated; names are unique.
    pub fn create_project(&self, id: &str, name: &str, created_at_ms: i64) -> Result<Project> {
        let name = name.trim();
        self.conn.execute(
            "INSERT INTO project (id, name, created_at_ms) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, created_at_ms],
        )?;
        Ok(Project {
            id: id.to_owned(),
            name: name.to_owned(),
            created_at_ms,
        })
    }

    /// Every project, by name.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, created_at_ms FROM project ORDER BY name COLLATE NOCASE")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    created_at_ms: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Renames a project (trimmed). Names stay unique, so a taken name is an error. Returns whether
    /// the project exists.
    pub fn rename_project(&self, project_id: &str, name: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE project SET name = ?2 WHERE id = ?1",
            rusqlite::params![project_id, name.trim()],
        )?;
        Ok(n > 0)
    }

    /// A project's instructions: what matters to the user there. `None` when the project doesn't
    /// exist; empty when none were written.
    pub fn project_instructions(&self, project_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT instructions FROM project WHERE id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Replaces a project's instructions (trimmed; empty clears them). Returns whether the project
    /// exists.
    pub fn set_project_instructions(&self, project_id: &str, text: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE project SET instructions = ?2 WHERE id = ?1",
            rusqlite::params![project_id, text.trim()],
        )?;
        Ok(n > 0)
    }

    /// Retitles a meeting (trimmed). Search reads titles from the meeting row, so hits follow.
    /// Returns whether the meeting exists.
    pub fn rename_meeting(&self, meeting_id: &str, title: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE meeting SET title = ?2 WHERE id = ?1",
            rusqlite::params![meeting_id, title.trim()],
        )?;
        Ok(n > 0)
    }

    /// Retitles a meeting only if its title is still `expected`, so a late automatic title never
    /// replaces one the user just typed. Returns whether it changed.
    pub fn rename_meeting_if(&self, meeting_id: &str, title: &str, expected: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE meeting SET title = ?2 WHERE id = ?1 AND title = ?3",
            rusqlite::params![meeting_id, title.trim(), expected],
        )?;
        Ok(n > 0)
    }

    /// Moves a meeting into `project_id`, or out of any project with `None`. Returns whether the
    /// meeting exists.
    pub fn set_meeting_project(&self, meeting_id: &str, project_id: Option<&str>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE meeting SET project_id = ?2 WHERE id = ?1",
            rusqlite::params![meeting_id, project_id],
        )?;
        Ok(n > 0)
    }

    /// Adds a source to a project: stores it, chunks its text for search, and embeds the chunks when
    /// an embedder is set. Its expiry follows the [`RetentionPolicy`] for its kind. Returns its id.
    pub fn add_source(&mut self, project_id: &str, input: &SourceInput) -> Result<i64> {
        let chunks = embed::chunk_document(&input.text, SOURCE_CHUNK_CHARS);
        // Embed before the transaction — inference touches no DB and may be slow.
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let vectors = self.embed_exact(&texts)?;
        let sha256 = hex(&Sha256::digest(input.text.as_bytes()));
        let expires_at_ms = self.retention.source_expiry(input.kind, input.added_at_ms);

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO source
                 (project_id, kind, label, origin_path, managed_path, sha256, added_at_ms,
                  expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                project_id,
                input.kind.as_str(),
                input.label,
                input.origin_path.as_ref().map(|p| p.display().to_string()),
                input.managed_path.as_ref().map(|p| p.display().to_string()),
                sha256,
                input.added_at_ms,
                expires_at_ms,
            ],
        )?;
        let source_id = tx.last_insert_rowid();
        for (i, chunk) in chunks.iter().enumerate() {
            let vector = vectors.as_ref().and_then(|v| v.get(i));
            tx.execute(
                "INSERT INTO source_chunk (source_id, idx, text, embedding, line_start)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    source_id,
                    i as i64,
                    chunk.text,
                    vector,
                    chunk.line_start as i64
                ],
            )?;
        }
        tx.commit()?;
        Ok(source_id)
    }

    /// Adds a file source, or refreshes the one already indexed from the same path in this project:
    /// unchanged text (same hash) is left alone; changed text replaces the old source. Other kinds
    /// of source are simply added.
    pub fn upsert_file_source(&mut self, project_id: &str, input: &SourceInput) -> Result<Upsert> {
        let origin = match (&input.kind, &input.origin_path) {
            (SourceKind::File, Some(path)) => path.display().to_string(),
            _ => return self.add_source(project_id, input).map(Upsert::Added),
        };
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, sha256 FROM source
                 WHERE project_id = ?1 AND kind = 'file' AND origin_path = ?2",
                rusqlite::params![project_id, origin],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let sha256 = hex(&Sha256::digest(input.text.as_bytes()));
        match existing {
            Some((id, old)) if old == sha256 => Ok(Upsert::Unchanged(id)),
            Some(_) => {
                let id = self.add_source(project_id, input)?;
                // Every other row for this path goes, so an interrupted refresh can't leave two.
                self.conn.execute(
                    "DELETE FROM source
                     WHERE project_id = ?1 AND kind = 'file' AND origin_path = ?2 AND id <> ?3",
                    rusqlite::params![project_id, origin, id],
                )?;
                self.after_delete()?;
                Ok(Upsert::Updated(id))
            }
            None => self.add_source(project_id, input).map(Upsert::Added),
        }
    }

    /// Replaces a source's text (a screenshot's description once it arrives): re-chunks and
    /// re-embeds it, keeping the source's id, kind and expiry. Returns `false` if it's gone.
    pub fn replace_source_text(&mut self, source_id: i64, text: &str) -> Result<bool> {
        let chunks = embed::chunk_document(text, SOURCE_CHUNK_CHARS);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let vectors = self.embed_exact(&texts)?;
        let sha256 = hex(&Sha256::digest(text.as_bytes()));
        let tx = self.conn.transaction()?;
        let found = tx.execute(
            "UPDATE source SET sha256 = ?2 WHERE id = ?1",
            rusqlite::params![source_id, sha256],
        )?;
        if found == 0 {
            return Ok(false);
        }
        tx.execute("DELETE FROM source_chunk WHERE source_id = ?1", [source_id])?;
        for (i, chunk) in chunks.iter().enumerate() {
            let vector = vectors.as_ref().and_then(|v| v.get(i));
            tx.execute(
                "INSERT INTO source_chunk (source_id, idx, text, embedding, line_start)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    source_id,
                    i as i64,
                    chunk.text,
                    vector,
                    chunk.line_start as i64
                ],
            )?;
        }
        tx.commit()?;
        self.after_delete()?;
        Ok(true)
    }

    /// One source by id.
    pub fn get_source(&self, source_id: i64) -> Result<Option<Source>> {
        Ok(self
            .sources_where("WHERE s.id = ?1", [source_id])?
            .into_iter()
            .next())
    }

    /// A project's sources, newest first.
    pub fn list_sources(&self, project_id: &str) -> Result<Vec<Source>> {
        self.sources_where("WHERE s.project_id = ?1", [project_id])
    }

    fn sources_where(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.id, s.project_id, s.kind, s.label, s.origin_path, s.managed_path, s.sha256,
                    s.added_at_ms, s.expires_at_ms,
                    (SELECT count(*) FROM source_chunk c WHERE c.source_id = s.id)
             FROM source s {filter} ORDER BY s.added_at_ms DESC, s.id DESC"
        ))?;
        let rows = stmt
            .query_map(params, |r| {
                Ok(Source {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    kind: r.get(2)?,
                    label: r.get(3)?,
                    origin_path: r.get(4)?,
                    managed_path: r.get(5)?,
                    sha256: r.get(6)?,
                    added_at_ms: r.get(7)?,
                    expires_at_ms: r.get(8)?,
                    chunk_count: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Deletes one source, its chunks and vectors, and its managed file (only inside
    /// `managed_root`). Returns what was removed.
    pub fn remove_source(&mut self, source_id: i64, managed_root: &Path) -> Result<PruneReport> {
        let managed: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT managed_path FROM source WHERE id = ?1",
                [source_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(managed) = managed else {
            return Ok(PruneReport::default());
        };
        self.conn
            .execute("DELETE FROM source WHERE id = ?1", [source_id])?;
        let mut report = PruneReport {
            sources_expired: 1,
            ..PruneReport::default()
        };
        self.after_delete()?;
        delete_managed(
            managed.into_iter().map(PathBuf::from),
            managed_root,
            &mut report,
        );
        Ok(report)
    }

    /// Deletes a project and everything in it: its meetings (transcripts, summaries, vectors), its
    /// sources, and their managed files (only inside `managed_root`).
    pub fn delete_project(&mut self, project_id: &str, managed_root: &Path) -> Result<PruneReport> {
        let managed = self.managed_paths("WHERE project_id = ?1", rusqlite::params![project_id])?;
        let tx = self.conn.transaction()?;
        let sources = tx.query_row(
            "SELECT count(*) FROM source WHERE project_id = ?1",
            [project_id],
            |r| r.get::<_, i64>(0),
        )?;
        for table in ["state_op", "candidate_log", "llm_call"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE meeting_id IN (SELECT id FROM meeting WHERE project_id = ?1)"
                ),
                [project_id],
            )?;
        }
        let meetings = tx.execute("DELETE FROM meeting WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM project WHERE id = ?1", [project_id])?;
        tx.commit()?;

        let mut report = PruneReport {
            meetings_deleted: meetings,
            sources_expired: sources as usize,
            ..PruneReport::default()
        };
        self.after_delete()?;
        delete_managed(managed, managed_root, &mut report);
        Ok(report)
    }

    /// Deletes everything whose expiry is at or before `now_ms`:
    /// - expired transcripts lose their segments, search entries and vectors; the meeting row,
    ///   title and summary stay, marked with when the transcript was pruned;
    /// - expired sources are deleted with their chunks and vectors, and their managed files are
    ///   deleted if they sit inside `managed_root`.
    ///
    /// Idempotent. Run it at startup and periodically.
    pub fn prune(&mut self, now_ms: i64, managed_root: &Path) -> Result<PruneReport> {
        let managed = self.managed_paths(
            "WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?1",
            rusqlite::params![now_ms],
        )?;

        let tx = self.conn.transaction()?;
        let expired: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM meeting
                 WHERE transcript_expires_at_ms IS NOT NULL AND transcript_expires_at_ms <= ?1
                   AND transcript_pruned_at_ms IS NULL",
            )?;
            let ids = stmt
                .query_map([now_ms], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for id in &expired {
            tx.execute("DELETE FROM segment WHERE meeting_id = ?1", [id])?;
            tx.execute("DELETE FROM chunk WHERE meeting_id = ?1", [id])?;
            tx.execute("DELETE FROM candidate_log WHERE meeting_id = ?1", [id])?;
            tx.execute("DELETE FROM llm_call WHERE meeting_id = ?1", [id])?;
            // People's names go with the transcript that labelled them.
            tx.execute("DELETE FROM speaker_name WHERE meeting_id = ?1", [id])?;
            tx.execute(
                "UPDATE meeting SET transcript_pruned_at_ms = ?2, segment_count = 0 WHERE id = ?1",
                rusqlite::params![id, now_ms],
            )?;
        }
        let sources = tx.execute(
            "DELETE FROM source WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?1",
            [now_ms],
        )?;
        // Calls for a meeting whose transcript is gone (logged after it expired), and calls with
        // no saved meeting, which expire by their own time.
        tx.execute(
            "DELETE FROM llm_call
             WHERE meeting_id IN (SELECT id FROM meeting WHERE transcript_pruned_at_ms IS NOT NULL)
                OR (?2 IS NOT NULL AND at_ms + ?2 <= ?1
                    AND (meeting_id IS NULL OR meeting_id NOT IN (SELECT id FROM meeting)))",
            rusqlite::params![
                now_ms,
                self.retention
                    .transcript_days
                    .map(|d| i64::from(d) * DAY_MS)
            ],
        )?;
        tx.commit()?;

        let mut report = PruneReport {
            transcripts_expired: expired.len(),
            sources_expired: sources,
            ..PruneReport::default()
        };
        if report.transcripts_expired > 0 || report.sources_expired > 0 {
            self.after_delete()?;
        }
        delete_managed(managed, managed_root, &mut report);
        Ok(report)
    }

    /// The managed paths of sources matching `filter` (a `WHERE` clause on `source`).
    fn managed_paths(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<PathBuf>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT managed_path FROM source {filter} AND managed_path IS NOT NULL"
        ))?;
        let paths = stmt
            .query_map(params, |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths.into_iter().map(PathBuf::from).collect())
    }

    /// Rewrites both full-text indexes after deletes, so removed text leaves the index structures
    /// rather than lingering until the next merge.
    fn after_delete(&self) -> Result<()> {
        self.conn.execute_batch(
            "INSERT INTO segment_fts (segment_fts) VALUES ('optimize');
             INSERT INTO source_chunk_fts (source_chunk_fts) VALUES ('optimize');",
        )?;
        Ok(())
    }
}

/// Deletes each managed file that resolves to somewhere inside `root`; anything else is recorded
/// as refused and left alone. A missing file counts as already gone.
fn delete_managed(paths: impl IntoIterator<Item = PathBuf>, root: &Path, report: &mut PruneReport) {
    let Ok(root) = root.canonicalize() else {
        report.files_refused.extend(paths);
        return;
    };
    for path in paths {
        match path.canonicalize() {
            Ok(real) if real.starts_with(&root) && real != root => {
                if std::fs::remove_file(&real).is_ok() {
                    report.files_deleted += 1;
                }
            }
            Ok(_) => report.files_refused.push(path),
            Err(_) => {} // already gone
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Embedder;
    use std::time::Duration;
    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptSegment};

    const T0: i64 = 1_700_000_000_000;

    fn seg(id: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id,
            text: text.to_owned(),
            start: Duration::from_millis(id * 1000),
            end: Duration::from_millis(id * 1000 + 900),
            status: SegmentStatus::Final,
            source: AudioSourceKind::System,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    fn meta(title: &str, summary: Option<&str>) -> MeetingMeta {
        MeetingMeta {
            title: Some(title.to_owned()),
            date: None,
            engine: None,
            language: None,
            summary: summary.map(str::to_owned),
        }
    }

    /// One dimension per distinct first letter: enough to prove vectors are written and removed.
    struct LetterEmbedder;

    impl Embedder for LetterEmbedder {
        fn dim(&self) -> usize {
            26
        }
        fn embed_passages(&self, texts: &[&str]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| self.vec(t)).collect())
        }
        fn embed_query(&self, text: &str) -> crate::Result<Vec<f32>> {
            Ok(self.vec(text))
        }
    }

    impl LetterEmbedder {
        fn vec(&self, t: &str) -> Vec<f32> {
            let mut v = vec![0.0; 26];
            let i = t
                .bytes()
                .find(u8::is_ascii_lowercase)
                .map_or(0, |b| (b - b'a') as usize);
            v[i] = 1.0;
            v
        }
    }

    fn count(lib: &Library, sql: &str) -> i64 {
        lib.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn library_with_meeting() -> Library {
        let mut lib = Library::open_in_memory().unwrap();
        lib.set_embedder(Some(Box::new(LetterEmbedder)));
        lib.save_note(
            "m1",
            &meta("Acme scoping", Some("Azure only; retention unresolved.")),
            T0,
            &[
                seg(1, "production must run in azure"),
                seg(2, "ninety day retention"),
            ],
        )
        .unwrap();
        lib
    }

    #[test]
    fn a_meeting_can_be_retitled_and_moved_between_projects() {
        let lib = library_with_meeting();
        lib.create_project("p1", "Acme", T0).unwrap();
        lib.create_project("p2", "Globex", T0).unwrap();

        // An automatic rename only lands while the title is still the one it expects.
        assert!(!lib
            .rename_meeting_if("m1", "Auto", "Something else")
            .unwrap());
        assert!(lib.rename_meeting_if("m1", "Auto", "Acme scoping").unwrap());
        assert!(lib.rename_meeting("m1", "  Acme hosting call ").unwrap());
        assert!(!lib.rename_meeting("nope", "x").unwrap());
        let (note, _) = lib.get_note("m1").unwrap().unwrap();
        assert_eq!(note.title, "Acme hosting call");
        // Search hits carry the new title.
        let hits = lib.search("azure", 10).unwrap();
        assert_eq!(hits[0].title, "Acme hosting call");

        assert!(lib.set_meeting_project("m1", Some("p1")).unwrap());
        assert!(lib.set_meeting_project("m1", Some("p2")).unwrap());
        assert_eq!(
            lib.get_note("m1").unwrap().unwrap().0.project_id.as_deref(),
            Some("p2")
        );
        assert!(lib.set_meeting_project("m1", None).unwrap());
        assert_eq!(lib.get_note("m1").unwrap().unwrap().0.project_id, None);

        // A project that doesn't exist (say, deleted meanwhile) is refused, and the meeting stays put.
        assert!(lib.set_meeting_project("m1", Some("gone")).is_err());
        assert_eq!(lib.get_note("m1").unwrap().unwrap().0.project_id, None);
    }

    #[test]
    fn a_project_can_be_renamed_but_names_stay_unique() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("p1", "Acme", T0).unwrap();
        lib.create_project("p2", "Globex", T0).unwrap();

        assert!(lib.rename_project("p1", " Acme Corp ").unwrap());
        assert!(!lib.rename_project("nope", "x").unwrap());
        let names: Vec<String> = lib
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Acme Corp", "Globex"]);

        let taken = lib.rename_project("p2", "Acme Corp").unwrap_err();
        assert!(taken.to_string().contains("UNIQUE"), "{taken}");
    }

    #[test]
    fn a_project_keeps_its_instructions() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("p1", "Acme", T0).unwrap();
        assert_eq!(lib.project_instructions("p1").unwrap().as_deref(), Some(""));
        assert_eq!(lib.project_instructions("nope").unwrap(), None);

        assert!(lib
            .set_project_instructions("p1", "  I own the migration. Ignore billing.\n")
            .unwrap());
        assert_eq!(
            lib.project_instructions("p1").unwrap().as_deref(),
            Some("I own the migration. Ignore billing.")
        );
        assert!(!lib.set_project_instructions("nope", "x").unwrap());
        // A rename keeps them.
        assert!(lib.rename_project("p1", "Acme Corp").unwrap());
        assert_eq!(
            lib.project_instructions("p1").unwrap().as_deref(),
            Some("I own the migration. Ignore billing.")
        );
        // Empty clears them.
        assert!(lib.set_project_instructions("p1", "  ").unwrap());
        assert_eq!(lib.project_instructions("p1").unwrap().as_deref(), Some(""));
    }

    #[test]
    fn default_policy_is_ninety_and_thirty_days() {
        let p = RetentionPolicy::default();
        assert_eq!(p.transcript_expiry(T0), Some(T0 + 90 * DAY_MS));
        assert_eq!(
            p.source_expiry(SourceKind::Pasted, T0),
            Some(T0 + 30 * DAY_MS)
        );
        assert_eq!(
            p.source_expiry(SourceKind::Managed, T0),
            Some(T0 + 30 * DAY_MS)
        );
        assert_eq!(
            p.source_expiry(SourceKind::File, T0),
            None,
            "files in place never expire"
        );
        let keep = RetentionPolicy {
            transcript_days: None,
            temp_source_days: None,
        };
        assert_eq!(keep.transcript_expiry(T0), None);
    }

    #[test]
    fn saving_a_meeting_stamps_its_transcript_expiry() {
        let lib = library_with_meeting();
        let (note, _) = lib.get_note("m1").unwrap().unwrap();
        assert_eq!(note.transcript_expires_at_ms, Some(T0 + 90 * DAY_MS));
        assert_eq!(note.transcript_pruned_at_ms, None);
    }

    #[test]
    fn prune_deletes_an_expired_transcript_but_keeps_the_meeting_and_summary() {
        let mut lib = library_with_meeting();
        let root = tempfile::tempdir().unwrap();
        let expiry = T0 + 90 * DAY_MS;

        // A day early: nothing happens.
        let early = lib.prune(expiry - DAY_MS, root.path()).unwrap();
        assert_eq!(early, PruneReport::default());
        assert_eq!(lib.search("azure", 10).unwrap().len(), 1);

        let report = lib.prune(expiry, root.path()).unwrap();
        assert_eq!(report.transcripts_expired, 1);

        let (note, segments) = lib.get_note("m1").unwrap().unwrap();
        assert!(segments.is_empty(), "segments deleted");
        assert_eq!(
            note.summary.as_deref(),
            Some("Azure only; retention unresolved.")
        );
        assert_eq!(note.title, "Acme scoping");
        assert_eq!(note.transcript_pruned_at_ms, Some(expiry));
        assert_eq!(note.segment_count, 0);
        assert!(
            lib.search("azure", 10).unwrap().is_empty(),
            "full-text entries gone"
        );
        assert!(
            lib.search_semantic("azure", 10).unwrap().is_empty(),
            "vectors gone"
        );
        assert_eq!(count(&lib, "SELECT count(*) FROM chunk"), 0);

        // Idempotent.
        assert_eq!(
            lib.prune(expiry + DAY_MS, root.path()).unwrap(),
            PruneReport::default()
        );
    }

    #[test]
    fn ai_activity_goes_with_its_transcript_or_by_its_own_time() {
        use crate::llm_log::tests::call;
        let mut lib = library_with_meeting();
        let root = tempfile::tempdir().unwrap();
        let expiry = T0 + 90 * DAY_MS;
        // Made for m1 late in its life: still goes with m1's transcript.
        lib.insert_llm_call(&call(expiry - DAY_MS, Some("m1")))
            .unwrap();
        // No meeting, or one never saved: expire by their own time.
        lib.insert_llm_call(&call(T0, None)).unwrap();
        lib.insert_llm_call(&call(T0 + DAY_MS, Some("unsaved")))
            .unwrap();
        lib.insert_llm_call(&call(T0 + 60 * DAY_MS, None)).unwrap();
        let left = |lib: &Library| {
            lib.llm_calls(None, 10)
                .unwrap()
                .iter()
                .map(|c| c.at_ms)
                .collect::<Vec<_>>()
        };

        lib.prune(expiry - DAY_MS, root.path()).unwrap();
        assert_eq!(left(&lib).len(), 4, "nothing due yet");
        lib.prune(expiry, root.path()).unwrap();
        assert_eq!(left(&lib), [T0 + 60 * DAY_MS, T0 + DAY_MS]);
        lib.prune(T0 + 150 * DAY_MS, root.path()).unwrap();
        assert!(left(&lib).is_empty());

        // Keeping transcripts forever keeps the log too.
        lib.set_retention(RetentionPolicy {
            transcript_days: None,
            ..RetentionPolicy::default()
        });
        lib.insert_llm_call(&call(T0, None)).unwrap();
        lib.prune(T0 + 1000 * DAY_MS, root.path()).unwrap();
        assert_eq!(left(&lib).len(), 1);
    }

    #[test]
    fn resaving_a_meeting_keeps_its_project() {
        let mut lib = library_with_meeting();
        lib.create_project("p1", "Acme", T0).unwrap();
        assert!(lib.set_meeting_project("m1", Some("p1")).unwrap());

        lib.save_note("m1", &meta("Acme scoping v2", None), T0, &[seg(1, "again")])
            .unwrap();

        let (note, _) = lib.get_note("m1").unwrap().unwrap();
        assert_eq!(note.project_id.as_deref(), Some("p1"));
        assert_eq!(note.title, "Acme scoping v2");
    }

    #[test]
    fn upgrading_from_v2_keeps_meetings_and_gives_them_no_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(crate::store::SCHEMA_V1).unwrap();
            conn.execute_batch(crate::store::SCHEMA_V2).unwrap();
            conn.execute(
                "INSERT INTO meeting (id, title, started_at_ms, duration_ms, segment_count)
                 VALUES ('old', 'Old', 1000, 0, 0)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let mut lib = Library::open(&path).unwrap();
        let (note, _) = lib.get_note("old").unwrap().unwrap();
        assert_eq!(
            note.transcript_expires_at_ms, None,
            "an upgrade never schedules a deletion"
        );
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            lib.prune(i64::MAX, root.path())
                .unwrap()
                .transcripts_expired,
            0
        );
        lib.create_project("p", "P", 0).unwrap();
        assert_eq!(lib.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn expired_sources_go_with_their_chunks_vectors_and_managed_file() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.set_embedder(Some(Box::new(LetterEmbedder)));
        lib.create_project("p1", "Acme", T0).unwrap();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        let managed = root.path().join("security-doc.txt");
        std::fs::write(&managed, "copy").unwrap();
        let stray = outside.path().join("not-ours.txt");
        std::fs::write(&stray, "user file").unwrap();

        let source = |kind, label: &str, managed_path: Option<PathBuf>, origin: Option<PathBuf>| {
            SourceInput {
                kind,
                label: label.into(),
                text: format!("{label} mentions a ninety day retention requirement"),
                origin_path: origin,
                managed_path,
                added_at_ms: T0,
            }
        };
        lib.add_source(
            "p1",
            &source(SourceKind::Managed, "managed", Some(managed.clone()), None),
        )
        .unwrap();
        lib.add_source(
            "p1",
            &source(SourceKind::Pasted, "pasted", Some(stray.clone()), None),
        )
        .unwrap();
        let kept = lib
            .add_source(
                "p1",
                &source(SourceKind::File, "folder", None, Some("/docs/a.md".into())),
            )
            .unwrap();

        let sources = lib.list_sources("p1").unwrap();
        assert_eq!(sources.len(), 3);
        assert!(sources
            .iter()
            .all(|s| s.chunk_count == 1 && s.sha256.len() == 64));
        assert_eq!(
            count(
                &lib,
                "SELECT count(*) FROM source_chunk WHERE embedding IS NOT NULL"
            ),
            3
        );
        let fts = "SELECT count(*) FROM source_chunk_fts WHERE source_chunk_fts MATCH 'retention'";
        assert_eq!(count(&lib, fts), 3);

        let report = lib.prune(T0 + 30 * DAY_MS, root.path()).unwrap();
        assert_eq!(report.sources_expired, 2);
        assert_eq!(report.files_deleted, 1);
        assert_eq!(report.files_refused, vec![stray.clone()]);
        assert!(!managed.exists(), "managed copy deleted");
        assert!(
            stray.exists(),
            "a path outside the managed directory is never deleted"
        );

        let left = lib.list_sources("p1").unwrap();
        assert_eq!(left.iter().map(|s| s.id).collect::<Vec<_>>(), vec![kept]);
        assert_eq!(count(&lib, "SELECT count(*) FROM source_chunk"), 1);
        assert_eq!(
            count(&lib, fts),
            1,
            "expired chunks left the full-text index"
        );
    }

    #[test]
    fn a_screenshot_is_temporary_its_description_is_searchable_and_its_image_goes_with_it() {
        let mut lib = library_with_meeting();
        lib.create_project("p1", "Acme", T0).unwrap();
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("shot.png");
        std::fs::write(&image, "png").unwrap();
        let id = lib
            .add_source(
                "p1",
                &SourceInput {
                    kind: SourceKind::Screenshot,
                    label: "Screenshot · 14:03".into(),
                    text: "Screenshot (not described yet)".into(),
                    origin_path: None,
                    managed_path: Some(image.clone()),
                    added_at_ms: T0,
                },
            )
            .unwrap();
        let source = lib.get_source(id).unwrap().unwrap();
        assert_eq!(source.kind, "screenshot");
        assert_eq!(source.expires_at_ms, Some(T0 + 30 * DAY_MS));

        assert!(lib
            .replace_source_text(id, "Architecture diagram: web tier in Azure West Europe")
            .unwrap());
        let fts = |w: &str| {
            format!("SELECT count(*) FROM source_chunk_fts WHERE source_chunk_fts MATCH '{w}'")
        };
        assert_eq!(count(&lib, &fts("diagram")), 1);
        assert_eq!(count(&lib, &fts("described")), 0, "the placeholder is gone");
        assert!(!lib.replace_source_text(999, "x").unwrap());

        assert_eq!(
            lib.preview_policy(
                RetentionPolicy {
                    transcript_days: None,
                    temp_source_days: Some(7)
                },
                T0 + 8 * DAY_MS
            )
            .unwrap()
            .1,
            1,
            "screenshots follow the temporary-source policy"
        );
        let report = lib.prune(T0 + 30 * DAY_MS, root.path()).unwrap();
        assert_eq!(report.sources_expired, 1);
        assert!(!image.exists(), "the image is deleted with its source");
        assert!(lib.get_source(id).unwrap().is_none());
    }

    #[test]
    fn deleting_a_project_removes_its_meetings_sources_and_files() {
        let mut lib = library_with_meeting();
        lib.create_project("p1", "Acme", T0).unwrap();
        lib.set_meeting_project("m1", Some("p1")).unwrap();
        lib.save_note("m2", &meta("Other", None), T0, &[seg(1, "unrelated")])
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("copy.txt");
        std::fs::write(&managed, "x").unwrap();
        lib.add_source(
            "p1",
            &SourceInput {
                kind: SourceKind::Managed,
                label: "copy".into(),
                text: "azure tenant".into(),
                origin_path: None,
                managed_path: Some(managed.clone()),
                added_at_ms: T0,
            },
        )
        .unwrap();

        let report = lib.delete_project("p1", root.path()).unwrap();
        assert_eq!(report.meetings_deleted, 1);
        assert_eq!(report.sources_expired, 1);
        assert_eq!(report.files_deleted, 1);
        assert!(lib.get_note("m1").unwrap().is_none());
        assert!(
            lib.get_note("m2").unwrap().is_some(),
            "meetings outside the project stay"
        );
        assert!(lib.list_projects().unwrap().is_empty());
        assert_eq!(count(&lib, "SELECT count(*) FROM source_chunk"), 0);
        assert!(!managed.exists());
    }

    /// Row counts in every table that holds meeting- or project-derived data.
    fn footprint(lib: &Library, meeting: &str, project: &str) -> Vec<(&'static str, i64)> {
        let q =
            |sql: &str, arg: &str| -> i64 { lib.conn.query_row(sql, [arg], |r| r.get(0)).unwrap() };
        vec![
            ("meeting", q("SELECT count(*) FROM meeting WHERE id = ?1", meeting)),
            ("segment", q("SELECT count(*) FROM segment WHERE meeting_id = ?1", meeting)),
            ("chunk", q("SELECT count(*) FROM chunk WHERE meeting_id = ?1", meeting)),
            ("state_op", q("SELECT count(*) FROM state_op WHERE meeting_id = ?1", meeting)),
            ("candidate_log", q("SELECT count(*) FROM candidate_log WHERE meeting_id = ?1", meeting)),
            ("llm_call", q("SELECT count(*) FROM llm_call WHERE meeting_id = ?1", meeting)),
            ("source", q("SELECT count(*) FROM source WHERE project_id = ?1", project)),
            ("source_chunk", q("SELECT count(*) FROM source_chunk c JOIN source s ON s.id = c.source_id WHERE s.project_id = ?1", project)),
            ("project_memory", q("SELECT count(*) FROM project_memory WHERE project_id = ?1", project)),
        ]
    }

    /// A project with one meeting carrying every kind of derived data, and a managed file.
    fn full_project(lib: &mut Library, root: &Path) -> PathBuf {
        lib.create_project("p", "Acme", T0).unwrap();
        lib.set_meeting_project("m1", Some("p")).unwrap();
        lib.save_state_ops(
            "m1",
            &[crate::StoredOp {
                seq: 0,
                at_ms: T0,
                op: "{}".into(),
            }],
        )
        .unwrap();
        lib.save_candidate_log(
            "m1",
            &[crate::StoredLogEntry {
                seq: 0,
                at_ms: T0,
                entry: "{}".into(),
            }],
        )
        .unwrap();
        lib.insert_llm_call(&crate::llm_log::tests::call(T0, Some("m1")))
            .unwrap();
        lib.add_memory(
            "p",
            &crate::MemoryInput {
                kind: "fact".into(),
                text: "Azure only".into(),
                status: "stated".into(),
                confidence: 1.0,
                provenance: vec![],
                meeting_id: Some("m1".into()),
            },
            T0,
        )
        .unwrap();
        let copy = root.join("spec.md");
        std::fs::write(&copy, "azure spec").unwrap();
        lib.add_source(
            "p",
            &SourceInput {
                kind: SourceKind::Managed,
                label: "spec.md".into(),
                text: "azure spec".into(),
                origin_path: None,
                managed_path: Some(copy.clone()),
                added_at_ms: T0,
            },
        )
        .unwrap();
        copy
    }

    #[test]
    fn deleting_a_project_leaves_nothing_behind() {
        let mut lib = library_with_meeting();
        let root = tempfile::tempdir().unwrap();
        let copy = full_project(&mut lib, root.path());
        assert!(
            footprint(&lib, "m1", "p").iter().all(|(_, n)| *n > 0),
            "{:?}",
            footprint(&lib, "m1", "p")
        );

        let report = lib.delete_project("p", root.path()).unwrap();
        assert_eq!(
            (
                report.meetings_deleted,
                report.sources_expired,
                report.files_deleted
            ),
            (1, 1, 1)
        );
        let left: Vec<_> = footprint(&lib, "m1", "p")
            .into_iter()
            .filter(|(_, n)| *n != 0)
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(!copy.exists());
        assert!(lib.search("azure", 10).unwrap().is_empty());
        assert!(lib.search_semantic("azure", 10).unwrap().is_empty());
        let fts_rows: i64 = lib
            .conn
            .query_row(
                "SELECT count(*) FROM source_chunk_fts WHERE source_chunk_fts MATCH 'azure'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_rows, 0);
    }

    #[test]
    fn deleting_a_meeting_removes_all_of_it_but_not_its_project() {
        let mut lib = library_with_meeting();
        let root = tempfile::tempdir().unwrap();
        full_project(&mut lib, root.path());
        assert!(lib.delete_note("m1").unwrap());
        let fp = footprint(&lib, "m1", "p");
        for table in [
            "meeting",
            "segment",
            "chunk",
            "state_op",
            "candidate_log",
            "llm_call",
        ] {
            assert_eq!(
                fp.iter().find(|(t, _)| *t == table).unwrap().1,
                0,
                "{table}"
            );
        }
        assert!(lib.search("ninety", 10).unwrap().is_empty());
        assert_eq!(
            lib.list_memory("p").unwrap().len(),
            1,
            "accepted project knowledge stays"
        );
        assert_eq!(lib.list_sources("p").unwrap().len(), 1);
    }

    #[test]
    fn a_first_start_after_months_away_prunes_everything_due_in_one_pass() {
        let mut lib = library_with_meeting();
        lib.create_project("p", "Acme", T0).unwrap();
        lib.add_source(
            "p",
            &SourceInput {
                kind: SourceKind::Pasted,
                label: "notes".into(),
                text: "azure".into(),
                origin_path: None,
                managed_path: None,
                added_at_ms: T0,
            },
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let months_later = T0 + 400 * DAY_MS;
        let report = lib.prune(months_later, root.path()).unwrap();
        assert_eq!((report.transcripts_expired, report.sources_expired), (1, 1));
        assert_eq!(
            lib.prune(months_later, root.path()).unwrap(),
            PruneReport::default()
        );
    }

    #[test]
    fn a_new_policy_applies_to_existing_data_and_can_be_previewed() {
        let mut lib = library_with_meeting();
        lib.create_project("p", "Acme", T0).unwrap();
        let pasted = SourceInput {
            kind: SourceKind::Pasted,
            label: "notes".into(),
            text: "azure".into(),
            origin_path: None,
            managed_path: None,
            added_at_ms: T0,
        };
        lib.add_source("p", &pasted).unwrap();
        lib.add_source(
            "p",
            &SourceInput {
                kind: SourceKind::File,
                origin_path: Some(PathBuf::from("/docs/a.md")),
                ..pasted.clone()
            },
        )
        .unwrap();
        let week = RetentionPolicy {
            transcript_days: Some(7),
            temp_source_days: Some(7),
        };
        assert_eq!(lib.preview_policy(week, T0 + 8 * DAY_MS).unwrap(), (1, 1));
        assert_eq!(lib.preview_policy(week, T0 + 6 * DAY_MS).unwrap(), (0, 0));
        assert_eq!(
            lib.preview_policy(
                RetentionPolicy {
                    transcript_days: None,
                    temp_source_days: None
                },
                i64::MAX
            )
            .unwrap(),
            (0, 0)
        );

        assert_eq!(
            lib.restamp_expiries(week).unwrap(),
            (1, 1),
            "the in-place file is not restamped"
        );
        assert_eq!(lib.retention(), week);
        let (note, _) = lib.get_note("m1").unwrap().unwrap();
        assert_eq!(note.transcript_expires_at_ms, Some(T0 + 7 * DAY_MS));
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            lib.prune(T0 + 8 * DAY_MS, root.path())
                .unwrap()
                .transcripts_expired,
            1
        );

        // Keep forever clears expiries; a pruned transcript stays pruned.
        let keep = RetentionPolicy {
            transcript_days: None,
            temp_source_days: None,
        };
        assert_eq!(lib.restamp_expiries(keep).unwrap(), (0, 0));
        assert!(lib
            .list_sources("p")
            .unwrap()
            .iter()
            .all(|s| s.expires_at_ms.is_none()));
        assert!(lib
            .get_note("m1")
            .unwrap()
            .unwrap()
            .0
            .transcript_pruned_at_ms
            .is_some());
    }

    #[test]
    fn secure_delete_is_on() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(count(&lib, "PRAGMA secure_delete"), 1);
    }
}

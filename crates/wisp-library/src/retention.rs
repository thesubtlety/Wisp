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
            SourceKind::Pasted | SourceKind::Managed => self
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
    /// A copy the app owns (an import, a screenshot's text). Temporary; the file is deleted too.
    Managed,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::File => "file",
            SourceKind::Pasted => "pasted",
            SourceKind::Managed => "managed",
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

    /// A project's sources, newest first.
    pub fn list_sources(&self, project_id: &str) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.project_id, s.kind, s.label, s.origin_path, s.managed_path, s.sha256,
                    s.added_at_ms, s.expires_at_ms,
                    (SELECT count(*) FROM source_chunk c WHERE c.source_id = s.id)
             FROM source s WHERE s.project_id = ?1 ORDER BY s.added_at_ms DESC, s.id DESC",
        )?;
        let rows = stmt
            .query_map([project_id], |r| {
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
        tx.execute(
            "DELETE FROM state_op WHERE meeting_id IN (SELECT id FROM meeting WHERE project_id = ?1)",
            [project_id],
        )?;
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
            tx.execute(
                "UPDATE meeting SET transcript_pruned_at_ms = ?2, segment_count = 0 WHERE id = ?1",
                rusqlite::params![id, now_ms],
            )?;
        }
        let sources = tx.execute(
            "DELETE FROM source WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?1",
            [now_ms],
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

    #[test]
    fn secure_delete_is_on() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(count(&lib, "PRAGMA secure_delete"), 1);
    }
}

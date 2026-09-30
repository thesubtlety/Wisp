//! The prompt library: saved prompts the user runs over a meeting, and the outputs of those runs.
//!
//! Prompts are the user's own text (the built-ins are seeded on open and can be edited, then reset,
//! but not deleted). A run's output quotes the transcript, so it lives as long as the transcript:
//! it is deleted with its meeting (`ON DELETE CASCADE`) and pruned when the transcript expires.

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use wisp_core::prompts::{builtin_prompt, BUILTIN_PROMPTS};
use wisp_core::speakers::{line_speaker, SpeakerNames};
use wisp_core::transcript::SpeakerId;

use crate::store::Library;
use crate::Result;

/// A saved prompt (one `prompt` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub body: String,
    /// `meeting` or `speaker` (see [`wisp_core::prompts::PromptScope`]).
    pub scope: String,
    /// Shipped with the app: it can be reset, not deleted.
    pub builtin: bool,
    /// A built-in whose text differs from the shipped one.
    pub customized: bool,
    pub updated_at_ms: i64,
}

/// One stored output of a prompt run over a saved meeting (one `prompt_run` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRun {
    /// Assigned by the store; ignored on insert.
    pub id: i64,
    pub meeting_id: String,
    /// The prompt's name when it ran (the prompt may change or go later).
    pub prompt_name: String,
    /// The speaker it was about, for a speaker-scoped prompt.
    pub speaker: Option<String>,
    pub output: String,
    /// The backend that answered.
    pub backend: String,
    pub at_ms: i64,
}

const PROMPT_COLUMNS: &str = "id, name, body, scope, builtin, updated_at_ms";

fn prompt_row(r: &rusqlite::Row) -> rusqlite::Result<Prompt> {
    let id: String = r.get(0)?;
    let name: String = r.get(1)?;
    let body: String = r.get(2)?;
    let scope: String = r.get(3)?;
    let builtin = r.get::<_, i64>(4)? != 0;
    let customized = builtin
        && builtin_prompt(&id)
            .is_some_and(|b| b.name != name || b.body != body || b.scope.as_str() != scope);
    Ok(Prompt {
        id,
        name,
        body,
        scope,
        builtin,
        customized,
        updated_at_ms: r.get(5)?,
    })
}

fn run_row(r: &rusqlite::Row) -> rusqlite::Result<PromptRun> {
    Ok(PromptRun {
        id: r.get(0)?,
        meeting_id: r.get(1)?,
        prompt_name: r.get(2)?,
        speaker: r.get(3)?,
        output: r.get(4)?,
        backend: r.get(5)?,
        at_ms: r.get(6)?,
    })
}

impl Library {
    /// Adds any built-in prompt that is missing. Never touches one that exists, so the user's edits
    /// stay. Idempotent; run on every open.
    pub(crate) fn seed_prompts(&self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT OR IGNORE INTO prompt (id, name, body, scope, builtin, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 1, 0)",
        )?;
        for p in BUILTIN_PROMPTS {
            stmt.execute(rusqlite::params![p.id, p.name, p.body, p.scope.as_str()])?;
        }
        Ok(())
    }

    /// Every prompt: the built-ins in shipped order, then the user's, oldest first.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROMPT_COLUMNS} FROM prompt ORDER BY builtin DESC, rowid"
        ))?;
        let rows = stmt
            .query_map([], prompt_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One prompt by id.
    pub fn get_prompt(&self, id: &str) -> Result<Option<Prompt>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {PROMPT_COLUMNS} FROM prompt WHERE id = ?1"),
                [id],
                prompt_row,
            )
            .optional()?)
    }

    /// Adds a user prompt under a caller-generated `id`. `scope` must be valid (the caller checks).
    pub fn create_prompt(
        &self,
        id: &str,
        name: &str,
        body: &str,
        scope: &str,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO prompt (id, name, body, scope, builtin, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            rusqlite::params![id, name, body, scope, now_ms],
        )?;
        Ok(())
    }

    /// Replaces a prompt's name, body and scope. Returns whether it exists.
    pub fn update_prompt(
        &self,
        id: &str,
        name: &str,
        body: &str,
        scope: &str,
        now_ms: i64,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE prompt SET name = ?2, body = ?3, scope = ?4, updated_at_ms = ?5 WHERE id = ?1",
            rusqlite::params![id, name, body, scope, now_ms],
        )?;
        Ok(n > 0)
    }

    /// Deletes a user prompt. Built-ins are never deleted. Returns whether one was deleted.
    pub fn delete_prompt(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM prompt WHERE id = ?1 AND builtin = 0", [id])?;
        Ok(n > 0)
    }

    /// Puts a built-in back to its shipped text. Returns whether `id` is a built-in.
    pub fn reset_prompt(&self, id: &str, now_ms: i64) -> Result<bool> {
        let Some(b) = builtin_prompt(id) else {
            return Ok(false);
        };
        self.conn.execute(
            "INSERT INTO prompt (id, name, body, scope, builtin, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, body = excluded.body,
                 scope = excluded.scope, builtin = 1, updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![b.id, b.name, b.body, b.scope.as_str(), now_ms],
        )?;
        Ok(true)
    }

    /// Stores a run's output for a saved meeting. Returns its row id. Fails if the meeting is not
    /// saved.
    /// Stores a run. Nothing is stored (id 0) when the meeting is gone or its transcript was pruned
    /// while the run was in flight: the output quotes the transcript and must not outlive it.
    pub fn insert_prompt_run(&self, run: &PromptRun) -> Result<i64> {
        let n = self.conn.execute(
            "INSERT INTO prompt_run (meeting_id, prompt_name, speaker, output, backend, at_ms)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE EXISTS (SELECT 1 FROM meeting
                           WHERE id = ?1 AND transcript_pruned_at_ms IS NULL)",
            rusqlite::params![
                run.meeting_id,
                run.prompt_name,
                run.speaker,
                run.output,
                run.backend,
                run.at_ms
            ],
        )?;
        Ok(if n == 0 {
            0
        } else {
            self.conn.last_insert_rowid()
        })
    }

    /// A meeting's stored runs, newest first.
    pub fn prompt_runs(&self, meeting_id: &str) -> Result<Vec<PromptRun>> {
        read_runs(&self.conn, meeting_id)
    }

    /// Deletes one stored run. Returns whether it existed.
    pub fn delete_prompt_run(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM prompt_run WHERE id = ?1", [id])?
            > 0)
    }

    /// A saved meeting's transcript as `Name: text` lines, in speech order, with the speaker names
    /// the user gave. `None` when there is no such meeting; empty once the transcript is pruned.
    pub fn meeting_transcript(&self, meeting_id: &str) -> Result<Option<String>> {
        let Some((_, segments)) = self.get_note(meeting_id)? else {
            return Ok(None);
        };
        let names = self.speaker_names(meeting_id)?;
        let mut segments = segments;
        segments.sort_by_key(|s| (s.start_ms, s.idx));
        let lines: Vec<String> = segments
            .iter()
            .map(|s| format!("{}: {}", label(&s.source, s.speaker, &names), s.text))
            .collect();
        Ok(Some(lines.join("\n")))
    }
}

/// Who spoke a stored line: the same rule as the reasoning context ([`line_speaker`]).
fn label(source: &str, speaker: Option<i64>, names: &SpeakerNames) -> String {
    let id = speaker.and_then(|s| u32::try_from(s).ok()).map(SpeakerId);
    line_speaker(source == "mic", id, names)
}

/// A meeting's runs, newest first, on any connection or transaction.
pub(crate) fn read_runs(conn: &Connection, meeting_id: &str) -> Result<Vec<PromptRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, meeting_id, prompt_name, speaker, output, backend, at_ms FROM prompt_run
         WHERE meeting_id = ?1 ORDER BY at_ms DESC, id DESC",
    )?;
    let runs = stmt
        .query_map([meeting_id], run_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(runs)
}

/// Puts back runs read before their meeting was re-saved (the re-save cascades them away).
pub(crate) fn write_runs(conn: &Connection, runs: &[PromptRun]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO prompt_run (id, meeting_id, prompt_name, speaker, output, backend, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for r in runs {
        stmt.execute(rusqlite::params![
            r.id,
            r.meeting_id,
            r.prompt_name,
            r.speaker,
            r.output,
            r.backend,
            r.at_ms
        ])?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn a_run_is_not_stored_for_a_pruned_or_missing_meeting() {
        let lib = library_with_meeting();
        lib.conn
            .execute(
                "UPDATE meeting SET transcript_pruned_at_ms = 1 WHERE id = 'm1'",
                [],
            )
            .unwrap();
        assert_eq!(lib.insert_prompt_run(&run("m1", 10)).unwrap(), 0);
        assert_eq!(lib.insert_prompt_run(&run("gone", 10)).unwrap(), 0);
        assert!(lib.prompt_runs("m1").unwrap().is_empty());
    }

    use std::path::Path;
    use std::time::Duration;

    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptSegment};

    use super::*;

    fn seg(
        id: u64,
        source: AudioSourceKind,
        speaker: Option<u32>,
        text: &str,
    ) -> TranscriptSegment {
        TranscriptSegment {
            id,
            text: text.into(),
            start: Duration::from_millis(id * 1000),
            end: Duration::from_millis(id * 1000 + 500),
            status: SegmentStatus::Final,
            source,
            speaker: speaker.map(SpeakerId),
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    fn library_with_meeting() -> Library {
        let mut lib = Library::open_in_memory().unwrap();
        lib.save_note(
            "m1",
            &MeetingMeta::default(),
            0,
            &[
                seg(0, AudioSourceKind::Microphone, None, "Hello all."),
                seg(1, AudioSourceKind::System, Some(0), "Hi there."),
                seg(2, AudioSourceKind::System, Some(1), "Morning."),
            ],
        )
        .unwrap();
        lib
    }

    pub(crate) fn run(meeting: &str, at_ms: i64) -> PromptRun {
        PromptRun {
            id: 0,
            meeting_id: meeting.into(),
            prompt_name: "Communication feedback".into(),
            speaker: Some("Alice".into()),
            output: "Alice said: Hi there.".into(),
            backend: "claude".into(),
            at_ms,
        }
    }

    fn run_count(lib: &Library) -> i64 {
        lib.conn
            .query_row("SELECT count(*) FROM prompt_run", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn builtins_are_seeded_once_and_edits_survive_a_reseed() {
        let lib = Library::open_in_memory().unwrap();
        let prompts = lib.list_prompts().unwrap();
        assert_eq!(prompts.len(), BUILTIN_PROMPTS.len());
        assert!(prompts.iter().all(|p| p.builtin && !p.customized));
        assert_eq!(prompts[0].id, BUILTIN_PROMPTS[0].id, "shipped order");

        lib.update_prompt("builtin-summary", "Recap", "Short.", "meeting", 5)
            .unwrap();
        lib.seed_prompts().unwrap();
        lib.seed_prompts().unwrap();
        let prompts = lib.list_prompts().unwrap();
        assert_eq!(prompts.len(), BUILTIN_PROMPTS.len());
        let summary = lib.get_prompt("builtin-summary").unwrap().unwrap();
        assert_eq!((summary.name.as_str(), summary.customized), ("Recap", true));

        assert!(lib.reset_prompt("builtin-summary", 9).unwrap());
        let summary = lib.get_prompt("builtin-summary").unwrap().unwrap();
        assert_eq!(summary.name, "Summary");
        assert!(!summary.customized);
    }

    #[test]
    fn user_prompts_can_be_deleted_but_builtins_cannot() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_prompt("u1", "Mine", "Do {thing}.", "speaker", 1)
            .unwrap();
        let mine = lib.list_prompts().unwrap().pop().unwrap();
        assert_eq!((mine.id.as_str(), mine.builtin), ("u1", false));
        assert!(!lib.reset_prompt("u1", 2).unwrap());
        assert!(!lib.delete_prompt("builtin-summary").unwrap());
        assert!(lib.delete_prompt("u1").unwrap());
        assert!(lib.get_prompt("u1").unwrap().is_none());
        assert!(!lib.update_prompt("u1", "x", "y", "meeting", 3).unwrap());
    }

    #[test]
    fn the_transcript_reads_with_speaker_names() {
        let lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        assert_eq!(
            lib.meeting_transcript("m1").unwrap().unwrap(),
            "You: Hello all.\nAlice: Hi there.\nSpeaker 2: Morning."
        );
        assert!(lib.meeting_transcript("missing").unwrap().is_none());
    }

    #[test]
    fn runs_are_listed_newest_first_and_deletable() {
        let lib = library_with_meeting();
        let a = lib.insert_prompt_run(&run("m1", 10)).unwrap();
        lib.insert_prompt_run(&run("m1", 20)).unwrap();
        let runs = lib.prompt_runs("m1").unwrap();
        assert_eq!(runs.iter().map(|r| r.at_ms).collect::<Vec<_>>(), [20, 10]);
        assert_eq!(runs[1].speaker.as_deref(), Some("Alice"));
        assert!(lib.delete_prompt_run(a).unwrap());
        assert!(!lib.delete_prompt_run(a).unwrap());
        assert_eq!(
            lib.insert_prompt_run(&run("unsaved", 1)).unwrap(),
            0,
            "a run needs a saved meeting"
        );
    }

    #[test]
    fn runs_go_with_the_meeting_and_survive_a_resave() {
        let mut lib = library_with_meeting();
        lib.insert_prompt_run(&run("m1", 10)).unwrap();
        lib.save_note(
            "m1",
            &MeetingMeta::default(),
            0,
            &[seg(0, AudioSourceKind::Microphone, None, "Hello.")],
        )
        .unwrap();
        assert_eq!(lib.prompt_runs("m1").unwrap().len(), 1);

        assert!(lib.delete_note("m1").unwrap());
        assert_eq!(run_count(&lib), 0);
    }

    #[test]
    fn runs_are_pruned_with_the_transcript() {
        let mut lib = library_with_meeting();
        lib.insert_prompt_run(&run("m1", 10)).unwrap();
        let expiry: i64 = lib
            .conn
            .query_row(
                "SELECT transcript_expires_at_ms FROM meeting WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        lib.prune(expiry - 1, Path::new("/nonexistent")).unwrap();
        assert_eq!(run_count(&lib), 1, "kept while the transcript is");
        lib.prune(expiry, Path::new("/nonexistent")).unwrap();
        assert_eq!(run_count(&lib), 0);
        assert!(lib.get_note("m1").unwrap().is_some(), "the meeting stays");
    }

    #[test]
    fn a_v10_database_gains_the_prompt_tables_and_keeps_its_meetings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let mut lib = Library::open(&path).unwrap();
            lib.save_note(
                "m1",
                &MeetingMeta::default(),
                0,
                &[seg(0, AudioSourceKind::Microphone, None, "Hello.")],
            )
            .unwrap();
            lib.conn
                .execute_batch(
                    "DROP TABLE prompt_run; DROP TABLE prompt; PRAGMA user_version = 10;",
                )
                .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let version: i64 = lib
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 11);
        assert_eq!(lib.count().unwrap(), 1);
        assert_eq!(lib.list_prompts().unwrap().len(), BUILTIN_PROMPTS.len());
        lib.insert_prompt_run(&run("m1", 1)).unwrap();
    }
}

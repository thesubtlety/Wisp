//! Ask threads over a saved meeting: the questions the user asked about a past meeting and the
//! answers they got back.
//!
//! An answer quotes the transcript — its citations are transcript lines and state items — so a
//! thread lives exactly as long as the transcript: it is deleted with its meeting
//! (`ON DELETE CASCADE`) and pruned when the transcript expires. The live pane's Ask is in-memory
//! only; this is the saved-meeting counterpart, the same way `prompt_run` stores a saved run.

use rusqlite::Connection;

use crate::store::Library;
use crate::Result;

/// One stored question-and-answer about a saved meeting. `answer` is opaque to the store: the app
/// layer encodes the checked answer (its text and citations) as JSON and the store only keeps and
/// returns that string.
#[derive(Debug, Clone, PartialEq)]
pub struct AskTurnRow {
    /// Assigned by the store; ignored on insert.
    pub id: i64,
    pub meeting_id: String,
    pub question: String,
    /// The answer, encoded by the app layer (JSON of the checked answer).
    pub answer: String,
    pub at_ms: i64,
}

impl Library {
    /// Stores one asked-and-answered turn for a saved meeting. Returns its row id, or 0 when nothing
    /// was stored: the meeting is gone, or its transcript was pruned while the question was in
    /// flight. The answer quotes the transcript and must not outlive it — the same guard as
    /// [`Library::insert_prompt_run`].
    pub fn insert_ask_turn(&self, turn: &AskTurnRow) -> Result<i64> {
        let n = self.conn.execute(
            "INSERT INTO ask_turn (meeting_id, question, answer, at_ms)
             SELECT ?1, ?2, ?3, ?4
             WHERE EXISTS (SELECT 1 FROM meeting
                           WHERE id = ?1 AND transcript_pruned_at_ms IS NULL)",
            rusqlite::params![turn.meeting_id, turn.question, turn.answer, turn.at_ms],
        )?;
        Ok(if n == 0 {
            0
        } else {
            self.conn.last_insert_rowid()
        })
    }

    /// A saved meeting's ask thread, oldest first — reading order, and the order it is replayed as
    /// history when the conversation continues.
    pub fn ask_turns(&self, meeting_id: &str) -> Result<Vec<AskTurnRow>> {
        read_turns(&self.conn, meeting_id)
    }

    /// Clears a saved meeting's ask thread. Returns how many turns it held.
    pub fn clear_ask_turns(&self, meeting_id: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM ask_turn WHERE meeting_id = ?1", [meeting_id])?)
    }

    /// Replaces one stored turn's answer (the app re-encoded it, e.g. after a speaker rename).
    /// Returns whether a row changed.
    pub fn set_ask_answer(&self, id: i64, answer: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE ask_turn SET answer = ?2 WHERE id = ?1",
            rusqlite::params![id, answer],
        )?;
        Ok(n > 0)
    }
}

/// A meeting's ask turns, oldest first, on any connection or transaction.
fn read_turns(conn: &Connection, meeting_id: &str) -> Result<Vec<AskTurnRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, meeting_id, question, answer, at_ms FROM ask_turn
         WHERE meeting_id = ?1 ORDER BY at_ms ASC, id ASC",
    )?;
    let turns = stmt
        .query_map([meeting_id], |r| {
            Ok(AskTurnRow {
                id: r.get(0)?,
                meeting_id: r.get(1)?,
                question: r.get(2)?,
                answer: r.get(3)?,
                at_ms: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(turns)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::time::Duration;

    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, SpeakerId, TranscriptSegment};

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
            &[seg(0, AudioSourceKind::Microphone, None, "Ship it Friday.")],
        )
        .unwrap();
        lib
    }

    /// A stored turn for `meeting`, used here and by the retention drift tests.
    pub(crate) fn turn(meeting: &str, at_ms: i64) -> AskTurnRow {
        AskTurnRow {
            id: 0,
            meeting_id: meeting.into(),
            question: "When do we ship?".into(),
            answer: r#"{"answer":"Friday.","citations":[],"grounded":true}"#.into(),
            at_ms,
        }
    }

    fn turn_count(lib: &Library) -> i64 {
        lib.conn
            .query_row("SELECT count(*) FROM ask_turn", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn turns_are_listed_oldest_first() {
        let lib = library_with_meeting();
        lib.insert_ask_turn(&turn("m1", 20)).unwrap();
        lib.insert_ask_turn(&turn("m1", 10)).unwrap();
        let turns = lib.ask_turns("m1").unwrap();
        assert_eq!(turns.iter().map(|t| t.at_ms).collect::<Vec<_>>(), [10, 20]);
        assert_eq!(turns[0].question, "When do we ship?");
    }

    #[test]
    fn a_turn_needs_a_saved_meeting() {
        let lib = library_with_meeting();
        assert_eq!(
            lib.insert_ask_turn(&turn("unsaved", 1)).unwrap(),
            0,
            "a turn needs a saved meeting"
        );
        assert_eq!(turn_count(&lib), 0);
    }

    #[test]
    fn turns_go_with_the_meeting() {
        let lib = library_with_meeting();
        lib.insert_ask_turn(&turn("m1", 10)).unwrap();
        assert!(lib.delete_note("m1").unwrap());
        assert_eq!(turn_count(&lib), 0, "cascade clears the thread");
    }

    #[test]
    fn turns_are_pruned_with_the_transcript() {
        let mut lib = library_with_meeting();
        lib.insert_ask_turn(&turn("m1", 10)).unwrap();
        let expiry: i64 = lib
            .conn
            .query_row(
                "SELECT transcript_expires_at_ms FROM meeting WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        lib.prune(expiry - 1, Path::new("/nonexistent")).unwrap();
        assert_eq!(turn_count(&lib), 1, "kept while the transcript is");
        lib.prune(expiry, Path::new("/nonexistent")).unwrap();
        assert_eq!(turn_count(&lib), 0);
        assert!(lib.get_note("m1").unwrap().is_some(), "the meeting stays");
        assert_eq!(
            lib.insert_ask_turn(&turn("m1", 30)).unwrap(),
            0,
            "a turn is not stored once the transcript is pruned"
        );
    }

    #[test]
    fn an_answer_can_be_replaced_in_place() {
        let lib = library_with_meeting();
        let id = lib.insert_ask_turn(&turn("m1", 10)).unwrap();
        assert!(lib.set_ask_answer(id, r#"{"answer":"Monday."}"#).unwrap());
        assert_eq!(
            lib.ask_turns("m1").unwrap()[0].answer,
            r#"{"answer":"Monday."}"#
        );
        assert!(!lib.set_ask_answer(id + 1, "x").unwrap(), "no such turn");
    }

    #[test]
    fn clearing_removes_a_threads_turns() {
        let lib = library_with_meeting();
        lib.insert_ask_turn(&turn("m1", 10)).unwrap();
        lib.insert_ask_turn(&turn("m1", 20)).unwrap();
        assert_eq!(lib.clear_ask_turns("m1").unwrap(), 2);
        assert!(lib.ask_turns("m1").unwrap().is_empty());
    }
}

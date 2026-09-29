//! Speaker names per meeting, and folding one diarized speaker into another.
//!
//! Segments keep their diarized ids; names live beside them in `speaker_name`, so a rename is one row
//! and needs no reindex. Names go with the meeting on delete (`ON DELETE CASCADE`).

use rusqlite::Connection;
use wisp_core::speakers::SpeakerNames;

use crate::store::Library;
use crate::Result;

impl Library {
    /// The names the user gave this meeting's speakers. Empty when none (or no such meeting).
    pub fn speaker_names(&self, meeting_id: &str) -> Result<SpeakerNames> {
        read_names(&self.conn, meeting_id)
    }

    /// Names one speaker of a meeting. A blank name clears it instead.
    pub fn set_speaker_name(&self, meeting_id: &str, speaker: u32, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return self.clear_speaker_name(meeting_id, speaker);
        }
        self.conn.execute(
            "INSERT INTO speaker_name (meeting_id, speaker, name) VALUES (?1, ?2, ?3)
             ON CONFLICT (meeting_id, speaker) DO UPDATE SET name = excluded.name",
            rusqlite::params![meeting_id, speaker, name],
        )?;
        Ok(())
    }

    /// Replaces all of a meeting's speaker names with `names` (blank names are dropped).
    pub fn set_speaker_names(&mut self, meeting_id: &str, names: &SpeakerNames) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM speaker_name WHERE meeting_id = ?1",
            [meeting_id],
        )?;
        write_names(&tx, meeting_id, names)?;
        tx.commit()?;
        Ok(())
    }

    /// Removes one speaker's name, so it reads as `Speaker N` again.
    pub fn clear_speaker_name(&self, meeting_id: &str, speaker: u32) -> Result<()> {
        self.conn.execute(
            "DELETE FROM speaker_name WHERE meeting_id = ?1 AND speaker = ?2",
            rusqlite::params![meeting_id, speaker],
        )?;
        Ok(())
    }

    /// Folds speaker `from` into `into`: every segment `from` spoke becomes `into`'s. `from`'s name
    /// moves to `into` if `into` has none, else it is dropped. For when diarization split one
    /// person in two. Returns the segments moved.
    pub fn merge_speaker(&mut self, meeting_id: &str, from: u32, into: u32) -> Result<usize> {
        if from == into {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let moved = tx.execute(
            "UPDATE segment SET speaker = ?3 WHERE meeting_id = ?1 AND speaker = ?2",
            rusqlite::params![meeting_id, from, into],
        )?;
        // Keep the name the user gave, unless the speaker it merges into is already named.
        tx.execute(
            "UPDATE OR IGNORE speaker_name SET speaker = ?3 WHERE meeting_id = ?1 AND speaker = ?2",
            rusqlite::params![meeting_id, from, into],
        )?;
        tx.execute(
            "DELETE FROM speaker_name WHERE meeting_id = ?1 AND speaker = ?2",
            rusqlite::params![meeting_id, from],
        )?;
        tx.commit()?;
        Ok(moved)
    }
}

/// A meeting's names, read on any connection or transaction.
pub(crate) fn read_names(conn: &Connection, meeting_id: &str) -> Result<SpeakerNames> {
    let mut stmt = conn.prepare("SELECT speaker, name FROM speaker_name WHERE meeting_id = ?1")?;
    let names = stmt
        .query_map([meeting_id], |r| Ok((r.get::<_, u32>(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<SpeakerNames>>()?;
    Ok(names)
}

/// Inserts `names` for a meeting that has none; blank names are skipped.
pub(crate) fn write_names(conn: &Connection, meeting_id: &str, names: &SpeakerNames) -> Result<()> {
    let mut stmt =
        conn.prepare("INSERT INTO speaker_name (meeting_id, speaker, name) VALUES (?1, ?2, ?3)")?;
    for (speaker, name) in names {
        let name = name.trim();
        if !name.is_empty() {
            stmt.execute(rusqlite::params![meeting_id, speaker, name])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, SpeakerId, TranscriptSegment};

    use super::*;

    fn seg(id: u64, speaker: Option<u32>) -> TranscriptSegment {
        TranscriptSegment {
            id,
            text: format!("line {id}"),
            start: Duration::from_millis(id * 1000),
            end: Duration::from_millis(id * 1000 + 500),
            status: SegmentStatus::Final,
            source: AudioSourceKind::System,
            speaker: speaker.map(SpeakerId),
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    fn library_with_meeting() -> Library {
        let mut lib = Library::open_in_memory().unwrap();
        let segs = [
            seg(0, Some(0)),
            seg(1, Some(1)),
            seg(2, Some(0)),
            seg(3, None),
        ];
        lib.save_note("m1", &MeetingMeta::default(), 0, &segs)
            .unwrap();
        lib
    }

    fn speakers(lib: &Library) -> Vec<Option<i64>> {
        let (_, rows) = lib.get_note("m1").unwrap().unwrap();
        rows.iter().map(|r| r.speaker).collect()
    }

    #[test]
    fn set_get_and_clear_names() {
        let lib = library_with_meeting();
        assert!(lib.speaker_names("m1").unwrap().is_empty());

        lib.set_speaker_name("m1", 0, " Alice ").unwrap();
        lib.set_speaker_name("m1", 1, "Bob").unwrap();
        lib.set_speaker_name("m1", 1, "Robert").unwrap();
        let names = lib.speaker_names("m1").unwrap();
        assert_eq!(names.get(&0).map(String::as_str), Some("Alice"));
        assert_eq!(names.get(&1).map(String::as_str), Some("Robert"));

        lib.clear_speaker_name("m1", 0).unwrap();
        lib.set_speaker_name("m1", 1, "   ").unwrap();
        assert!(lib.speaker_names("m1").unwrap().is_empty());
    }

    #[test]
    fn set_names_replaces_the_whole_map() {
        let mut lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        let next: SpeakerNames = [(1, "Bob".to_owned()), (2, " ".to_owned())]
            .into_iter()
            .collect();
        lib.set_speaker_names("m1", &next).unwrap();
        let names = lib.speaker_names("m1").unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&1).map(String::as_str), Some("Bob"));
    }

    #[test]
    fn naming_an_unknown_meeting_fails() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.set_speaker_name("missing", 0, "Alice").is_err());
    }

    #[test]
    fn merge_moves_segments_and_drops_the_old_name() {
        let mut lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        lib.set_speaker_name("m1", 1, "Al").unwrap();

        assert_eq!(lib.merge_speaker("m1", 1, 0).unwrap(), 1);
        assert_eq!(speakers(&lib), vec![Some(0), Some(0), Some(0), None]);
        let names = lib.speaker_names("m1").unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&0).map(String::as_str), Some("Alice"));
    }

    #[test]
    fn merging_into_an_unnamed_speaker_keeps_the_name() {
        let mut lib = library_with_meeting();
        lib.set_speaker_name("m1", 1, "Don").unwrap();
        lib.merge_speaker("m1", 1, 0).unwrap();
        let names = lib.speaker_names("m1").unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&0).map(String::as_str), Some("Don"));
    }

    #[test]
    fn merging_into_itself_changes_nothing() {
        let mut lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        assert_eq!(lib.merge_speaker("m1", 0, 0).unwrap(), 0);
        assert_eq!(lib.speaker_names("m1").unwrap().len(), 1);
    }

    #[test]
    fn merge_touches_only_its_meeting() {
        let mut lib = library_with_meeting();
        lib.save_note("m2", &MeetingMeta::default(), 0, &[seg(0, Some(1))])
            .unwrap();
        lib.merge_speaker("m1", 1, 0).unwrap();
        let (_, rows) = lib.get_note("m2").unwrap().unwrap();
        assert_eq!(rows[0].speaker, Some(1));
    }

    #[test]
    fn names_go_with_the_meeting() {
        let lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        assert!(lib.delete_note("m1").unwrap());
        let left: i64 = lib
            .conn
            .query_row("SELECT count(*) FROM speaker_name", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn resaving_a_meeting_keeps_its_names() {
        let mut lib = library_with_meeting();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        lib.save_note("m1", &MeetingMeta::default(), 0, &[seg(0, Some(0))])
            .unwrap();
        assert_eq!(
            lib.speaker_names("m1").unwrap().get(&0).map(String::as_str),
            Some("Alice")
        );
    }
}

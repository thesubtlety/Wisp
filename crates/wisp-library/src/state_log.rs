//! Storage for a meeting's state log: the ordered operations the intelligence layer applied, as
//! opaque JSON. The library doesn't interpret them; it keeps them apart from the transcript, with
//! their own lifetime (see `SCHEMA_V5`).

use serde::{Deserialize, Serialize};

use crate::store::Library;
use crate::Result;

/// One stored state operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredOp {
    pub seq: i64,
    pub at_ms: i64,
    /// The operation, serialized by its writer.
    pub op: String,
}

impl Library {
    /// Replaces a meeting's whole state log with `ops`, in one transaction.
    pub fn save_state_ops(&mut self, meeting_id: &str, ops: &[StoredOp]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM state_op WHERE meeting_id = ?1", [meeting_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO state_op (meeting_id, seq, at_ms, op) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for op in ops {
                stmt.execute(rusqlite::params![meeting_id, op.seq, op.at_ms, op.op])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A meeting's state log in order; empty if it has none.
    pub fn state_ops(&self, meeting_id: &str) -> Result<Vec<StoredOp>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, at_ms, op FROM state_op WHERE meeting_id = ?1 ORDER BY seq")?;
        let rows = stmt
            .query_map([meeting_id], |r| {
                Ok(StoredOp {
                    seq: r.get(0)?,
                    at_ms: r.get(1)?,
                    op: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptSegment};

    fn op(seq: i64) -> StoredOp {
        StoredOp {
            seq,
            at_ms: 100 + seq,
            op: format!("{{\"n\":{seq}}}"),
        }
    }

    fn save_meeting(lib: &mut Library, id: &str, started_at_ms: i64) {
        let seg = TranscriptSegment {
            id: 0,
            text: "hello".into(),
            start: Duration::ZERO,
            end: Duration::from_millis(500),
            status: SegmentStatus::Final,
            source: AudioSourceKind::System,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        };
        let meta = MeetingMeta {
            title: Some(id.into()),
            date: None,
            engine: None,
            language: None,
            summary: None,
        };
        lib.save_note(id, &meta, started_at_ms, &[seg]).unwrap();
    }

    #[test]
    fn a_log_is_replaced_whole_and_read_back_in_order() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.save_state_ops("m", &[op(1), op(0), op(2)]).unwrap();
        assert_eq!(lib.state_ops("m").unwrap(), [op(0), op(1), op(2)]);
        lib.save_state_ops("m", &[op(0)]).unwrap();
        assert_eq!(lib.state_ops("m").unwrap(), [op(0)]);
        assert!(lib.state_ops("other").unwrap().is_empty());
        // A duplicate seq fails and leaves the old log.
        assert!(lib.save_state_ops("m", &[op(3), op(3)]).is_err());
        assert_eq!(lib.state_ops("m").unwrap(), [op(0)]);
    }

    #[test]
    fn the_log_outlives_resave_and_transcript_prune_but_not_deletion() {
        let mut lib = Library::open_in_memory().unwrap();
        save_meeting(&mut lib, "m", 0);
        lib.save_state_ops("m", &[op(0), op(1)]).unwrap();

        save_meeting(&mut lib, "m", 0); // re-save replaces the meeting row
        assert_eq!(lib.state_ops("m").unwrap().len(), 2);

        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            lib.prune(i64::MAX, root.path())
                .unwrap()
                .transcripts_expired,
            1
        );
        assert_eq!(
            lib.state_ops("m").unwrap().len(),
            2,
            "derived state is long-lived"
        );

        assert!(lib.delete_note("m").unwrap());
        assert!(lib.state_ops("m").unwrap().is_empty());
    }

    #[test]
    fn deleting_a_project_removes_its_meetings_logs() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "P", 0).unwrap();
        save_meeting(&mut lib, "in", 0);
        save_meeting(&mut lib, "out", 0);
        lib.set_meeting_project("in", Some("p")).unwrap();
        lib.save_state_ops("in", &[op(0)]).unwrap();
        lib.save_state_ops("out", &[op(0)]).unwrap();
        let root = tempfile::tempdir().unwrap();
        lib.delete_project("p", root.path()).unwrap();
        assert!(lib.state_ops("in").unwrap().is_empty());
        assert_eq!(lib.state_ops("out").unwrap().len(), 1);
    }
}

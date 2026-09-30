//! Project memory: what the user accepted as durable knowledge for a project.
//!
//! Each entry keeps where it came from as [`Provenance`]: the evidence refs, a human label for each
//! ("Meeting Sept 26, 18:31"), and a SHA-256 of the evidence text. That survives the evidence being
//! deleted by retention without keeping the words themselves; [`Library::snippet_for_ref`] tells
//! whether the raw source still exists.

use serde::{Deserialize, Serialize};

use crate::store::Library;
use crate::Result;

/// Where a piece of project knowledge came from, kept after the source itself expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceRef {
    /// The canonical evidence ref (`M<meeting>:T<segment>`, `S<source>:C<chunk>`).
    pub source_ref: String,
    /// What it was, for display after the text is gone.
    pub label: String,
    /// SHA-256 of the evidence text, hex. Proves what it said without keeping it.
    pub sha256: String,
}

/// A new memory entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInput {
    /// `fact`, `requirement`, `decision`, `person`, `open_issue`.
    pub kind: String,
    pub text: String,
    /// `stated` or `inferred`.
    pub status: String,
    pub confidence: f64,
    pub provenance: Vec<ProvenanceRef>,
    /// The meeting it was learned from, if any.
    pub meeting_id: Option<String>,
}

/// A stored memory entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: i64,
    pub project_id: String,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub confidence: f64,
    pub provenance: Vec<ProvenanceRef>,
    pub meeting_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// How a project's knowledge relates to one meeting: entries learned only from it, and entries
/// that also cite other meetings or sources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingKnowledge {
    /// Entries whose evidence is all from the meeting. These can move with it.
    pub exclusive: usize,
    /// Entries citing the meeting and something else. These stay in the project.
    pub shared: usize,
}

/// The meeting a `M<meeting>:T<segment>` ref points into, if it is one.
fn ref_meeting(source_ref: &str) -> Option<&str> {
    source_ref
        .strip_prefix('M')?
        .rsplit_once(":T")
        .map(|(m, _)| m)
}

/// Whether `entry` cites `meeting_id` at all, and whether that is all it cites.
fn cites(entry: &MemoryEntry, meeting_id: &str) -> (bool, bool) {
    let from_meeting = entry.meeting_id.as_deref() == Some(meeting_id);
    let refs_here = entry
        .provenance
        .iter()
        .filter(|p| ref_meeting(&p.source_ref) == Some(meeting_id))
        .count();
    let related = from_meeting || refs_here > 0;
    let only = related
        && refs_here == entry.provenance.len()
        && entry.meeting_id.as_deref().is_none_or(|m| m == meeting_id);
    (related, only)
}

impl Library {
    /// How `project_id`'s knowledge relates to `meeting_id` (see [`MeetingKnowledge`]).
    pub fn meeting_knowledge(
        &self,
        meeting_id: &str,
        project_id: &str,
    ) -> Result<MeetingKnowledge> {
        let mut out = MeetingKnowledge::default();
        for entry in self.list_memory(project_id)? {
            match cites(&entry, meeting_id) {
                (true, true) => out.exclusive += 1,
                (true, false) => out.shared += 1,
                _ => {}
            }
        }
        Ok(out)
    }

    /// Moves the knowledge learned only from `meeting_id` from one project to another. Entries that
    /// also cite other meetings or sources stay where they are. Returns how many moved.
    pub fn move_meeting_knowledge(
        &self,
        meeting_id: &str,
        from_project: &str,
        to_project: &str,
    ) -> Result<usize> {
        if from_project == to_project {
            return Ok(0);
        }
        let ids: Vec<i64> = self
            .list_memory(from_project)?
            .into_iter()
            .filter(|e| cites(e, meeting_id) == (true, true))
            .map(|e| e.id)
            .collect();
        let tx = self.conn.unchecked_transaction()?;
        for id in &ids {
            tx.execute(
                "UPDATE project_memory SET project_id = ?2 WHERE id = ?1",
                rusqlite::params![id, to_project],
            )?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// Stores accepted knowledge for a project. Returns its id.
    pub fn add_memory(&self, project_id: &str, input: &MemoryInput, now_ms: i64) -> Result<i64> {
        let provenance = serde_json::to_string(&input.provenance)
            .map_err(|e| crate::LibraryError::Embed(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO project_memory
                 (project_id, kind, text, status, confidence, provenance, meeting_id, created_at_ms,
                  updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            rusqlite::params![
                project_id,
                input.kind,
                input.text.trim(),
                input.status,
                input.confidence,
                provenance,
                input.meeting_id,
                now_ms,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A project's memory, oldest first.
    pub fn list_memory(&self, project_id: &str) -> Result<Vec<MemoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, kind, text, status, confidence, provenance, meeting_id,
                    created_at_ms, updated_at_ms
             FROM project_memory WHERE project_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([project_id], |r| {
                let provenance: String = r.get(6)?;
                Ok(MemoryEntry {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                    status: r.get(4)?,
                    confidence: r.get(5)?,
                    provenance: serde_json::from_str(&provenance).unwrap_or_default(),
                    meeting_id: r.get(7)?,
                    created_at_ms: r.get(8)?,
                    updated_at_ms: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replaces an entry's text. `false` if there is no such entry.
    pub fn update_memory_text(&self, id: i64, text: &str, now_ms: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE project_memory SET text = ?2, updated_at_ms = ?3 WHERE id = ?1",
            rusqlite::params![id, text.trim(), now_ms],
        )? > 0)
    }

    /// Deletes an entry. `false` if there is no such entry.
    pub fn delete_memory(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM project_memory WHERE id = ?1", [id])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> MemoryInput {
        MemoryInput {
            kind: "requirement".into(),
            text: text.into(),
            status: "stated".into(),
            confidence: 0.95,
            provenance: vec![ProvenanceRef {
                source_ref: "Mm1:T4".into(),
                label: "Meeting Sept 26, 18:31".into(),
                sha256: "ab".repeat(32),
            }],
            meeting_id: Some("m1".into()),
        }
    }

    #[test]
    fn memory_is_stored_listed_edited_and_deleted() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        let a = lib
            .add_memory("p", &input("  Production runs in Azure "), 5)
            .unwrap();
        let b = lib
            .add_memory("p", &input("Sarah owns security"), 6)
            .unwrap();
        let list = lib.list_memory("p").unwrap();
        assert_eq!(list.iter().map(|m| m.id).collect::<Vec<_>>(), [a, b]);
        assert_eq!(list[0].text, "Production runs in Azure");
        assert_eq!(list[0].provenance[0].label, "Meeting Sept 26, 18:31");
        assert!(lib
            .update_memory_text(a, "Production runs in Azure (EU)", 9)
            .unwrap());
        assert_eq!(lib.list_memory("p").unwrap()[0].updated_at_ms, 9);
        assert!(lib.delete_memory(b).unwrap());
        assert!(!lib.delete_memory(b).unwrap());
        assert_eq!(lib.list_memory("p").unwrap().len(), 1);
        assert!(
            lib.add_memory("nope", &input("x"), 0).is_err(),
            "project must exist"
        );
    }

    fn cited(text: &str, refs: &[&str], meeting: Option<&str>) -> MemoryInput {
        MemoryInput {
            provenance: refs
                .iter()
                .map(|r| ProvenanceRef {
                    source_ref: (*r).into(),
                    label: "x".into(),
                    sha256: "ab".repeat(32),
                })
                .collect(),
            meeting_id: meeting.map(Into::into),
            ..input(text)
        }
    }

    #[test]
    fn only_knowledge_learned_solely_from_the_meeting_moves_with_it() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", 0).unwrap();
        lib.create_project("b", "Beta", 0).unwrap();
        let only = lib
            .add_memory(
                "a",
                &cited("Runs in Azure", &["Mm1:T4", "Mm1:T9"], Some("m1")),
                1,
            )
            .unwrap();
        let no_meeting_field = lib
            .add_memory("a", &cited("EU only", &["Mm1:T2"], None), 1)
            .unwrap();
        lib.add_memory(
            "a",
            &cited("SSO via Okta", &["Mm1:T5", "Mm2:T1"], Some("m1")),
            1,
        )
        .unwrap();
        lib.add_memory(
            "a",
            &cited("Budget is fixed", &["Mm1:T6", "S3:C0"], Some("m1")),
            1,
        )
        .unwrap();
        lib.add_memory(
            "a",
            &cited("Sarah owns security", &["Mm2:T3"], Some("m2")),
            1,
        )
        .unwrap();

        assert_eq!(
            lib.meeting_knowledge("m1", "a").unwrap(),
            MeetingKnowledge {
                exclusive: 2,
                shared: 2
            }
        );
        assert_eq!(lib.move_meeting_knowledge("m1", "a", "b").unwrap(), 2);
        let moved: Vec<i64> = lib.list_memory("b").unwrap().iter().map(|m| m.id).collect();
        assert_eq!(moved, [only, no_meeting_field]);
        let stayed: Vec<String> = lib
            .list_memory("a")
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect();
        assert_eq!(
            stayed,
            ["SSO via Okta", "Budget is fixed", "Sarah owns security"]
        );
        assert_eq!(
            lib.meeting_knowledge("m1", "a").unwrap(),
            MeetingKnowledge {
                exclusive: 0,
                shared: 2
            }
        );
        assert_eq!(lib.move_meeting_knowledge("m1", "a", "b").unwrap(), 0);
        assert_eq!(lib.move_meeting_knowledge("m1", "b", "b").unwrap(), 0);
        assert!(
            lib.move_meeting_knowledge("m1", "b", "nope").is_err(),
            "the target project must exist"
        );
        assert_eq!(
            lib.list_memory("b").unwrap().len(),
            2,
            "a failed move changes nothing"
        );
    }

    #[test]
    fn memory_outlives_its_transcript_but_not_its_project() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        lib.add_memory("p", &input("Production runs in Azure"), 5)
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        lib.prune(i64::MAX, root.path()).unwrap();
        let kept = lib.list_memory("p").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            lib.snippet_for_ref(&kept[0].provenance[0].source_ref)
                .unwrap(),
            None
        );
        lib.delete_project("p", root.path()).unwrap();
        assert!(lib.list_memory("p").unwrap().is_empty());
    }
}

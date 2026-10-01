//! Meeting types: saved profiles that tune live intelligence for a kind of meeting (see
//! [`wisp_core::meeting_types`]).
//!
//! Like prompts, the built-ins are seeded on open and can be edited, then reset, but not deleted.
//! A meeting keeps the id of the type it ran as, and a project may name a default type. Deleting a
//! user type clears both, so nothing points at a type that is gone.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use wisp_core::meeting_types::{
    builtin_meeting_type, BuiltinMeetingType, Cadence, CardStyle, BUILTIN_MEETING_TYPES,
};

use crate::store::Library;
use crate::Result;

/// A saved meeting type (one `meeting_type` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingType {
    pub id: String,
    pub name: String,
    /// When to use it, in one line. Inference reads it to tell the types apart.
    pub description: String,
    /// What live passes should watch for. Empty adds nothing to them.
    pub watch_for: String,
    /// A [`CardStyle`] name; anything else reads as `balanced`.
    pub card_style: String,
    /// A [`Cadence`] name; anything else reads as `normal`.
    pub cadence: String,
    /// What the wrap-up audit checks.
    pub wrap_checklist: Vec<String>,
    /// Extra summary sections, in order.
    pub summary_sections: Vec<String>,
    /// Saved prompts (by name) offered first after the meeting.
    pub suggested_prompts: Vec<String>,
    /// Shipped with the app: it can be reset, not deleted.
    #[serde(default)]
    pub builtin: bool,
    /// A built-in that differs from the shipped one.
    #[serde(default)]
    pub customized: bool,
    #[serde(default)]
    pub updated_at_ms: i64,
}

impl MeetingType {
    /// The shipped version of a built-in.
    pub fn from_builtin(b: &BuiltinMeetingType) -> Self {
        let list = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect();
        Self {
            id: b.id.to_owned(),
            name: b.name.to_owned(),
            description: b.description.to_owned(),
            watch_for: b.watch_for.to_owned(),
            card_style: b.card_style.as_str().to_owned(),
            cadence: b.cadence.as_str().to_owned(),
            wrap_checklist: list(b.wrap_checklist),
            summary_sections: list(b.summary_sections),
            suggested_prompts: list(b.suggested_prompts),
            builtin: true,
            customized: false,
            updated_at_ms: 0,
        }
    }

    /// The cadence, `normal` when the stored name is unknown.
    pub fn cadence(&self) -> Cadence {
        Cadence::parse(&self.cadence).unwrap_or(Cadence::Normal)
    }

    /// The card style, `balanced` when the stored name is unknown.
    pub fn card_style(&self) -> CardStyle {
        CardStyle::parse(&self.card_style).unwrap_or(CardStyle::Balanced)
    }

    /// The same type with text trimmed, blank list lines dropped, and unknown knob names replaced
    /// by the defaults. What the store writes.
    pub fn cleaned(&self) -> Self {
        let list = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Self {
            name: self.name.trim().to_owned(),
            description: self.description.trim().to_owned(),
            watch_for: self.watch_for.trim().to_owned(),
            card_style: self.card_style().as_str().to_owned(),
            cadence: self.cadence().as_str().to_owned(),
            wrap_checklist: list(&self.wrap_checklist),
            summary_sections: list(&self.summary_sections),
            suggested_prompts: list(&self.suggested_prompts),
            ..self.clone()
        }
    }

    /// Whether the user-editable fields equal `other`'s.
    fn same_content(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.watch_for == other.watch_for
            && self.card_style == other.card_style
            && self.cadence == other.cadence
            && self.wrap_checklist == other.wrap_checklist
            && self.summary_sections == other.summary_sections
            && self.suggested_prompts == other.suggested_prompts
    }
}

const COLUMNS: &str = "id, name, description, watch_for, card_style, cadence, wrap_checklist, \
                       summary_sections, suggested_prompts, builtin, updated_at_ms";

fn list_column(r: &rusqlite::Row, idx: usize) -> rusqlite::Result<Vec<String>> {
    let text: String = r.get(idx)?;
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

fn list_json(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

fn type_row(r: &rusqlite::Row) -> rusqlite::Result<MeetingType> {
    let mut t = MeetingType {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        watch_for: r.get(3)?,
        card_style: r.get(4)?,
        cadence: r.get(5)?,
        wrap_checklist: list_column(r, 6)?,
        summary_sections: list_column(r, 7)?,
        suggested_prompts: list_column(r, 8)?,
        builtin: r.get::<_, i64>(9)? != 0,
        customized: false,
        updated_at_ms: r.get(10)?,
    };
    t.customized = t.builtin
        && builtin_meeting_type(&t.id)
            .is_some_and(|b| !t.same_content(&MeetingType::from_builtin(b)));
    Ok(t)
}

impl Library {
    /// Adds any built-in meeting type that is missing. Never touches one that exists, so the user's
    /// edits stay. Idempotent; run on every open.
    pub(crate) fn seed_meeting_types(&self) -> Result<()> {
        for b in BUILTIN_MEETING_TYPES {
            self.write_type(&MeetingType::from_builtin(b), true, 0, "INSERT OR IGNORE")?;
        }
        Ok(())
    }

    fn write_type(&self, t: &MeetingType, builtin: bool, now_ms: i64, verb: &str) -> Result<usize> {
        let t = t.cleaned();
        Ok(self.conn.execute(
            &format!(
                "{verb} INTO meeting_type ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            rusqlite::params![
                t.id,
                t.name,
                t.description,
                t.watch_for,
                t.card_style,
                t.cadence,
                list_json(&t.wrap_checklist),
                list_json(&t.summary_sections),
                list_json(&t.suggested_prompts),
                builtin,
                now_ms
            ],
        )?)
    }

    /// Every meeting type: the built-ins in shipped order (General first), then the user's, oldest
    /// first.
    pub fn list_meeting_types(&self) -> Result<Vec<MeetingType>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM meeting_type WHERE builtin = 0 ORDER BY rowid"
        ))?;
        let users = stmt
            .query_map([], type_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for b in BUILTIN_MEETING_TYPES {
            if let Some(t) = self.get_meeting_type(b.id)? {
                out.push(t);
            }
        }
        out.extend(users);
        Ok(out)
    }

    /// One meeting type by id.
    pub fn get_meeting_type(&self, id: &str) -> Result<Option<MeetingType>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM meeting_type WHERE id = ?1"),
                [id],
                type_row,
            )
            .optional()?)
    }

    /// Adds a user meeting type under its caller-generated `id`, cleaned (see
    /// [`MeetingType::cleaned`]).
    pub fn create_meeting_type(&self, t: &MeetingType, now_ms: i64) -> Result<()> {
        self.write_type(t, false, now_ms, "INSERT")?;
        Ok(())
    }

    /// Replaces a meeting type's fields (cleaned); whether it is built-in stays. Returns whether it
    /// exists.
    pub fn update_meeting_type(&self, t: &MeetingType, now_ms: i64) -> Result<bool> {
        let t = t.cleaned();
        let n = self.conn.execute(
            "UPDATE meeting_type SET name = ?2, description = ?3, watch_for = ?4, card_style = ?5,
                 cadence = ?6, wrap_checklist = ?7, summary_sections = ?8, suggested_prompts = ?9,
                 updated_at_ms = ?10
             WHERE id = ?1",
            rusqlite::params![
                t.id,
                t.name,
                t.description,
                t.watch_for,
                t.card_style,
                t.cadence,
                list_json(&t.wrap_checklist),
                list_json(&t.summary_sections),
                list_json(&t.suggested_prompts),
                now_ms
            ],
        )?;
        Ok(n > 0)
    }

    /// Deletes a user meeting type, and clears it from meetings and project defaults. Built-ins are
    /// never deleted. Returns whether one was deleted.
    pub fn delete_meeting_type(&mut self, id: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM meeting_type WHERE id = ?1 AND builtin = 0",
            [id],
        )?;
        if n > 0 {
            tx.execute("UPDATE meeting SET type_id = NULL WHERE type_id = ?1", [id])?;
            tx.execute(
                "UPDATE project SET default_type_id = NULL WHERE default_type_id = ?1",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Puts a built-in back to its shipped version. Returns whether `id` is a built-in.
    pub fn reset_meeting_type(&self, id: &str, now_ms: i64) -> Result<bool> {
        let Some(b) = builtin_meeting_type(id) else {
            return Ok(false);
        };
        self.write_type(
            &MeetingType::from_builtin(b),
            true,
            now_ms,
            "INSERT OR REPLACE",
        )?;
        Ok(true)
    }

    /// The type a saved meeting ran as, if recorded.
    pub fn meeting_type_id(&self, meeting_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT type_id FROM meeting WHERE id = ?1",
                [meeting_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Records (or with `None` clears) the type a saved meeting ran as. Returns whether the meeting
    /// exists.
    pub fn set_meeting_type(&self, meeting_id: &str, type_id: Option<&str>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE meeting SET type_id = ?2 WHERE id = ?1",
            rusqlite::params![meeting_id, type_id],
        )?;
        Ok(n > 0)
    }

    /// A project's default meeting type, if it names one.
    pub fn project_default_type(&self, project_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT default_type_id FROM project WHERE id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Sets (or with `None` clears) a project's default meeting type. Returns whether the project
    /// exists.
    pub fn set_project_default_type(
        &self,
        project_id: &str,
        type_id: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE project SET default_type_id = ?2 WHERE id = ?1",
            rusqlite::params![project_id, type_id],
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use wisp_core::export::MeetingMeta;
    use wisp_core::meeting_types::GENERAL_TYPE_ID;

    use super::*;

    fn custom(id: &str) -> MeetingType {
        MeetingType {
            id: id.into(),
            name: "  Board meeting ".into(),
            description: "Quarterly board".into(),
            watch_for: "Votes and resolutions".into(),
            card_style: "risks".into(),
            cadence: "warp".into(),
            wrap_checklist: vec!["Minutes taker named".into(), "  ".into()],
            summary_sections: vec!["Resolutions".into()],
            suggested_prompts: vec!["Action items".into()],
            builtin: false,
            customized: false,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn builtins_are_seeded_once_and_edits_survive_a_reseed_until_reset() {
        let lib = Library::open_in_memory().unwrap();
        let types = lib.list_meeting_types().unwrap();
        assert_eq!(types.len(), BUILTIN_MEETING_TYPES.len());
        assert_eq!(types[0].id, GENERAL_TYPE_ID, "General first");
        assert!(types.iter().all(|t| t.builtin && !t.customized));

        let mut interview = lib.get_meeting_type("builtin-interview").unwrap().unwrap();
        assert_eq!(interview.cadence(), Cadence::Fast);
        interview.cadence = "calm".into();
        interview
            .wrap_checklist
            .push("Salary range discussed".into());
        assert!(lib.update_meeting_type(&interview, 5).unwrap());
        lib.seed_meeting_types().unwrap();
        let edited = lib.get_meeting_type("builtin-interview").unwrap().unwrap();
        assert!(edited.customized && edited.builtin);
        assert_eq!(edited.cadence(), Cadence::Calm);
        assert_eq!(edited.wrap_checklist.len(), 4);

        assert!(lib.reset_meeting_type("builtin-interview", 9).unwrap());
        let reset = lib.get_meeting_type("builtin-interview").unwrap().unwrap();
        assert!(!reset.customized);
        assert_eq!(reset.cadence(), Cadence::Fast);
    }

    #[test]
    fn user_types_are_cleaned_and_deletable_but_builtins_are_not() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_meeting_type(&custom("u1"), 1).unwrap();
        let mine = lib.list_meeting_types().unwrap().pop().unwrap();
        assert_eq!(mine.id, "u1");
        assert_eq!(mine.name, "Board meeting");
        assert_eq!(mine.cadence, "normal", "unknown cadence reads as normal");
        assert_eq!(mine.card_style(), CardStyle::Risks);
        assert_eq!(mine.wrap_checklist, ["Minutes taker named"]);
        assert!(!mine.builtin && !mine.customized);
        assert!(!lib.reset_meeting_type("u1", 2).unwrap());
        assert!(!lib.delete_meeting_type(GENERAL_TYPE_ID).unwrap());
        assert!(lib.delete_meeting_type("u1").unwrap());
        assert!(lib.get_meeting_type("u1").unwrap().is_none());
        assert!(!lib.update_meeting_type(&custom("u1"), 3).unwrap());
    }

    #[test]
    fn a_meeting_keeps_its_type_across_a_resave_and_loses_a_deleted_one() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.save_note("m1", &MeetingMeta::default(), 0, &[])
            .unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        lib.create_meeting_type(&custom("u1"), 1).unwrap();
        assert!(lib.set_meeting_type("m1", Some("u1")).unwrap());
        assert!(lib.set_project_default_type("p", Some("u1")).unwrap());
        assert!(!lib.set_meeting_type("missing", Some("u1")).unwrap());

        lib.save_note("m1", &MeetingMeta::default(), 0, &[])
            .unwrap();
        assert_eq!(lib.meeting_type_id("m1").unwrap().as_deref(), Some("u1"));
        assert_eq!(
            lib.project_default_type("p").unwrap().as_deref(),
            Some("u1")
        );

        lib.delete_meeting_type("u1").unwrap();
        assert_eq!(lib.meeting_type_id("m1").unwrap(), None);
        assert_eq!(lib.project_default_type("p").unwrap(), None);
        assert_eq!(lib.meeting_type_id("missing").unwrap(), None);
    }

    #[test]
    fn a_v12_database_gains_meeting_types_and_keeps_its_meetings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let mut lib = Library::open(&path).unwrap();
            lib.save_note("m1", &MeetingMeta::default(), 0, &[])
                .unwrap();
            lib.create_project("p", "Acme", 0).unwrap();
            lib.conn
                .execute_batch(&format!(
                    "{}PRAGMA user_version = 12;",
                    crate::store::DROP_V13
                ))
                .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let version: i64 = lib
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 13);
        assert_eq!(lib.count().unwrap(), 1);
        assert_eq!(
            lib.list_meeting_types().unwrap().len(),
            BUILTIN_MEETING_TYPES.len()
        );
        assert_eq!(lib.meeting_type_id("m1").unwrap(), None);
        assert_eq!(lib.project_default_type("p").unwrap(), None);
        assert!(lib.set_meeting_type("m1", Some(GENERAL_TYPE_ID)).unwrap());
    }
}

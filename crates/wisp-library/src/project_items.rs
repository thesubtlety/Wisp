//! Project items the user added by hand: a commitment, open question, decision or risk that
//! belongs to the project, not to one meeting. Stored as plain text fields; the intelligence layer
//! decides which kinds and lifecycles are valid. They are user-authored, so transcript retention
//! never touches them; they go with their project (see `SCHEMA_V12`).

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::store::Library;
use crate::Result;

/// The fields of a project item the user writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectItemInput {
    /// `commitment`, `open_question`, `decision` or `risk`.
    pub kind: String,
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    /// `active`, `resolved` and the like.
    pub lifecycle: String,
}

/// A stored project item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectItem {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub lifecycle: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

const COLUMNS: &str =
    "id, project_id, kind, text, owner, due, lifecycle, created_at_ms, updated_at_ms";

fn row(r: &rusqlite::Row) -> rusqlite::Result<ProjectItem> {
    Ok(ProjectItem {
        id: r.get(0)?,
        project_id: r.get(1)?,
        kind: r.get(2)?,
        text: r.get(3)?,
        owner: r.get(4)?,
        due: r.get(5)?,
        lifecycle: r.get(6)?,
        created_at_ms: r.get(7)?,
        updated_at_ms: r.get(8)?,
    })
}

impl Library {
    /// Adds an item to a project under the caller's `id`.
    pub fn add_project_item(
        &self,
        project_id: &str,
        id: &str,
        input: &ProjectItemInput,
        now_ms: i64,
    ) -> Result<ProjectItem> {
        self.conn.execute(
            "INSERT INTO project_item
                 (id, project_id, kind, text, owner, due, lifecycle, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            rusqlite::params![
                id,
                project_id,
                input.kind,
                input.text,
                input.owner,
                input.due,
                input.lifecycle,
                now_ms
            ],
        )?;
        Ok(ProjectItem {
            id: id.to_owned(),
            project_id: project_id.to_owned(),
            kind: input.kind.clone(),
            text: input.text.clone(),
            owner: input.owner.clone(),
            due: input.due.clone(),
            lifecycle: input.lifecycle.clone(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    /// A project's items, oldest first.
    pub fn list_project_items(&self, project_id: &str) -> Result<Vec<ProjectItem>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM project_item WHERE project_id = ?1
             ORDER BY created_at_ms, id"
        ))?;
        let rows = stmt
            .query_map([project_id], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One item, if it exists.
    pub fn get_project_item(&self, id: &str) -> Result<Option<ProjectItem>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM project_item WHERE id = ?1"),
                [id],
                row,
            )
            .optional()?)
    }

    /// Replaces an item's fields (its kind stays). `false` if there is no such item.
    pub fn update_project_item(
        &self,
        id: &str,
        input: &ProjectItemInput,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE project_item
             SET text = ?2, owner = ?3, due = ?4, lifecycle = ?5, updated_at_ms = ?6
             WHERE id = ?1",
            rusqlite::params![
                id,
                input.text,
                input.owner,
                input.due,
                input.lifecycle,
                now_ms
            ],
        )? > 0)
    }

    /// Deletes an item. `false` if there is no such item.
    pub fn delete_project_item(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM project_item WHERE id = ?1", [id])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, text: &str) -> ProjectItemInput {
        ProjectItemInput {
            kind: kind.into(),
            text: text.into(),
            owner: None,
            due: None,
            lifecycle: "active".into(),
        }
    }

    #[test]
    fn items_are_added_listed_updated_and_deleted() {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        lib.create_project("q", "Other", 0).unwrap();
        let a = lib
            .add_project_item("p", "pi-a", &input("risk", "Budget may slip"), 10)
            .unwrap();
        lib.add_project_item("p", "pi-b", &input("commitment", "Send the plan"), 20)
            .unwrap();
        lib.add_project_item("q", "pi-c", &input("decision", "Elsewhere"), 5)
            .unwrap();
        let listed = lib.list_project_items("p").unwrap();
        assert_eq!(
            listed.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["pi-a", "pi-b"]
        );
        assert_eq!(listed[0], a);

        let edit = ProjectItemInput {
            owner: Some("Sarah".into()),
            due: Some("Friday".into()),
            lifecycle: "resolved".into(),
            ..input("ignored", "Budget will slip")
        };
        assert!(lib.update_project_item("pi-a", &edit, 30).unwrap());
        let got = lib.get_project_item("pi-a").unwrap().unwrap();
        assert_eq!(got.kind, "risk", "the kind stays");
        assert_eq!(got.text, "Budget will slip");
        assert_eq!(
            (got.owner.as_deref(), got.due.as_deref()),
            (Some("Sarah"), Some("Friday"))
        );
        assert_eq!(got.lifecycle, "resolved");
        assert_eq!((got.created_at_ms, got.updated_at_ms), (10, 30));
        assert!(!lib.update_project_item("nope", &edit, 30).unwrap());

        assert!(lib.delete_project_item("pi-b").unwrap());
        assert!(!lib.delete_project_item("pi-b").unwrap());
        assert_eq!(lib.list_project_items("p").unwrap().len(), 1);
    }

    #[test]
    fn items_go_with_their_project_but_not_with_retention() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        lib.add_project_item("p", "pi-a", &input("risk", "Budget"), 0)
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        lib.prune(i64::MAX / 2, root.path()).unwrap();
        assert_eq!(lib.list_project_items("p").unwrap().len(), 1);
        lib.delete_project("p", root.path()).unwrap();
        assert!(lib.get_project_item("pi-a").unwrap().is_none());
        assert!(
            lib.add_project_item("gone", "pi-x", &input("risk", "x"), 0)
                .is_err(),
            "an item needs its project"
        );
    }

    #[test]
    fn a_v11_database_gains_the_item_table_and_keeps_its_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let lib = Library::open(&path).unwrap();
            lib.create_project("p", "Acme", 0).unwrap();
            lib.conn
                .execute_batch(&format!(
                    "{}DROP TABLE project_item; PRAGMA user_version = 11;",
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
        assert_eq!(lib.list_projects().unwrap().len(), 1);
        lib.add_project_item("p", "pi-a", &input("risk", "Budget"), 0)
            .unwrap();
        assert_eq!(lib.list_project_items("p").unwrap().len(), 1);
    }
}

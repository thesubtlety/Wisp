//! Hand edits: the user marks an item done, rewrites it, or deletes it after the meeting.
//!
//! An edit to a meeting's item is a [`ResolvedOp::UserEdit`] appended to that meeting's log, so the
//! state, its exports and the project brief all see it on replay, and the log keeps what the model
//! first recorded. It cites no evidence (the user is the source), so it never goes through the
//! reducer's evidence rules; [`user_edit`] checks it instead. "Delete" withdraws the item.
//!
//! Items the user adds to a project by hand ([`ProjectItemInput`]) are not in any meeting's log;
//! [`new_manual_item`] and [`edit_manual_item`] check them the same way.

use serde::{Deserialize, Serialize};
use wisp_library::{ProjectItem, ProjectItemInput};

use crate::model::{ItemKind, Lifecycle, MeetingState, StateItem};
use crate::ops::{AppliedOp, ReplayError, ResolvedOp};
use crate::reducer::{clean_field, clean_text, RejectReason};

/// The kinds of item a user can add to a project by hand.
pub const MANUAL_KINDS: [ItemKind; 4] = [
    ItemKind::Commitment,
    ItemKind::OpenQuestion,
    ItemKind::Decision,
    ItemKind::Risk,
];

/// What the user changes on an item. `None` leaves a field as it is; an empty `owner` or `due`
/// clears it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ItemChange {
    pub text: Option<String>,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub lifecycle: Option<Lifecycle>,
}

/// Why an edit was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditError {
    #[error("no item {0}")]
    UnknownItem(String),
    #[error("{0} was replaced or deleted and can't change")]
    Terminal(String),
    #[error("an item needs text")]
    EmptyText,
    #[error("text too long ({0} characters)")]
    TextTooLong(usize),
    #[error("an item can't be set to {}", .0.as_str())]
    BadLifecycle(Lifecycle),
    #[error("a project item can't be a {0}")]
    BadKind(String),
    #[error("nothing to change")]
    NoChange,
    #[error("the meeting's state log is damaged: {0}")]
    Replay(#[from] ReplayError),
}

fn text(t: &str) -> Result<String, EditError> {
    clean_text(Some(t)).map_err(|e| match e {
        RejectReason::TextTooLong(n) => EditError::TextTooLong(n),
        _ => EditError::EmptyText,
    })
}

/// A trimmed owner or due value; `""` when the user cleared it.
fn field(v: &str) -> String {
    clean_field(Some(v)).unwrap_or_default()
}

/// Lifecycles a user can set. Superseding needs a replacement, which only the model records.
fn settable(l: Lifecycle) -> Result<Lifecycle, EditError> {
    match l {
        Lifecycle::Superseded => Err(EditError::BadLifecycle(l)),
        _ => Ok(l),
    }
}

/// The op that makes `change` to item `id`, carrying only what actually differs.
pub fn user_edit(
    state: &MeetingState,
    id: &str,
    change: &ItemChange,
) -> Result<ResolvedOp, EditError> {
    let item: &StateItem = state
        .item(id)
        .ok_or_else(|| EditError::UnknownItem(id.to_owned()))?;
    if item.lifecycle.is_terminal() {
        return Err(EditError::Terminal(id.to_owned()));
    }
    let new_text = change.text.as_deref().map(text).transpose()?;
    let text = new_text.filter(|t| *t != item.text);
    let owner = change
        .owner
        .as_deref()
        .map(field)
        .filter(|o| item.owner.as_deref().unwrap_or("") != o);
    let due = change
        .due
        .as_deref()
        .map(field)
        .filter(|d| item.due.as_deref().unwrap_or("") != d);
    let lifecycle = change
        .lifecycle
        .map(settable)
        .transpose()?
        .filter(|l| *l != item.lifecycle);
    if text.is_none() && owner.is_none() && due.is_none() && lifecycle.is_none() {
        return Err(EditError::NoChange);
    }
    Ok(ResolvedOp::UserEdit {
        id: id.to_owned(),
        text,
        owner,
        due,
        lifecycle,
    })
}

/// Appends the user's `change` to item `id` to a meeting's `log`. Returns the new log entry and
/// the state after it; the caller stores the entry after the rest.
pub fn append_user_edit(
    meeting_id: &str,
    log: &[AppliedOp],
    id: &str,
    change: &ItemChange,
    now_ms: i64,
) -> Result<(AppliedOp, MeetingState), EditError> {
    let mut state = MeetingState::replay(meeting_id, log)?;
    let applied = AppliedOp {
        seq: state.op_count,
        at_ms: now_ms,
        op: user_edit(&state, id, change)?,
    };
    state.apply(&applied)?;
    Ok((applied, state))
}

/// An item kind or lifecycle from its stored name.
pub(crate) fn parse_name<T: for<'de> Deserialize<'de>>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

/// A new project item, checked: one of [`MANUAL_KINDS`], with text; it starts active.
pub fn new_manual_item(
    kind: &str,
    item_text: &str,
    owner: Option<&str>,
    due: Option<&str>,
) -> Result<ProjectItemInput, EditError> {
    let k: ItemKind = parse_name(kind)
        .filter(|k| MANUAL_KINDS.contains(k))
        .ok_or_else(|| EditError::BadKind(kind.to_owned()))?;
    Ok(ProjectItemInput {
        kind: k.as_str().to_owned(),
        text: text(item_text)?,
        owner: clean_field(owner),
        due: clean_field(due),
        lifecycle: Lifecycle::Active.as_str().to_owned(),
    })
}

/// A project item with `change` made. Deleting one is a real delete, not a lifecycle, so
/// withdrawn is refused here.
pub fn edit_manual_item(
    item: &ProjectItem,
    change: &ItemChange,
) -> Result<ProjectItemInput, EditError> {
    let lifecycle = match change.lifecycle {
        Some(l @ (Lifecycle::Superseded | Lifecycle::Withdrawn)) => {
            return Err(EditError::BadLifecycle(l))
        }
        Some(l) => l.as_str().to_owned(),
        None => item.lifecycle.clone(),
    };
    let optional = |new: &Option<String>, old: &Option<String>| match new {
        Some(v) => clean_field(Some(v)),
        None => old.clone(),
    };
    let out = ProjectItemInput {
        kind: item.kind.clone(),
        text: match &change.text {
            Some(t) => text(t)?,
            None => item.text.clone(),
        },
        owner: optional(&change.owner, &item.owner),
        due: optional(&change.due, &item.due),
        lifecycle,
    };
    let same = out.text == item.text
        && out.owner == item.owner
        && out.due == item.due
        && out.lifecycle == item.lifecycle;
    if same {
        return Err(EditError::NoChange);
    }
    Ok(out)
}

/// Each item's text as the meeting recorded it, before any hand edits: the log replayed without its
/// [`ResolvedOp::UserEdit`]s. Only items whose text was edited are returned. Used to match the same
/// item across meetings after the user reworded one copy.
pub fn original_texts(
    meeting_id: &str,
    log: &[AppliedOp],
) -> std::collections::HashMap<String, String> {
    let edited: std::collections::HashSet<&str> = log
        .iter()
        .filter_map(|a| match &a.op {
            ResolvedOp::UserEdit {
                id, text: Some(_), ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    if edited.is_empty() {
        return Default::default();
    }
    let recorded = log
        .iter()
        .filter(|a| !matches!(a.op, ResolvedOp::UserEdit { .. }));
    let Ok(state) = MeetingState::replay(meeting_id, recorded) else {
        return Default::default();
    };
    edited
        .into_iter()
        .filter_map(|id| state.item(id).map(|i| (id.to_owned(), i.text.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpistemicStatus;

    fn log() -> Vec<AppliedOp> {
        let add = |seq: u64, id: &str, kind: ItemKind, text: &str| AppliedOp {
            seq,
            at_ms: 10,
            op: ResolvedOp::Add {
                id: id.into(),
                kind,
                text: text.into(),
                status: EpistemicStatus::Stated,
                confidence: 0.8,
                source_refs: vec![format!("Mm:T{seq}")],
                related_items: vec![],
                owner: Some("Them".into()),
                due: None,
            },
        };
        vec![
            add(0, "COM-1", ItemKind::Commitment, "Send the numbers"),
            add(1, "Q-1", ItemKind::OpenQuestion, "Who signs off?"),
            add(2, "RISK-1", ItemKind::Risk, "Vendor lock-in"),
        ]
    }

    fn change() -> ItemChange {
        ItemChange::default()
    }

    /// Appends each change in turn, as the app does, and checks the stored log still replays.
    fn edit_all(changes: &[(&str, ItemChange)]) -> (Vec<AppliedOp>, MeetingState) {
        let mut log = log();
        let mut state = MeetingState::default();
        for (id, c) in changes {
            let (applied, s) = append_user_edit("m", &log, id, c, 99).unwrap();
            log.push(applied);
            state = s;
        }
        let json = serde_json::to_string(&log).unwrap();
        let back: Vec<AppliedOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(MeetingState::replay("m", &back).unwrap(), state);
        (log, state)
    }

    #[test]
    fn original_texts_are_the_recorded_wording_of_edited_items() {
        let (edited, _) = edit_all(&[(
            "COM-1",
            ItemChange {
                text: Some("Send the final numbers".into()),
                ..change()
            },
        )]);
        let original = original_texts("m", &edited);
        assert_eq!(original.len(), 1, "only edited items: {original:?}");
        assert_eq!(original["COM-1"], "Send the numbers");
        assert!(original_texts("m", &log()).is_empty());
    }

    #[test]
    fn edits_resolve_rewrite_and_withdraw_and_replay() {
        let (log, state) = edit_all(&[
            (
                "COM-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Resolved),
                    ..change()
                },
            ),
            (
                "Q-1",
                ItemChange {
                    text: Some("  Who signs   off on hosting? ".into()),
                    owner: Some("".into()),
                    due: Some(" Friday ".into()),
                    ..change()
                },
            ),
            (
                "RISK-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Withdrawn),
                    ..change()
                },
            ),
        ]);
        assert_eq!(state.item("COM-1").unwrap().lifecycle, Lifecycle::Resolved);
        let q = state.item("Q-1").unwrap();
        assert_eq!(q.text, "Who signs off on hosting?");
        assert_eq!(
            (q.owner.as_deref(), q.due.as_deref()),
            (None, Some("Friday"))
        );
        assert_eq!(q.source_refs, ["Mm:T1"], "evidence is kept");
        assert_eq!((q.revision, q.updated_at_ms), (2, 99));
        assert!(state.live_items().iter().all(|i| i.id != "RISK-1"));
        // The original ops are still in the log.
        assert!(matches!(&log[1].op, ResolvedOp::Add { text, .. } if text == "Who signs off?"));
        assert_eq!(log.len(), 6);
        // Only the fields that changed are recorded.
        assert_eq!(
            log[4].op,
            ResolvedOp::UserEdit {
                id: "Q-1".into(),
                text: Some("Who signs off on hosting?".into()),
                owner: Some(String::new()),
                due: Some("Friday".into()),
                lifecycle: None,
            }
        );
    }

    #[test]
    fn a_resolved_item_can_be_reopened_but_a_withdrawn_one_is_final() {
        let (log, state) = edit_all(&[
            (
                "COM-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Resolved),
                    ..change()
                },
            ),
            (
                "COM-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Active),
                    ..change()
                },
            ),
            (
                "RISK-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Withdrawn),
                    ..change()
                },
            ),
        ]);
        assert_eq!(state.item("COM-1").unwrap().lifecycle, Lifecycle::Active);
        let text = ItemChange {
            text: Some("x".into()),
            ..change()
        };
        assert_eq!(
            append_user_edit("m", &log, "RISK-1", &text, 1).unwrap_err(),
            EditError::Terminal("RISK-1".into())
        );
    }

    #[test]
    fn bad_edits_are_refused() {
        let state = MeetingState::replay("m", &log()).unwrap();
        let err = |id: &str, c: ItemChange| user_edit(&state, id, &c).unwrap_err();
        assert_eq!(
            err("NOPE-1", change()),
            EditError::UnknownItem("NOPE-1".into())
        );
        assert_eq!(err("COM-1", change()), EditError::NoChange);
        assert_eq!(
            err(
                "COM-1",
                ItemChange {
                    text: Some("Send the numbers".into()),
                    owner: Some("Them".into()),
                    ..change()
                }
            ),
            EditError::NoChange,
            "the same values change nothing"
        );
        assert_eq!(
            err(
                "COM-1",
                ItemChange {
                    text: Some("   ".into()),
                    ..change()
                }
            ),
            EditError::EmptyText
        );
        assert!(matches!(
            err(
                "COM-1",
                ItemChange {
                    text: Some("x".repeat(500)),
                    ..change()
                }
            ),
            EditError::TextTooLong(500)
        ));
        assert_eq!(
            err(
                "COM-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Superseded),
                    ..change()
                }
            ),
            EditError::BadLifecycle(Lifecycle::Superseded)
        );
    }

    #[test]
    fn manual_items_are_checked() {
        let item = new_manual_item("risk", "  Budget   may slip ", Some(" "), Some("Q3")).unwrap();
        assert_eq!(item.text, "Budget may slip");
        assert_eq!((item.owner, item.due.as_deref()), (None, Some("Q3")));
        assert_eq!(item.lifecycle, "active");
        assert_eq!(
            new_manual_item("fact", "x", None, None).unwrap_err(),
            EditError::BadKind("fact".into())
        );
        assert_eq!(
            new_manual_item("risk", " ", None, None).unwrap_err(),
            EditError::EmptyText
        );

        let stored = ProjectItem {
            id: "pi-1".into(),
            project_id: "p".into(),
            kind: "risk".into(),
            text: "Budget may slip".into(),
            owner: None,
            due: Some("Q3".into()),
            lifecycle: "active".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let done = edit_manual_item(
            &stored,
            &ItemChange {
                lifecycle: Some(Lifecycle::Resolved),
                due: Some("".into()),
                ..change()
            },
        )
        .unwrap();
        assert_eq!((done.lifecycle.as_str(), done.due), ("resolved", None));
        assert_eq!(done.text, stored.text);
        assert_eq!(
            edit_manual_item(&stored, &change()).unwrap_err(),
            EditError::NoChange
        );
        assert_eq!(
            edit_manual_item(
                &stored,
                &ItemChange {
                    lifecycle: Some(Lifecycle::Withdrawn),
                    ..change()
                }
            )
            .unwrap_err(),
            EditError::BadLifecycle(Lifecycle::Withdrawn)
        );
    }
}

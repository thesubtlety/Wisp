//! The Projects view: a project's overview, hand edits to meeting items, hand-added project items,
//! and knowledge edits. Thin dispatchers; the rules live in `wisp_intel::edit` and
//! `wisp_intel::brief`.

use std::sync::atomic::{AtomicU64, Ordering};

use tauri::{AppHandle, Manager, State};
use wisp_intel::{
    append_user_edit, edit_manual_item, new_manual_item, project_overview as overview, BriefInput,
    ItemChange, MeetingState, ProjectOverview, StateItem,
};
use wisp_library::{Library, ProjectItem};

use crate::intel::{all_items, brief_meetings, now_ms, save_log, stored_log};
use crate::AppState;

fn library<'a>(
    state: &'a State<'_, AppState>,
) -> Result<std::sync::MutexGuard<'a, Library>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())
}

/// Appends the user's change to item `item_id` of saved meeting `id` to its state log.
fn edit_saved_item(
    library: &mut Library,
    id: &str,
    item_id: &str,
    change: &ItemChange,
    now: i64,
) -> Result<MeetingState, String> {
    let mut log = stored_log(library, id)?;
    let (applied, meeting) =
        append_user_edit(id, &log, item_id, change, now).map_err(|e| e.to_string())?;
    log.push(applied);
    save_log(library, id, &log)?;
    Ok(meeting)
}

/// Changes one item of a saved meeting by hand (done, edited, deleted). Returns the meeting's
/// items afterwards, as `intel_saved_state` does.
#[tauri::command]
pub(crate) fn edit_meeting_item(
    state: State<'_, AppState>,
    id: String,
    item_id: String,
    change: ItemChange,
) -> Result<Vec<StateItem>, String> {
    let meeting = edit_saved_item(&mut *library(&state)?, &id, &item_id, &change, now_ms())?;
    Ok(all_items(meeting))
}

/// What is live in a project across its meetings and hand-added items, each meeting's gist, and
/// meeting knowledge not yet kept. Off the main thread: it replays every meeting in the project.
#[tauri::command]
pub(crate) async fn project_overview(
    app: AppHandle,
    project_id: String,
    offset_minutes: i32,
) -> Result<ProjectOverview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let library = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?;
        let manual = library
            .list_project_items(&project_id)
            .map_err(|e| e.to_string())?;
        let memory = library
            .list_memory(&project_id)
            .map_err(|e| e.to_string())?;
        let meetings = brief_meetings(&library, &project_id, offset_minutes)?;
        Ok(overview(&BriefInput {
            project: "",
            date: "",
            now_ms: now_ms(),
            instructions: "",
            memory: &memory,
            meetings: &meetings,
            manual: &manual,
        }))
    })
    .await
    .map_err(|e| format!("overview task failed: {e}"))?
}

/// A fresh project item id: the time plus a counter, so two added in one millisecond differ.
fn item_id(now: i64) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("pi-{now:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Adds an item to a project by hand.
#[tauri::command]
pub(crate) fn add_project_item(
    state: State<'_, AppState>,
    project_id: String,
    kind: String,
    text: String,
    owner: Option<String>,
    due: Option<String>,
) -> Result<ProjectItem, String> {
    let input = new_manual_item(&kind, &text, owner.as_deref(), due.as_deref())
        .map_err(|e| e.to_string())?;
    let now = now_ms();
    library(&state)?
        .add_project_item(&project_id, &item_id(now), &input, now)
        .map_err(|e| {
            if e.to_string().contains("FOREIGN KEY") {
                "that project no longer exists".to_owned()
            } else {
                e.to_string()
            }
        })
}

/// Changes a hand-added project item.
#[tauri::command]
pub(crate) fn edit_project_item(
    state: State<'_, AppState>,
    id: String,
    change: ItemChange,
) -> Result<ProjectItem, String> {
    let library = library(&state)?;
    let item = library
        .get_project_item(&id)
        .map_err(|e| e.to_string())?
        .ok_or("no such item")?;
    let input = edit_manual_item(&item, &change).map_err(|e| e.to_string())?;
    let now = now_ms();
    library
        .update_project_item(&id, &input, now)
        .map_err(|e| e.to_string())?;
    Ok(ProjectItem {
        text: input.text,
        owner: input.owner,
        due: input.due,
        lifecycle: input.lifecycle,
        updated_at_ms: now,
        ..item
    })
}

/// Deletes a hand-added project item.
#[tauri::command]
pub(crate) fn delete_project_item(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    library(&state)?
        .delete_project_item(&id)
        .map_err(|e| e.to_string())
}

/// Rewrites one piece of project knowledge.
#[tauri::command]
pub(crate) fn update_project_memory(
    state: State<'_, AppState>,
    id: i64,
    text: String,
) -> Result<bool, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("knowledge needs text".to_owned());
    }
    library(&state)?
        .update_memory_text(id, text, now_ms())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_core::export::MeetingMeta;
    use wisp_intel::{AppliedOp, EpistemicStatus, ItemKind, Lifecycle, ResolvedOp};

    #[test]
    fn a_hand_edit_is_appended_to_the_saved_log_and_replays() {
        let mut library = Library::open_in_memory().unwrap();
        library
            .save_note("m1", &MeetingMeta::default(), 0, &[])
            .unwrap();
        let add = AppliedOp {
            seq: 0,
            at_ms: 1,
            op: ResolvedOp::Add {
                id: "COM-1".into(),
                kind: ItemKind::Commitment,
                text: "Send the numbers".into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                source_refs: vec!["Mm1:T0".into()],
                related_items: vec![],
                owner: None,
                due: None,
            },
        };
        save_log(&mut library, "m1", &[add]).unwrap();

        let done = ItemChange {
            lifecycle: Some(Lifecycle::Resolved),
            owner: Some("Sarah".into()),
            ..ItemChange::default()
        };
        let state = edit_saved_item(&mut library, "m1", "COM-1", &done, 50).unwrap();
        let item = state.item("COM-1").unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Resolved);
        assert_eq!(item.owner.as_deref(), Some("Sarah"));

        let log = stored_log(&library, "m1").unwrap();
        assert_eq!(log.len(), 2, "the original op stays, the edit follows");
        assert!(matches!(log[1].op, ResolvedOp::UserEdit { .. }));
        assert_eq!(MeetingState::replay("m1", &log).unwrap(), state);

        // The same change again changes nothing, and nothing is stored.
        assert!(edit_saved_item(&mut library, "m1", "COM-1", &done, 60).is_err());
        assert_eq!(stored_log(&library, "m1").unwrap().len(), 2);
    }

    #[test]
    fn item_ids_differ_within_a_millisecond() {
        assert_ne!(item_id(5), item_id(5));
    }
}

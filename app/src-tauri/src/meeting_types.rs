//! Meeting type commands: manage the saved types (Settings › Meeting types) and a project's default
//! type. Switching the live meeting's type is `intel::intel_set_meeting_type`.

use tauri::State;
use wisp_core::meeting_types::{Cadence, CardStyle};
use wisp_library::{Library, MeetingType};

use crate::AppState;

fn lock(state: &AppState) -> Result<std::sync::MutexGuard<'_, Library>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())
}

fn fetch(library: &Library, id: &str) -> Result<MeetingType, String> {
    library
        .get_meeting_type(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no meeting type {id}"))
}

/// Checks what the editor sent: a name, and knob names the app knows.
fn checked(t: &MeetingType) -> Result<(), String> {
    if t.name.trim().is_empty() {
        return Err("a meeting type needs a name".to_owned());
    }
    if Cadence::parse(&t.cadence).is_none() {
        return Err(format!("unknown cadence: {}", t.cadence));
    }
    if CardStyle::parse(&t.card_style).is_none() {
        return Err(format!("unknown card style: {}", t.card_style));
    }
    Ok(())
}

/// Every meeting type, General and the other built-ins first.
#[tauri::command]
pub(crate) fn list_meeting_types(state: State<'_, AppState>) -> Result<Vec<MeetingType>, String> {
    lock(&state)?
        .list_meeting_types()
        .map_err(|e| e.to_string())
}

/// Saves a meeting type: a new one when `id` is empty, else replaces that one's fields.
#[tauri::command]
pub(crate) fn save_meeting_type(
    state: State<'_, AppState>,
    meeting_type: MeetingType,
) -> Result<MeetingType, String> {
    checked(&meeting_type)?;
    let now = crate::intel::now_ms();
    let library = lock(&state)?;
    if meeting_type.id.trim().is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let id = format!("type-{nanos:x}");
        library
            .create_meeting_type(
                &MeetingType {
                    id: id.clone(),
                    ..meeting_type
                },
                now,
            )
            .map_err(|e| e.to_string())?;
        return fetch(&library, &id);
    }
    if !library
        .update_meeting_type(&meeting_type, now)
        .map_err(|e| e.to_string())?
    {
        return Err(format!("no meeting type {}", meeting_type.id));
    }
    fetch(&library, &meeting_type.id)
}

/// Deletes one of the user's meeting types. A built-in can be reset, not deleted.
#[tauri::command]
pub(crate) fn delete_meeting_type(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if lock(&state)?
        .delete_meeting_type(&id)
        .map_err(|e| e.to_string())?
    {
        Ok(())
    } else {
        Err("a built-in meeting type can't be deleted; reset it instead".to_owned())
    }
}

/// Puts a built-in meeting type back to how it shipped.
#[tauri::command]
pub(crate) fn reset_meeting_type(
    state: State<'_, AppState>,
    id: String,
) -> Result<MeetingType, String> {
    let library = lock(&state)?;
    if !library
        .reset_meeting_type(&id, crate::intel::now_ms())
        .map_err(|e| e.to_string())?
    {
        return Err(format!("{id} is not a built-in meeting type"));
    }
    fetch(&library, &id)
}

/// A project's default meeting type, if it names one.
#[tauri::command]
pub(crate) fn get_project_default_type(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Option<String>, String> {
    lock(&state)?
        .project_default_type(&project_id)
        .map_err(|e| e.to_string())
}

/// Sets (or with an empty or missing id clears) a project's default meeting type.
#[tauri::command]
pub(crate) fn set_project_default_type(
    state: State<'_, AppState>,
    project_id: String,
    type_id: Option<String>,
) -> Result<(), String> {
    let type_id = type_id.filter(|t| !t.is_empty());
    let library = lock(&state)?;
    if let Some(t) = &type_id {
        fetch(&library, t)?;
    }
    if library
        .set_project_default_type(&project_id, type_id.as_deref())
        .map_err(|e| e.to_string())?
    {
        Ok(())
    } else {
        Err(format!("no project {project_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, cadence: &str, style: &str) -> MeetingType {
        MeetingType {
            id: String::new(),
            name: name.into(),
            description: String::new(),
            watch_for: String::new(),
            card_style: style.into(),
            cadence: cadence.into(),
            wrap_checklist: vec![],
            summary_sections: vec![],
            suggested_prompts: vec![],
            builtin: false,
            customized: false,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn the_editor_must_send_a_name_and_known_knobs() {
        assert!(checked(&draft("Board", "calm", "risks")).is_ok());
        assert!(checked(&draft("  ", "calm", "risks")).is_err());
        assert!(checked(&draft("Board", "slow", "risks")).is_err());
        assert!(checked(&draft("Board", "calm", "loud")).is_err());
    }
}

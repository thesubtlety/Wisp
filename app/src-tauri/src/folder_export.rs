//! Folder export: each project's meetings written as files into the folder the project picked, for
//! other apps to read. Which artifacts are written is one setting for every project, kept in
//! `folder-export.json`; where, each project's own folder, picked through the backend's folder
//! dialog so the webview never names a destination. A meeting's files are brought up to date after
//! every change to it, and a project's (or every project's) on demand. The library applies the one
//! rule that keeps the user's edits safe (see [`wisp_library::ExportTarget`]); this module decides
//! what each file should be.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use wisp_core::localtime::LocalTime;
use wisp_library::{
    export_file_name, file_stem, front_matter, ExportTarget, Note, SyncReport, EXPORT_KINDS,
};

use crate::AppState;

/// Which artifacts every project's folder gets, as the UI and the settings file see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderExportSettings {
    pub kinds: Vec<String>,
}

impl Default for FolderExportSettings {
    fn default() -> Self {
        Self {
            kinds: vec!["summary".to_owned()],
        }
    }
}

/// The last sync, for the status line in Settings and under a project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderExportStatus {
    pub last_sync_ms: Option<i64>,
    #[serde(flatten)]
    pub report: SyncReport,
    pub error: Option<String>,
}

pub(crate) struct FolderExportState {
    settings: Mutex<FolderExportSettings>,
    path: PathBuf,
    status: Mutex<FolderExportStatus>,
    /// The webview's UTC offset (minutes east), for local dates in file names; UTC until reported.
    utc_offset_minutes: AtomicI32,
}

impl FolderExportState {
    /// The saved settings, or the default (summaries only) when there are none or they can't be read.
    pub(crate) fn load(path: PathBuf) -> Self {
        let settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            settings: Mutex::new(settings),
            path,
            status: Mutex::new(FolderExportStatus::default()),
            utc_offset_minutes: AtomicI32::new(0),
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.settings
            .lock()
            .map(|s| s.kinds.clone())
            .unwrap_or_default()
    }

    fn save(&self, settings: FolderExportSettings) -> Result<(), String> {
        let json = serde_json::to_string(&settings).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json)
            .map_err(|e| format!("could not save folder export settings: {e}"))?;
        if let Ok(mut slot) = self.settings.lock() {
            *slot = settings;
        }
        Ok(())
    }

    fn record(&self, result: &Result<SyncReport, String>) -> FolderExportStatus {
        let status = FolderExportStatus {
            last_sync_ms: Some(crate::intel::now_ms()),
            report: result.clone().unwrap_or_default(),
            error: result.as_ref().err().cloned(),
        };
        if let Ok(mut slot) = self.status.lock() {
            slot.clone_from(&status);
        }
        status
    }
}

/// The settings and the last sync, for Settings › Storage.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderExportDto {
    kinds: Vec<String>,
    status: FolderExportStatus,
}

fn dto(state: &AppState) -> FolderExportDto {
    FolderExportDto {
        kinds: state.folder_export.kinds(),
        status: state
            .folder_export
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default(),
    }
}

#[tauri::command]
pub(crate) fn get_folder_export(state: State<'_, AppState>) -> FolderExportDto {
    dto(&state)
}

/// Sets which artifacts are written, then brings every project's folder in line: files of a kind
/// turned off are removed (while unchanged), ones turned on are written.
#[tauri::command]
pub(crate) async fn set_folder_export_kinds(
    app: AppHandle,
    kinds: Vec<String>,
) -> Result<FolderExportDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let kinds: Vec<String> = EXPORT_KINDS
            .iter()
            .filter(|k| kinds.iter().any(|w| w == *k))
            .map(|k| (*k).to_owned())
            .collect();
        state.folder_export.save(FolderExportSettings { kinds })?;
        let result = sync_all(&state);
        state.folder_export.record(&result);
        Ok(dto(&state))
    })
    .await
    .map_err(|e| format!("folder export task failed: {e}"))?
}

/// The webview's UTC offset in minutes east of UTC, so file names and dates read as the user's
/// calendar. Sent once the frontend is ready.
#[tauri::command]
pub(crate) fn set_utc_offset(state: State<'_, AppState>, offset_minutes: i32) {
    state
        .folder_export
        .utc_offset_minutes
        .store(offset_minutes, Ordering::Relaxed);
}

/// Lets the user pick a project's export folder in a folder dialog the backend shows, stores it,
/// and writes the project's meetings there. Returns that sync, or `None` when the user cancels.
/// Off the main thread: the dialog blocks until the user answers.
#[tauri::command]
pub(crate) async fn pick_project_export_dir(
    app: AppHandle,
    project_id: String,
) -> Result<Option<FolderExportStatus>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let Some(picked) = app.dialog().file().blocking_pick_folder() else {
            return Ok(None);
        };
        let dir = picked.into_path().map_err(|e| e.to_string())?;
        if !dir.is_dir() {
            return Err(format!("not a folder: {}", dir.display()));
        }
        let text = dir.display().to_string();
        let state = app.state::<AppState>();
        let exists = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .set_project_export_dir(&project_id, Some(&text))
            .map_err(|e| e.to_string())?;
        if !exists {
            return Err("no such project".to_owned());
        }
        let result = sync_project(&state, &project_id);
        let status = state.folder_export.record(&result);
        result.map(|_| Some(status))
    })
    .await
    .map_err(|e| format!("folder dialog failed: {e}"))?
}

/// Forgets a project's export folder and removes the files Wisp wrote there (while unchanged).
/// Returns whether the project exists.
#[tauri::command]
pub(crate) async fn clear_project_export_dir(
    app: AppHandle,
    project_id: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let exists = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .set_project_export_dir(&project_id, None)
            .map_err(|e| e.to_string())?;
        if exists {
            let result = sync_project(&state, &project_id);
            state.folder_export.record(&result);
            result?;
        }
        Ok(exists)
    })
    .await
    .map_err(|e| format!("folder export task failed: {e}"))?
}

/// Writes (or brings up to date) every meeting of one project.
#[tauri::command]
pub(crate) async fn sync_project_export(
    app: AppHandle,
    project_id: String,
) -> Result<FolderExportStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let result = sync_project(&state, &project_id);
        let status = state.folder_export.record(&result);
        result.map(|_| status)
    })
    .await
    .map_err(|e| format!("folder export task failed: {e}"))?
}

/// Writes (or brings up to date) every meeting of every project with a folder.
#[tauri::command]
pub(crate) async fn sync_all_exports(app: AppHandle) -> Result<FolderExportStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let result = sync_all(&state);
        let status = state.folder_export.record(&result);
        result.map(|_| status)
    })
    .await
    .map_err(|e| format!("folder export task failed: {e}"))?
}

/// A saved meeting changed (saved, summarized, renamed, moved, a speaker named, an item edited):
/// bring its files in line. Never fails the change itself; a problem is logged and shown as the
/// last sync's error.
pub(crate) fn after_change(state: &AppState, meeting_id: &str) {
    let result = sync_meeting(state, meeting_id);
    if let Err(e) = &result {
        eprintln!("wisp: folder export of meeting {meeting_id} failed: {e}");
    }
    state.folder_export.record(&result);
}

/// A saved meeting is about to be deleted: remove the files Wisp wrote for it (while unchanged).
/// Called before the deletion, which takes its records with it.
pub(crate) fn before_delete(state: &AppState, meeting_id: &str) {
    let now = crate::intel::now_ms();
    let Ok(library) = state.library.lock() else {
        return;
    };
    for kind in EXPORT_KINDS {
        if let Err(e) = library.sync_export_file(meeting_id, kind, ExportTarget::Retire, now) {
            eprintln!("wisp: removing the {kind} export of meeting {meeting_id} failed: {e}");
        }
    }
}

fn sync_all(state: &AppState) -> Result<SyncReport, String> {
    let projects: Vec<String> = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .list_projects()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| p.export_dir.is_some())
        .map(|p| p.id)
        .collect();
    let mut report = SyncReport::default();
    for project in projects {
        report.extend(sync_project(state, &project)?);
    }
    Ok(report)
}

fn sync_project(state: &AppState, project_id: &str) -> Result<SyncReport, String> {
    let ids: Vec<String> = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .list_notes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|n| n.project_id.as_deref() == Some(project_id))
        .map(|n| n.id)
        .collect();
    let mut report = SyncReport::default();
    for id in ids {
        report.extend(sync_meeting(state, &id)?);
    }
    Ok(report)
}

/// Brings one meeting's files in line: every kind that is on and has something to write gets a
/// file in the project's folder; a kind that is off, or a meeting without a folder, has its file
/// retired. A folder that no longer exists is an error rather than a reason to recreate it.
fn sync_meeting(state: &AppState, meeting_id: &str) -> Result<SyncReport, String> {
    let (note, dir, has_state, project) = {
        let library = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?;
        let (note, _) = library
            .get_note(meeting_id)
            .map_err(|e| e.to_string())?
            .ok_or("no such meeting")?;
        let dir = note
            .project_id
            .as_deref()
            .and_then(|p| library.project_export_dir(p).ok().flatten());
        let has_state = !crate::intel::stored_log(&library, meeting_id)?.is_empty();
        let project = crate::intel::project_name(&library, note.project_id.as_deref());
        (note, dir, has_state, project)
    };
    let dir = match dir {
        Some(d) => {
            let d = PathBuf::from(d);
            if !d.is_dir() {
                return Err(format!("export folder not found: {}", d.display()));
            }
            Some(d)
        }
        None => None,
    };
    let kinds = state.folder_export.kinds();
    let offset = state
        .folder_export
        .utc_offset_minutes
        .load(Ordering::Relaxed);
    let started = LocalTime::from_epoch_ms(note.started_at_ms, offset);
    let stem = file_stem(&started, &note.title);
    let when = started.display();
    let now = crate::intel::now_ms();
    let mut report = SyncReport::default();
    for kind in EXPORT_KINDS {
        let wanted: Option<&Path> = dir.as_deref().filter(|_| kinds.iter().any(|k| k == kind));
        let path = wanted.map(|d| d.join(export_file_name(&stem, kind).expect("a known kind")));
        let content =
            wanted.and_then(|_| render(state, &note, kind, &when, has_state, project.as_deref()));
        let target = match (&path, &content) {
            (Some(p), Some(c)) => ExportTarget::Write(p, c),
            (Some(_), None) => ExportTarget::Keep,
            (None, _) => ExportTarget::Retire,
        };
        let library = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?;
        report.extend(
            library
                .sync_export_file(meeting_id, kind, target, now)
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(report)
}

/// The document for `kind`, with front matter on the Markdown ones, or `None` when there is nothing
/// to write yet: no summary, no transcript (expired or never any), or no intelligence state behind
/// the record, packet and JSON.
fn render(
    state: &AppState,
    note: &Note,
    kind: &str,
    when: &str,
    has_state: bool,
    project: Option<&str>,
) -> Option<String> {
    let empty = |s: Option<&str>| s.map_or(true, |s| s.trim().is_empty());
    let nothing = match kind {
        "summary" => empty(note.summary.as_deref()),
        "transcript" => note.segment_count == 0 || note.transcript_pruned_at_ms.is_some(),
        _ => !has_state,
    };
    if nothing {
        return None;
    }
    let body =
        crate::intel::render_export(state, Some(&note.id), kind, None, Some(when.to_owned()))
            .ok()?;
    Some(if kind == "json" {
        body
    } else {
        format!(
            "{}{body}",
            front_matter(&note.title, when, project, &note.id)
        )
    })
}

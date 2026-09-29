//! Retention settings and destructive operations, as the Settings view drives them.
//!
//! The policy (how long transcripts and temporary sources are kept) persists to a small JSON file
//! and is applied before the startup prune, so expiry works even after months with the app closed.
//! Changing it applies to existing data too ([`Library::restamp_expiries`]); the UI previews what a
//! shorter policy would delete before the user confirms.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::State;
use wisp_library::{Library, PruneReport, RetentionPolicy};

use crate::AppState;

/// The policy as the UI and the settings file see it. `null` keeps forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RetentionDto {
    pub transcript_days: Option<u32>,
    pub temp_source_days: Option<u32>,
}

impl From<RetentionPolicy> for RetentionDto {
    fn from(p: RetentionPolicy) -> Self {
        Self {
            transcript_days: p.transcript_days,
            temp_source_days: p.temp_source_days,
        }
    }
}

impl From<RetentionDto> for RetentionPolicy {
    fn from(d: RetentionDto) -> Self {
        Self {
            transcript_days: d.transcript_days,
            temp_source_days: d.temp_source_days,
        }
    }
}

/// The saved policy, or the default when there is none (or it can't be read).
pub(crate) fn load_policy(path: &Path) -> RetentionPolicy {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<RetentionDto>(&s).ok())
        .map(RetentionPolicy::from)
        .unwrap_or_default()
}

fn save_policy(path: &Path, policy: RetentionPolicy) -> Result<(), String> {
    let json = serde_json::to_string(&RetentionDto::from(policy)).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("could not save retention settings: {e}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// What a prune or deletion removed, for the UI.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PruneDto {
    transcripts: usize,
    sources: usize,
    meetings: usize,
    files: usize,
    files_refused: usize,
}

impl From<PruneReport> for PruneDto {
    fn from(r: PruneReport) -> Self {
        Self {
            transcripts: r.transcripts_expired,
            sources: r.sources_expired,
            meetings: r.meetings_deleted,
            files: r.files_deleted,
            files_refused: r.files_refused.len(),
        }
    }
}

/// What would go right away under a policy.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewDto {
    transcripts: usize,
    sources: usize,
}

fn library<'a>(state: &'a AppState) -> Result<std::sync::MutexGuard<'a, Library>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())
}

/// The policy in effect.
#[tauri::command]
pub(crate) fn get_retention(state: State<'_, AppState>) -> Result<RetentionDto, String> {
    Ok(library(&state)?.retention().into())
}

/// How many transcripts and temporary sources `policy` would delete if adopted now.
#[tauri::command]
pub(crate) fn preview_retention(
    state: State<'_, AppState>,
    policy: RetentionDto,
) -> Result<PreviewDto, String> {
    let (transcripts, sources) = library(&state)?
        .preview_policy(policy.into(), now_ms())
        .map_err(|e| e.to_string())?;
    Ok(PreviewDto {
        transcripts,
        sources,
    })
}

/// Adopts `policy` for new and existing data, saves it, and prunes what is now due.
#[tauri::command]
pub(crate) fn set_retention(
    state: State<'_, AppState>,
    policy: RetentionDto,
) -> Result<PruneDto, String> {
    let mut lib = library(&state)?;
    apply_policy(
        &mut lib,
        policy.into(),
        &state.retention_path,
        &state.managed_dir,
        now_ms(),
    )
}

fn apply_policy(
    lib: &mut Library,
    policy: RetentionPolicy,
    path: &Path,
    managed_dir: &Path,
    now: i64,
) -> Result<PruneDto, String> {
    lib.restamp_expiries(policy).map_err(|e| e.to_string())?;
    save_policy(path, policy)?;
    Ok(lib
        .prune(now, managed_dir)
        .map_err(|e| e.to_string())?
        .into())
}

/// Runs retention now.
#[tauri::command]
pub(crate) fn prune_now(state: State<'_, AppState>) -> Result<PruneDto, String> {
    Ok(library(&state)?
        .prune(now_ms(), &state.managed_dir)
        .map_err(|e| e.to_string())?
        .into())
}

/// Deletes a project completely: its meetings (transcripts, summaries, state, logs), sources and
/// their app-managed copies, and its accepted knowledge.
#[tauri::command]
pub(crate) fn delete_project_completely(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<PruneDto, String> {
    Ok(library(&state)?
        .delete_project(&project_id, &state.managed_dir)
        .map_err(|e| e.to_string())?
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_round_trips_through_its_file_and_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention.json");
        assert_eq!(load_policy(&path), RetentionPolicy::default());
        let week = RetentionPolicy {
            transcript_days: Some(7),
            temp_source_days: None,
        };
        save_policy(&path, week).unwrap();
        assert_eq!(load_policy(&path), week);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_policy(&path), RetentionPolicy::default());
    }

    #[test]
    fn applying_a_policy_restamps_saves_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention.json");
        let mut lib = Library::open_in_memory().unwrap();
        let seg = wisp_core::transcript::TranscriptSegment {
            id: 0,
            text: "hello".into(),
            start: std::time::Duration::ZERO,
            end: std::time::Duration::from_millis(500),
            status: wisp_core::transcript::SegmentStatus::Final,
            source: wisp_core::transcript::AudioSourceKind::System,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        };
        let meta = wisp_core::export::MeetingMeta {
            title: Some("t".into()),
            date: None,
            engine: None,
            language: None,
            summary: None,
        };
        lib.save_note("m", &meta, 0, &[seg]).unwrap();
        let day = 24 * 60 * 60 * 1000;
        let week = RetentionPolicy {
            transcript_days: Some(7),
            temp_source_days: Some(7),
        };
        let report = apply_policy(&mut lib, week, &path, dir.path(), 8 * day).unwrap();
        assert_eq!(report.transcripts, 1);
        assert_eq!(load_policy(&path), week);
        assert_eq!(lib.retention(), week);
    }
}

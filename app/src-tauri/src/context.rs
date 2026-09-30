//! Manual screenshot context: "Capture Context" during a live meeting, or an image pasted or
//! imported. No continuous capture, ever: each screenshot is one explicit user action.
//!
//! A screenshot becomes a temporary source in the meeting's project (so retention, project
//! deletion and retrieval treat it like any other copy the app owns). A vision-capable backend
//! describes it once; the description is what gets indexed, pinned into later passes and
//! exported. The image itself goes to a model only for that description and, later, with an Ask
//! question whose retrieved evidence includes it.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use wisp_intel::screenshot::PLACEHOLDER;
use wisp_intel::{describe_screenshot, ScreenshotInput};
use wisp_library::{source_ref, Snippet, SnippetOrigin, SourceInput, SourceKind};
use wisp_reasoning::{CancelToken, MAX_IMAGE_BYTES};

use crate::AppState;

/// Emitted with the meeting's full screenshot list whenever it changes.
pub(crate) const CONTEXT_EVENT: &str = "intel://context";
/// The global shortcut for Capture Context, registered only while a meeting with intelligence runs.
pub(crate) const CAPTURE_SHORTCUT: &str = "CommandOrControl+Alt+Shift+C";
/// Largest image accepted before any downscaling.
const MAX_INPUT_BYTES: usize = 25 * 1024 * 1024;
/// Longest side after downscaling an image too large to send.
const DOWNSCALE_PX: &str = "2048";
const DESCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Where a screenshot's description stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ShotStatus {
    Describing,
    Described,
    Undescribed,
}

/// One screenshot attached to the current meeting.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextShot {
    pub(crate) source_id: i64,
    pub(crate) label: String,
    /// What the description calls it, once described.
    pub(crate) title: Option<String>,
    pub(crate) status: ShotStatus,
    /// Why it wasn't described.
    pub(crate) note: Option<String>,
    #[serde(skip)]
    pub(crate) path: PathBuf,
}

impl ContextShot {
    pub(crate) fn ref_id(&self) -> String {
        source_ref(self.source_id, 0)
    }
}

/// The meeting screenshots are filed under.
#[derive(Debug, Clone)]
struct Meeting {
    project_id: Option<String>,
    label: String,
    started_ms: i64,
}

/// The current meeting's screenshots. Kept after Stop, so they can still be seen and removed;
/// cleared when the next meeting starts.
#[derive(Default)]
pub(crate) struct ContextState {
    shots: Mutex<Vec<ContextShot>>,
    meeting: Mutex<Option<Meeting>>,
}

impl ContextState {
    /// Screenshots Ask may show to the model, by source (`S7`), newest first.
    pub(crate) fn images(&self) -> Vec<(String, PathBuf)> {
        self.shots
            .lock()
            .map(|s| {
                s.iter()
                    .rev()
                    .map(|x| (format!("S{}", x.source_id), x.path.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// A new meeting: forgets the last one's screenshots, files new ones under `project_id`, and
/// registers the capture shortcut.
pub(crate) fn begin(app: &AppHandle, project_id: Option<String>, label: Option<String>) {
    let state = app.state::<AppState>();
    let context = &state.intel.context;
    if let Ok(mut shots) = context.shots.lock() {
        shots.clear();
    }
    if let Ok(mut meeting) = context.meeting.lock() {
        *meeting = Some(Meeting {
            project_id,
            label: label
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| "a meeting".to_owned()),
            started_ms: now_ms(),
        });
    }
    let _ = app.emit(CONTEXT_EVENT, Vec::<ContextShot>::new());
    set_shortcut(app, true);
}

/// The live meeting moved to another project: screenshots from now on are filed there. Ones
/// already attached stay with the project they were filed under.
pub(crate) fn set_project(app: &AppHandle, project_id: Option<String>) {
    let state = app.state::<AppState>();
    if let Ok(mut meeting) = state.intel.context.meeting.lock() {
        if let Some(meeting) = meeting.as_mut() {
            meeting.project_id = project_id;
        }
    };
}

/// The meeting stopped: the capture shortcut goes; pasting and importing still work.
pub(crate) fn end(app: &AppHandle) {
    set_shortcut(app, false);
}

fn set_shortcut(app: &AppHandle, on: bool) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};
    let Ok(shortcut) = CAPTURE_SHORTCUT.parse::<Shortcut>() else {
        return;
    };
    let shortcuts = app.global_shortcut();
    let result = if on {
        if shortcuts.is_registered(shortcut) {
            Ok(())
        } else {
            shortcuts.register(shortcut)
        }
    } else if shortcuts.is_registered(shortcut) {
        shortcuts.unregister(shortcut)
    } else {
        Ok(())
    };
    if let Err(e) = result {
        eprintln!("wisp: capture shortcut: {e}");
    }
}

/// The capture shortcut was pressed.
pub(crate) fn on_shortcut(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = capture_blocking(&app) {
            eprintln!("wisp: capture context: {e}");
        }
    });
}

/// The image type from its first bytes; the extension or MIME type the caller claims is ignored.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn shots_dir(state: &AppState) -> Result<PathBuf, String> {
    let dir = state.managed_dir.join("screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    crate::restrict_to_owner(&dir, 0o700);
    Ok(dir)
}

/// On macOS, shrinks an image too large to send to at most [`DOWNSCALE_PX`] on its longest side,
/// as JPEG, with the system's `sips`. Returns the path to keep. Elsewhere it's left as is, and a
/// backend refuses it with a clear error.
fn fit(path: PathBuf) -> PathBuf {
    let small = std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_IMAGE_BYTES);
    if small || !cfg!(target_os = "macos") {
        return path;
    }
    let out = path.with_extension("fit.jpg");
    let ok = std::process::Command::new("/usr/bin/sips")
        .args(["-Z", DOWNSCALE_PX, "-s", "format", "jpeg"])
        .arg(&path)
        .arg("--out")
        .arg(&out)
        .output()
        .is_ok_and(|o| o.status.success() && out.exists());
    if ok {
        let _ = std::fs::remove_file(&path);
        out
    } else {
        let _ = std::fs::remove_file(&out);
        path
    }
}

fn emit_list(app: &AppHandle) {
    let state = app.state::<AppState>();
    let shots = state
        .intel
        .context
        .shots
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let _ = app.emit(CONTEXT_EVENT, shots);
}

/// Stores an image as a screenshot source of the meeting's project and starts describing it.
fn attach(app: &AppHandle, bytes: &[u8]) -> Result<ContextShot, String> {
    let state = app.state::<AppState>();
    let meeting = state
        .intel
        .context
        .meeting
        .lock()
        .ok()
        .and_then(|m| m.clone())
        .ok_or("start a meeting with Meeting intelligence on to attach screenshots")?;
    let project_id = meeting
        .project_id
        .clone()
        .ok_or("pick a project first: screenshots are kept with a project's sources")?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err("that image is too large (over 25 MB)".to_owned());
    }
    let ext = sniff(bytes).ok_or("not a PNG, JPEG, GIF or WebP image")?;
    let now = now_ms();
    let path = shots_dir(&state)?.join(format!("{now:x}-{:x}.{ext}", std::process::id()));
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    crate::restrict_to_owner(&path, 0o600);
    let path = fit(path);
    let label = format!(
        "Screenshot at {} in {}",
        clock(now - meeting.started_ms),
        meeting.label
    );
    let added = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .add_source(
            &project_id,
            &SourceInput {
                kind: SourceKind::Screenshot,
                label: label.clone(),
                text: PLACEHOLDER.to_owned(),
                origin_path: None,
                managed_path: Some(path.clone()),
                added_at_ms: now,
            },
        );
    let source_id = match added {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e.to_string());
        }
    };
    let shot = ContextShot {
        source_id,
        label,
        title: None,
        status: ShotStatus::Describing,
        note: None,
        path,
    };
    if let Ok(mut shots) = state.intel.context.shots.lock() {
        shots.push(shot.clone());
    }
    emit_list(app);
    let worker = app.clone();
    std::thread::spawn(move || describe(&worker, source_id));
    Ok(shot)
}

/// Describes a screenshot with the first backend that can see images, stores the description as
/// its source text and pins it into the live meeting's passes. Failure leaves it undescribed,
/// with the reason.
fn describe(app: &AppHandle, source_id: i64) {
    let state = app.state::<AppState>();
    let Some(shot) = state
        .intel
        .context
        .shots
        .lock()
        .ok()
        .and_then(|s| s.iter().find(|x| x.source_id == source_id).cloned())
    else {
        return;
    };
    let recent = crate::intel::recent_lines(&state);
    let backend = crate::reasoning::backend(&state);
    let result = if backend.capabilities().vision {
        describe_screenshot(
            backend.as_ref(),
            &CancelToken::new(),
            &ScreenshotInput {
                image: &shot.path,
                recent: &recent,
                timeout: DESCRIBE_TIMEOUT,
            },
        )
        .map_err(|e| e.to_string())
    } else {
        Err("no reasoning backend here can see images".to_owned())
    };
    let outcome = result.and_then(|d| {
        let text = d.source_text();
        let stored = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .replace_source_text(source_id, &text)
            .map_err(|e| e.to_string())?;
        if !stored {
            return Err("the screenshot was removed".to_owned());
        }
        let _ = crate::intel::with_runtime(&state, |rt| {
            rt.pin(Snippet {
                ref_id: shot.ref_id(),
                origin: SnippetOrigin::Source {
                    source_id,
                    chunk_idx: 0,
                    label: shot.label.clone(),
                    line_start: Some(1),
                },
                text: text.clone(),
                score: 0.0,
            })
        });
        Ok(d.title)
    });
    if let Ok(mut shots) = state.intel.context.shots.lock() {
        if let Some(s) = shots.iter_mut().find(|x| x.source_id == source_id) {
            match outcome {
                Ok(title) => {
                    s.title = Some(title);
                    s.status = ShotStatus::Described;
                    s.note = None;
                }
                Err(e) => {
                    eprintln!("wisp: screenshot not described: {e}");
                    s.status = ShotStatus::Undescribed;
                    s.note = Some(e);
                }
            }
        }
    }
    emit_list(app);
}

/// Runs the system's interactive screenshot (macOS `screencapture -i`: drag a region, or Space
/// for a window; Esc cancels). `None` when cancelled.
fn capture_blocking(app: &AppHandle) -> Result<Option<ContextShot>, String> {
    if !cfg!(target_os = "macos") {
        return Err(
            "Capture Context needs macOS here; paste or import an image instead".to_owned(),
        );
    }
    let state = app.state::<AppState>();
    let tmp = shots_dir(&state)?.join(format!("capture-{:x}.png", now_ms()));
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-i", "-x", "-t", "png"])
        .arg(&tmp)
        .status()
        .map_err(|e| format!("screencapture: {e}"))?;
    if !tmp.exists() {
        // Esc, or the capture failed without writing anything.
        return if status.success() {
            Ok(None)
        } else {
            Err(format!("screencapture exited with {status}"))
        };
    }
    let bytes = std::fs::read(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let bytes = bytes.map_err(|e| e.to_string())?;
    attach(app, &bytes).map(Some)
}

/// Capture Context: an interactive screenshot of a region or window. `None` when cancelled.
#[tauri::command]
pub(crate) async fn capture_context(app: AppHandle) -> Result<Option<ContextShot>, String> {
    tauri::async_runtime::spawn_blocking(move || capture_blocking(&app))
        .await
        .map_err(|e| format!("capture task failed: {e}"))?
}

/// An image pasted into the app, sent as the raw request body.
#[tauri::command]
pub(crate) async fn paste_context_image(
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<ContextShot, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected the image bytes".to_owned());
    };
    let bytes = bytes.clone();
    tauri::async_runtime::spawn_blocking(move || attach(&app, &bytes))
        .await
        .map_err(|e| format!("paste task failed: {e}"))?
}

/// An image file the user picks in an open dialog. `None` when cancelled.
#[tauri::command]
pub(crate) async fn import_context_image(app: AppHandle) -> Result<Option<ContextShot>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let Some(picked) = app
            .dialog()
            .file()
            .add_filter("Image", &["png", "jpg", "jpeg", "gif", "webp"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let path = picked.into_path().map_err(|e| e.to_string())?;
        let len = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if len > MAX_INPUT_BYTES as u64 {
            return Err("that image is too large (over 25 MB)".to_owned());
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        attach(&app, &bytes).map(Some)
    })
    .await
    .map_err(|e| format!("import task failed: {e}"))?
}

/// The current meeting's screenshots.
#[tauri::command]
pub(crate) fn list_context(state: State<'_, AppState>) -> Vec<ContextShot> {
    state
        .intel
        .context
        .shots
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

/// Tries the description again.
#[tauri::command]
pub(crate) fn describe_context(app: AppHandle, source_id: i64) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut shots = state
            .intel
            .context
            .shots
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?;
        let shot = shots
            .iter_mut()
            .find(|x| x.source_id == source_id)
            .ok_or("no such screenshot")?;
        shot.status = ShotStatus::Describing;
        shot.note = None;
    }
    emit_list(&app);
    let worker = app.clone();
    std::thread::spawn(move || describe(&worker, source_id));
    Ok(())
}

/// A screenshot kept with a project, for the Library's project view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectShot {
    pub(crate) source_id: i64,
    pub(crate) label: String,
    /// What the description calls it, once described.
    pub(crate) title: Option<String>,
    /// The start of the description, when there is one.
    pub(crate) snippet: Option<String>,
    pub(crate) added_at_ms: i64,
    pub(crate) expires_at_ms: Option<i64>,
}

/// The title and the rest of a stored description (see `ScreenshotDescription::source_text`).
/// `None` for an undescribed screenshot.
fn split_description(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if text.is_empty() || text == PLACEHOLDER {
        return None;
    }
    let (title, rest) = text.split_once('\n').unwrap_or((text, ""));
    Some((title.trim().to_owned(), rest.trim().to_owned()))
}

/// A project's screenshots, newest first.
#[tauri::command]
pub(crate) fn list_project_screenshots(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Vec<ProjectShot>, String> {
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let sources = library
        .list_sources(&project_id)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for source in sources.into_iter().filter(|s| s.kind == SCREENSHOT_KIND) {
        let text = library
            .snippet_for_ref(&source_ref(source.id, 0))
            .map_err(|e| e.to_string())?
            .map(|s| s.text)
            .unwrap_or_default();
        let (title, snippet) = match split_description(&text) {
            Some((title, rest)) => (Some(title), Some(rest).filter(|r| !r.is_empty())),
            None => (None, None),
        };
        out.push(ProjectShot {
            source_id: source.id,
            label: source.label,
            title,
            snippet,
            added_at_ms: source.added_at_ms,
            expires_at_ms: source.expires_at_ms,
        });
    }
    Ok(out)
}

/// The `kind` a screenshot source is stored with.
const SCREENSHOT_KIND: &str = "screenshot";

/// The screenshot file of `source`, only if it is a screenshot whose file resolves inside
/// `managed_root`.
fn screenshot_file(
    source: &wisp_library::Source,
    managed_root: &std::path::Path,
) -> Result<PathBuf, String> {
    if source.kind != SCREENSHOT_KIND {
        return Err("not a screenshot".to_owned());
    }
    let path = source
        .managed_path
        .as_deref()
        .ok_or("the screenshot has no image")?;
    let root = managed_root
        .canonicalize()
        .map_err(|e| format!("managed folder: {e}"))?;
    let path = std::path::Path::new(path)
        .canonicalize()
        .map_err(|_| "the screenshot image is gone".to_owned())?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err("the screenshot image is not in the app's folder".to_owned());
    }
    Ok(path)
}

/// A screenshot's image bytes, for a thumbnail or the full-size view.
#[tauri::command]
pub(crate) async fn context_image(
    app: AppHandle,
    source_id: i64,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let source = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .get_source(source_id)
            .map_err(|e| e.to_string())?
            .ok_or("no such screenshot")?;
        let path = screenshot_file(&source, &state.managed_dir)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("read screenshot: {e}"))?;
        Ok(tauri::ipc::Response::new(bytes))
    })
    .await
    .map_err(|e| format!("image task failed: {e}"))?
}

/// Deletes a screenshot now: its source, description and image file, and its pin. Works for any
/// screenshot the library still has, not only the current meeting's.
#[tauri::command]
pub(crate) fn remove_context(app: AppHandle, source_id: i64) -> Result<(), String> {
    let state = app.state::<AppState>();
    remove(&state, source_id)?;
    let _ = crate::intel::with_runtime(&state, |rt| rt.unpin(source_ref(source_id, 0)));
    emit_list(&app);
    Ok(())
}

fn remove(state: &AppState, source_id: i64) -> Result<(), String> {
    let is_shot = {
        let mut library = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?;
        let is_shot = library
            .get_source(source_id)
            .map_err(|e| e.to_string())?
            .is_some_and(|s| s.kind == SCREENSHOT_KIND);
        if is_shot {
            library
                .remove_source(source_id, &state.managed_dir)
                .map_err(|e| e.to_string())?;
        }
        is_shot
    };
    let mut shots = state
        .intel
        .context
        .shots
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    let known = shots.iter().any(|x| x.source_id == source_id);
    shots.retain(|x| x.source_id != source_id);
    if is_shot || known {
        Ok(())
    } else {
        Err("no such screenshot".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn images_are_recognised_by_their_bytes_not_their_names() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a.."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff(b"<svg xmlns="), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn labels_say_when_in_the_meeting() {
        assert_eq!(clock(65_000), "01:05");
        assert_eq!(clock(3_725_000), "1:02:05");
    }

    fn shot(kind: &str, managed_path: Option<&Path>) -> wisp_library::Source {
        wisp_library::Source {
            id: 1,
            project_id: "p".into(),
            kind: kind.into(),
            label: "Screenshot".into(),
            origin_path: None,
            managed_path: managed_path.map(|p| p.display().to_string()),
            sha256: String::new(),
            added_at_ms: 0,
            expires_at_ms: None,
            chunk_count: 1,
        }
    }

    #[test]
    fn only_screenshot_files_inside_the_managed_folder_are_served() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside = root.path().join("screenshots");
        std::fs::create_dir_all(&inside).unwrap();
        let good = inside.join("a.png");
        std::fs::write(&good, b"x").unwrap();
        let elsewhere = outside.path().join("b.png");
        std::fs::write(&elsewhere, b"x").unwrap();

        let served = screenshot_file(&shot("screenshot", Some(&good)), root.path()).unwrap();
        assert_eq!(served, good.canonicalize().unwrap());
        assert!(screenshot_file(&shot("pasted", Some(&good)), root.path()).is_err());
        assert!(screenshot_file(&shot("screenshot", None), root.path()).is_err());
        assert!(screenshot_file(&shot("screenshot", Some(&elsewhere)), root.path()).is_err());
        let sneaky = inside
            .join("..")
            .join("..")
            .join(outside.path().file_name().unwrap());
        assert!(screenshot_file(
            &shot("screenshot", Some(&sneaky.join("b.png"))),
            root.path()
        )
        .is_err());
        assert!(screenshot_file(
            &shot("screenshot", Some(&inside.join("gone.png"))),
            root.path()
        )
        .is_err());
    }

    #[test]
    fn a_stored_description_splits_into_title_and_detail() {
        assert_eq!(split_description(PLACEHOLDER), None);
        assert_eq!(split_description("  "), None);
        assert_eq!(
            split_description("Pricing table\n\nThree tiers."),
            Some(("Pricing table".to_owned(), "Three tiers.".to_owned()))
        );
        assert_eq!(
            split_description("Just a title"),
            Some(("Just a title".to_owned(), String::new()))
        );
    }

    #[test]
    fn small_images_are_kept_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(fit(p.clone()), p);
        assert!(Path::new(&p).exists());
    }
}

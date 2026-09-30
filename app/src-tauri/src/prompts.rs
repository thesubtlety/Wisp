//! The prompt library's commands: manage saved prompts, run one over a meeting on the reasoning
//! backend (Settings › Reasoning), and keep each run's output with its meeting.
//!
//! A saved meeting's transcript is read from the library with the speaker names the user gave, and
//! the run is stored under that meeting (deleted with it, pruned with its transcript). The live
//! meeting has no library row yet, so the webview hands in its transcript and the run is not stored;
//! its model call is still logged in AI activity under the live meeting's id. Long transcripts are
//! map-reduced exactly as the assist does it.

use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager, State};
use wisp_core::prompts::{fill_prompt, utc_date, PromptScope, PromptVars};
use wisp_library::{Library, Prompt, PromptRun};
use wisp_reasoning::{
    CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError, ReasoningRequest,
    ReasoningResponse, TaskKind,
};

use crate::AppState;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn lock(state: &AppState) -> Result<std::sync::MutexGuard<'_, Library>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())
}

/// A prompt's fields, trimmed and checked: a name, a body, and a known scope.
fn checked<'a>(
    name: &'a str,
    body: &'a str,
    scope: &str,
) -> Result<(&'a str, &'a str, PromptScope), String> {
    let name = name.trim();
    let body = body.trim();
    if name.is_empty() {
        return Err("a prompt needs a name".to_owned());
    }
    if body.is_empty() {
        return Err("a prompt needs some text".to_owned());
    }
    let scope = PromptScope::parse(scope).ok_or_else(|| format!("unknown scope: {scope}"))?;
    Ok((name, body, scope))
}

fn fetch(library: &Library, id: &str) -> Result<Prompt, String> {
    library
        .get_prompt(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no prompt {id}"))
}

/// Every saved prompt, built-ins first.
#[tauri::command]
pub(crate) fn list_prompts(state: State<'_, AppState>) -> Result<Vec<Prompt>, String> {
    lock(&state)?.list_prompts().map_err(|e| e.to_string())
}

/// Adds a prompt. `scope` is `meeting` or `speaker`.
#[tauri::command]
pub(crate) fn create_prompt(
    state: State<'_, AppState>,
    name: String,
    body: String,
    scope: String,
) -> Result<Prompt, String> {
    let (name, body, scope) = checked(&name, &body, &scope)?;
    let now = now_ms();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!("prompt-{nanos:x}");
    let library = lock(&state)?;
    library
        .create_prompt(&id, name, body, scope.as_str(), now)
        .map_err(|e| e.to_string())?;
    fetch(&library, &id)
}

/// Replaces a prompt's name, body and scope.
#[tauri::command]
pub(crate) fn update_prompt(
    state: State<'_, AppState>,
    id: String,
    name: String,
    body: String,
    scope: String,
) -> Result<Prompt, String> {
    let (name, body, scope) = checked(&name, &body, &scope)?;
    let library = lock(&state)?;
    if !library
        .update_prompt(&id, name, body, scope.as_str(), now_ms())
        .map_err(|e| e.to_string())?
    {
        return Err(format!("no prompt {id}"));
    }
    fetch(&library, &id)
}

/// Deletes one of the user's prompts. A built-in can be reset, not deleted.
#[tauri::command]
pub(crate) fn delete_prompt(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let library = lock(&state)?;
    if library.delete_prompt(&id).map_err(|e| e.to_string())? {
        Ok(())
    } else {
        Err("a built-in prompt can't be deleted; reset it instead".to_owned())
    }
}

/// Puts a built-in prompt back to its shipped text.
#[tauri::command]
pub(crate) fn reset_prompt(state: State<'_, AppState>, id: String) -> Result<Prompt, String> {
    let library = lock(&state)?;
    if !library
        .reset_prompt(&id, now_ms())
        .map_err(|e| e.to_string())?
    {
        return Err("only a built-in prompt can be reset".to_owned());
    }
    fetch(&library, &id)
}

/// A saved meeting's stored runs, newest first.
#[tauri::command]
pub(crate) fn list_prompt_runs(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<Vec<PromptRun>, String> {
    lock(&state)?
        .prompt_runs(&meeting_id)
        .map_err(|e| e.to_string())
}

/// Deletes one stored run.
#[tauri::command]
pub(crate) fn delete_prompt_run(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    lock(&state)?
        .delete_prompt_run(id)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Runs prompt `prompt_id` and returns its output. With `meeting_id`, over that saved meeting (the
/// run is stored with it); without, over the live `transcript` the webview passes (not stored).
/// `speaker` is required for a speaker-scoped prompt. `title` and `date` fill `{title}` and
/// `{date}`; for a saved meeting they default to its title and its UTC start date.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_prompt(
    app: AppHandle,
    prompt_id: String,
    speaker: Option<String>,
    meeting_id: Option<String>,
    transcript: Option<String>,
    title: Option<String>,
    date: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let job = RunJob {
            prompt_id,
            speaker,
            meeting_id,
            transcript,
            title,
            date,
        };
        run_blocking(&app, job)
    })
    .await
    .map_err(|e| format!("prompt task failed: {e}"))?
}

struct RunJob {
    prompt_id: String,
    speaker: Option<String>,
    meeting_id: Option<String>,
    transcript: Option<String>,
    title: Option<String>,
    date: Option<String>,
}

/// Everything a run needs from the library, read under one short lock.
struct Prepared {
    prompt: Prompt,
    transcript: String,
    vars: PromptVars,
}

fn prepare(library: &Library, job: &RunJob, about_me: String) -> Result<Prepared, String> {
    let prompt = fetch(library, &job.prompt_id)?;
    let (transcript, title, started_ms, project) = match &job.meeting_id {
        Some(id) => {
            let (note, _) = library
                .get_note(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no meeting {id}"))?;
            let transcript = library
                .meeting_transcript(id)
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let project = note
                .project_id
                .as_deref()
                .map(|p| project_text(library, p))
                .unwrap_or_default();
            (transcript, note.title, note.started_at_ms, project)
        }
        None => (
            job.transcript.clone().unwrap_or_default(),
            String::new(),
            now_ms(),
            String::new(),
        ),
    };
    let vars = PromptVars {
        speaker: job.speaker.clone(),
        title: job
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(title),
        date: job
            .date
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| utc_date(started_ms)),
        about_me,
        project,
    };
    Ok(Prepared {
        prompt,
        transcript,
        vars,
    })
}

/// A project's name, then its instructions when it has any.
fn project_text(library: &Library, project_id: &str) -> String {
    let name = library
        .list_projects()
        .ok()
        .and_then(|ps| ps.into_iter().find(|p| p.id == project_id))
        .map_or_else(|| project_id.to_owned(), |p| p.name);
    match library.project_instructions(project_id).ok().flatten() {
        Some(i) if !i.trim().is_empty() => format!("{name}\n{}", i.trim()),
        _ => name,
    }
}

/// The instructions the model gets: the filled prompt and how to read the transcript.
fn instructions(prompt: &Prompt, vars: &PromptVars) -> Result<String, String> {
    let scope = PromptScope::parse(&prompt.scope).unwrap_or(PromptScope::Meeting);
    let body = fill_prompt(&prompt.body, scope, vars).map_err(|e| e.to_string())?;
    Ok(format!(
        "{body}\n\nThe context is the meeting transcript, one \"Speaker: text\" line per turn. \
         \"You\" is the person who recorded it."
    ))
}

fn run_blocking(app: &AppHandle, job: RunJob) -> Result<String, String> {
    let state = app.state::<AppState>();
    let about_me = state.reasoning.about_me();
    let prepared = prepare(&*lock(&state)?, &job, about_me)?;
    if prepared.transcript.trim().is_empty() {
        return Err(
            "there's no transcript to run this on (a saved transcript may have expired)".to_owned(),
        );
    }
    let scope = PromptScope::parse(&prepared.prompt.scope).unwrap_or(PromptScope::Meeting);
    let system = instructions(&prepared.prompt, &prepared.vars)?;

    let inner = match &job.meeting_id {
        Some(id) => crate::reasoning::backend_for(&state, Some(id.clone())),
        None => crate::reasoning::backend(&state),
    };
    let backend = Arc::new(AsPromptRun::new(inner));
    let text = crate::assist::chat::run_on_reasoning(
        backend.clone(),
        &system,
        &prepared.transcript,
        crate::assist::chat::reasoning_context_tokens(&state),
    )?;

    if let Some(meeting_id) = job.meeting_id {
        let run = PromptRun {
            id: 0,
            meeting_id,
            prompt_name: prepared.prompt.name,
            speaker: (scope == PromptScope::Speaker)
                .then_some(prepared.vars.speaker)
                .flatten(),
            output: text.clone(),
            backend: backend.answered().unwrap_or_default(),
            at_ms: now_ms(),
        };
        lock(&state)?
            .insert_prompt_run(&run)
            .map_err(|e| e.to_string())?;
    }
    Ok(text)
}

/// Saves a prompt's output as a Markdown file the user picks. Returns `false` on cancel.
#[tauri::command]
pub(crate) async fn save_prompt_output(
    app: AppHandle,
    text: String,
    default_name: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let Some(picked) = app
            .dialog()
            .file()
            .set_file_name(format!("{default_name}.md"))
            .add_filter("MD", &["md"])
            .blocking_save_file()
        else {
            return Ok(false);
        };
        let dest = picked.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&dest, text).map_err(|e| format!("write {}: {e}", dest.display()))?;
        Ok(true)
    })
    .await
    .map_err(|e| format!("save task failed: {e}"))?
}

/// Files every call under [`TaskKind::PromptRun`] (the activity log shows it as a prompt run, not
/// an assist call) and remembers which backend answered last.
struct AsPromptRun {
    inner: Arc<dyn ReasoningBackend>,
    answered: Mutex<Option<String>>,
}

impl AsPromptRun {
    fn new(inner: Arc<dyn ReasoningBackend>) -> Self {
        Self {
            inner,
            answered: Mutex::new(None),
        }
    }

    fn answered(&self) -> Option<String> {
        self.answered.lock().ok().and_then(|a| a.clone())
    }
}

impl ReasoningBackend for AsPromptRun {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn model(&self) -> Option<&str> {
        self.inner.model()
    }
    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        let req = ReasoningRequest {
            task: TaskKind::PromptRun,
            ..req.clone()
        };
        let resp = self.inner.invoke(&req, cancel)?;
        if let Ok(mut a) = self.answered.lock() {
            *a = Some(resp.backend.clone());
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, SpeakerId, TranscriptSegment};

    fn job(prompt: &str, meeting: Option<&str>) -> RunJob {
        RunJob {
            prompt_id: prompt.into(),
            speaker: Some("Alice".into()),
            meeting_id: meeting.map(str::to_owned),
            transcript: Some("You: hi".into()),
            title: None,
            date: None,
        }
    }

    fn library() -> Library {
        let mut lib = Library::open_in_memory().unwrap();
        let seg = TranscriptSegment {
            id: 0,
            text: "Ship it Friday.".into(),
            start: Duration::ZERO,
            end: Duration::from_millis(900),
            status: SegmentStatus::Final,
            source: AudioSourceKind::System,
            speaker: Some(SpeakerId(0)),
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        };
        let meta = MeetingMeta {
            title: Some("Launch sync".into()),
            ..MeetingMeta::default()
        };
        lib.save_note("m1", &meta, 1_790_000_000_000, &[seg])
            .unwrap();
        lib.set_speaker_name("m1", 0, "Alice").unwrap();
        lib.create_project("p", "Acme", 0).unwrap();
        lib.set_project_instructions("p", "Focus on Azure.")
            .unwrap();
        lib.set_meeting_project("m1", Some("p")).unwrap();
        lib
    }

    #[test]
    fn prompt_fields_are_checked() {
        assert_eq!(
            checked(" Mine ", " Do it. ", "speaker").unwrap(),
            ("Mine", "Do it.", PromptScope::Speaker)
        );
        assert!(checked(" ", "x", "meeting").is_err());
        assert!(checked("x", " ", "meeting").is_err());
        assert!(checked("x", "y", "team").is_err());
    }

    #[test]
    fn a_saved_meeting_fills_its_title_date_project_and_named_transcript() {
        let lib = library();
        let p = prepare(&lib, &job("builtin-summary", Some("m1")), "PM".into()).unwrap();
        assert_eq!(p.transcript, "Alice: Ship it Friday.");
        assert_eq!(p.vars.title, "Launch sync");
        assert_eq!(p.vars.date, "2026-09-21");
        assert_eq!(p.vars.project, "Acme\nFocus on Azure.");
        assert_eq!(p.vars.about_me, "PM");
        let system = instructions(&p.prompt, &p.vars).unwrap();
        assert!(system.starts_with("Summarize the meeting \"Launch sync\" (2026-09-21)"));

        assert!(prepare(&lib, &job("nope", Some("m1")), String::new()).is_err());
        assert!(prepare(&lib, &job("builtin-summary", Some("gone")), String::new()).is_err());
    }

    #[test]
    fn a_live_run_uses_the_transcript_it_is_given() {
        let lib = library();
        let mut j = job("builtin-communication-feedback", None);
        j.title = Some("Standup".into());
        j.date = Some("30 Sep".into());
        let p = prepare(&lib, &j, String::new()).unwrap();
        assert_eq!(p.transcript, "You: hi");
        assert_eq!(
            (p.vars.title.as_str(), p.vars.date.as_str()),
            ("Standup", "30 Sep")
        );
        assert!(instructions(&p.prompt, &p.vars)
            .unwrap()
            .starts_with("Give communication feedback to Alice"));

        j.speaker = None;
        let p = prepare(&lib, &j, String::new()).unwrap();
        assert!(
            instructions(&p.prompt, &p.vars).is_err(),
            "a speaker prompt needs a speaker"
        );
    }

    #[test]
    fn calls_are_filed_as_prompt_runs_and_the_answering_backend_is_kept() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let scripted = wisp_reasoning::ScriptedBackend::with_responder("scripted", move |req| {
            log.lock().unwrap().push(req.task);
            Ok(serde_json::json!({"text": "Done."}))
        });
        let backend = Arc::new(AsPromptRun::new(Arc::new(scripted)));
        let out =
            crate::assist::chat::run_on_reasoning(backend.clone(), "Summarize.", "You: hi", 1000)
                .unwrap();
        assert_eq!(out, "Done.");
        assert_eq!(*seen.lock().unwrap(), [TaskKind::PromptRun]);
        assert_eq!(backend.answered().as_deref(), Some("scripted"));
    }
}

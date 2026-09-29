//! The AI activity log: every model call the app makes, with what was sent and received, stored in
//! the library for the Settings › AI activity view. The records come from
//! [`wisp_reasoning::AuditingBackend`] and [`wisp_reasoning::audited`]; this module files each one
//! under the meeting it was made for and writes it on a background thread, so a caller holding the
//! library lock can never deadlock on its own call's record.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager, State};
use wisp_library::{LlmCall, LlmTotals};
use wisp_reasoning::{AuditRecord, AuditSink};

use crate::AppState;

/// Most calls the view loads at once; each can hold a whole transcript.
const MAX_LISTED: usize = 200;

/// The log's share of [`AppState`].
pub(crate) struct AuditState {
    /// The id the running live meeting will be saved under; `None` between sessions.
    live_meeting: Mutex<Option<String>>,
    tx: Sender<LlmCall>,
    /// Taken by the writer thread once the app state is managed.
    rx: Mutex<Option<Receiver<LlmCall>>>,
}

impl Default for AuditState {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            live_meeting: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
}

/// Starts the thread that writes records into the library. Call once, after `manage`.
pub(crate) fn spawn_writer(app: &AppHandle) {
    let Some(rx) = app
        .state::<AppState>()
        .audit
        .rx
        .lock()
        .ok()
        .and_then(|mut r| r.take())
    else {
        return;
    };
    let handle = app.clone();
    std::thread::spawn(move || {
        for call in rx {
            let state = handle.state::<AppState>();
            let Ok(library) = state.library.lock() else {
                continue;
            };
            if let Err(e) = library.insert_llm_call(&call) {
                eprintln!("wisp: logging an AI call failed: {e}");
            }
        }
    });
}

/// Sets (or with `None` clears) the running live meeting's id.
pub(crate) fn set_live_meeting(state: &AppState, id: Option<String>) {
    if let Ok(mut slot) = state.audit.live_meeting.lock() {
        *slot = id.filter(|s| !s.trim().is_empty());
    }
}

/// The running live meeting's id, if a session is running.
pub(crate) fn live_meeting(state: &AppState) -> Option<String> {
    state.audit.live_meeting.lock().ok().and_then(|m| m.clone())
}

/// A sink that files each record under `meeting`.
pub(crate) fn sink(state: &AppState, meeting: Option<String>) -> AuditSink {
    let tx = state.audit.tx.clone();
    Arc::new(move |record| {
        let _ = tx.send(to_call(record, meeting.clone()));
    })
}

fn to_call(r: AuditRecord, meeting_id: Option<String>) -> LlmCall {
    LlmCall {
        id: 0,
        at_ms: r.at_ms,
        meeting_id,
        task: r.task,
        backend: r.backend,
        model: r.model,
        local: r.local,
        instructions: r.instructions,
        context: r.context,
        images: r.images.iter().map(|p| p.display().to_string()).collect(),
        output: r.output,
        error: r.error,
        elapsed_ms: i64::try_from(r.elapsed_ms).unwrap_or(i64::MAX),
        tokens_in: i64::try_from(r.tokens_in).unwrap_or(i64::MAX),
        tokens_out: i64::try_from(r.tokens_out).unwrap_or(i64::MAX),
        tokens_estimated: r.tokens_estimated,
        cache_read_tokens: i64::try_from(r.cache_read_tokens).unwrap_or(i64::MAX),
        cache_write_tokens: i64::try_from(r.cache_write_tokens).unwrap_or(i64::MAX),
        cost_usd: r.cost_usd,
        model_reported: r.model_reported,
    }
}

/// The newest logged calls, newest first; only `meeting_id`'s when given.
#[tauri::command]
pub(crate) fn list_ai_activity(
    state: State<'_, AppState>,
    meeting_id: Option<String>,
) -> Result<Vec<LlmCall>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .llm_calls(meeting_id.as_deref(), MAX_LISTED)
        .map_err(|e| e.to_string())
}

/// Calls, tokens and reported cost over the whole log, or only `meeting_id`'s when given.
#[tauri::command]
pub(crate) fn ai_activity_totals(
    state: State<'_, AppState>,
    meeting_id: Option<String>,
) -> Result<LlmTotals, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .llm_call_totals(meeting_id.as_deref())
        .map_err(|e| e.to_string())
}

/// The running live meeting's id, for the view's "this meeting" filter.
#[tauri::command]
pub(crate) fn ai_activity_live_meeting(state: State<'_, AppState>) -> Option<String> {
    live_meeting(&state)
}

/// Deletes the whole log. Returns how many calls it held.
#[tauri::command]
pub(crate) fn clear_ai_activity(state: State<'_, AppState>) -> Result<usize, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .clear_llm_calls()
        .map_err(|e| e.to_string())
}

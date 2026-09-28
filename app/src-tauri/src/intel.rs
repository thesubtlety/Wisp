//! Live meeting intelligence, wired into a live session.
//!
//! When the session starts with intelligence on, [`start`] spawns a [`wisp_intel::IntelRuntime`].
//! The live sink calls [`route_final`] for every admitted final — a quick lock and a channel send,
//! so a slow model can never hold up audio or transcription. Each pass result goes to the webview as
//! [`INTEL_EVENT`]. [`stop`] runs after capture teardown and parks the meeting's state and log until
//! `save_note` stores them with [`persist`], remapped to the saved transcript's segment ids. With
//! auto-save off nothing is stored, the same as the transcript.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use wisp_core::transcript::{AudioSourceKind, TranscriptSegment};
use wisp_intel::{
    ask, remap_refs, saved_positions, AppliedOp, AskAnswer, AskInput, AskTurn, Card,
    EndgameTrigger, Finished, Gap, IntelRuntime, IntelUpdate, LogEntry, MeetingState, Retriever,
    RuntimeConfig, StateItem, TranscriptLine, LIVE_MEETING_ID,
};
use wisp_library::{meeting_ref, Library, RetrievalQuery, Snippet, StoredLogEntry, StoredOp};
use wisp_reasoning::{CancelToken, FallbackBackend};

use crate::AppState;

/// Emitted after every intelligence pass attempt.
pub(crate) const INTEL_EVENT: &str = "intel://update";

/// The live runtime, and the last meeting's result waiting to be saved.
#[derive(Default)]
pub(crate) struct IntelState {
    runtime: Mutex<Option<IntelRuntime>>,
    finished: Mutex<Option<Finished>>,
    /// The question being answered, so it can be cancelled.
    ask_cancel: Mutex<Option<CancelToken>>,
}

/// A pass result as the webview sees it.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum IntelUpdateDto {
    #[serde(rename_all = "camelCase")]
    Pass {
        applied: usize,
        rejected: usize,
        items: Vec<StateItem>,
        remaining_lines: usize,
        backend: String,
        cards: Vec<Card>,
    },
    NothingNew,
    WrapSuggested {
        trigger: EndgameTrigger,
    },
    #[serde(rename_all = "camelCase")]
    Audit {
        gaps: Vec<Gap>,
        rejected: usize,
        markdown: String,
        backend: String,
    },
    Failed {
        message: String,
    },
}

impl From<IntelUpdate> for IntelUpdateDto {
    fn from(update: IntelUpdate) -> Self {
        match update {
            IntelUpdate::Pass {
                applied,
                rejected,
                live_items,
                remaining_lines,
                backend,
                cards,
            } => IntelUpdateDto::Pass {
                applied,
                rejected,
                items: live_items,
                remaining_lines,
                backend,
                cards,
            },
            IntelUpdate::NothingNew => IntelUpdateDto::NothingNew,
            IntelUpdate::WrapSuggested(trigger) => IntelUpdateDto::WrapSuggested { trigger },
            IntelUpdate::Audit { report, backend } => IntelUpdateDto::Audit {
                markdown: report.to_markdown(),
                rejected: report.rejected.len(),
                gaps: report.gaps,
                backend,
            },
            IntelUpdate::Failed(message) => IntelUpdateDto::Failed { message },
        }
    }
}

/// An answer as the webview gets it: the checked answer plus its Markdown for the copy button.
#[derive(Serialize)]
pub(crate) struct AskAnswerDto {
    #[serde(flatten)]
    answer: AskAnswer,
    markdown: String,
}

/// Retrieves project context from the library, scoped to the meeting's project. A meeting outside
/// any project gets none: the project is the boundary for what reaches a reasoning backend.
struct LibraryRetriever {
    app: AppHandle,
    project_id: Option<String>,
}

impl Retriever for LibraryRetriever {
    fn retrieve(&self, text: &str) -> Vec<Snippet> {
        let Some(project) = &self.project_id else {
            return Vec::new();
        };
        let state = self.app.state::<AppState>();
        let Ok(library) = state.library.lock() else {
            return Vec::new();
        };
        library
            .retrieve(&RetrievalQuery {
                text,
                project_id: Some(project),
                include_meetings: true,
                exclude_meeting_id: None,
                limit: 6,
                max_total_chars: 3000,
            })
            .unwrap_or_default()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Drops any runtime and the previous meeting's parked result. Called when a live session starts,
/// and when a start fails.
pub(crate) fn reset(state: &AppState) {
    let runtime = state.intel.runtime.lock().ok().and_then(|mut r| r.take());
    drop(runtime); // stops the worker, outside the lock
    if let Ok(mut finished) = state.intel.finished.lock() {
        *finished = None;
    }
}

/// Resets intelligence when dropped, unless disarmed: a session start that fails after starting the
/// runtime must not leave it behind.
pub(crate) struct StartGuard<'a> {
    state: &'a AppState,
    armed: bool,
}

impl<'a> StartGuard<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self { state, armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            reset(self.state);
        }
    }
}

/// Starts the runtime for a new live session, using the Codex CLI and falling back to Claude Code.
pub(crate) fn start(app: &AppHandle) {
    let emitter = app.clone();
    let runtime = IntelRuntime::spawn(
        Arc::new(FallbackBackend::codex_then_claude()),
        Box::new(LibraryRetriever {
            app: app.clone(),
            project_id: None,
        }),
        RuntimeConfig::default(),
        Box::new(move |update| {
            match &update {
                IntelUpdate::Pass {
                    applied,
                    rejected,
                    backend,
                    ..
                } => eprintln!(
                    "wisp: intel pass via {backend}: {applied} applied, {rejected} rejected"
                ),
                IntelUpdate::Failed(e) => eprintln!("wisp: intel pass failed: {e}"),
                IntelUpdate::Audit { report, backend } => eprintln!(
                    "wisp: intel audit via {backend}: {} gaps, {} rejected",
                    report.gaps.len(),
                    report.rejected.len()
                ),
                IntelUpdate::NothingNew | IntelUpdate::WrapSuggested(_) => {}
            }
            let _ = emitter.emit(INTEL_EVENT, IntelUpdateDto::from(update));
        }),
        Box::new(now_ms),
    );
    let state = app.state::<AppState>();
    let Ok(mut slot) = state.intel.runtime.lock() else {
        return;
    };
    *slot = Some(runtime);
}

/// The speaker label a line carries into the reasoning context.
fn speaker_label(segment: &TranscriptSegment) -> String {
    match (segment.source, segment.speaker) {
        (AudioSourceKind::Microphone, _) => "You".to_owned(),
        (_, Some(s)) => format!("Speaker {}", s.0 + 1),
        _ => "Them".to_owned(),
    }
}

/// Hands one admitted final to the running runtime, if any. Never blocks on a model.
pub(crate) fn route_final(app: &AppHandle, segment: &TranscriptSegment) {
    let state = app.state::<AppState>();
    let Ok(guard) = state.intel.runtime.lock() else {
        return;
    };
    if let Some(runtime) = guard.as_ref() {
        runtime.push_final(
            speaker_label(segment),
            segment.start.as_millis() as i64,
            segment.text.clone(),
        );
    }
}

/// The live transcript as the runtime numbers it: admitted finals in arrival order, blank lines
/// dropped, so `T<n>` means the same line to Ask as to the state's evidence.
fn live_lines(retained: &[TranscriptSegment]) -> Vec<TranscriptLine> {
    retained
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .enumerate()
        .map(|(i, s)| TranscriptLine {
            idx: i as i64,
            start_ms: s.start.as_millis() as i64,
            speaker: speaker_label(s),
            text: s.text.clone(),
        })
        .collect()
}

/// The current meeting's state: the running runtime's latest, else the stopped meeting's, else empty.
fn current_state(state: &AppState) -> MeetingState {
    if let Some(runtime) = state
        .intel
        .runtime
        .lock()
        .ok()
        .as_ref()
        .and_then(|r| r.as_ref())
    {
        return runtime.snapshot();
    }
    if let Some(finished) = state
        .intel
        .finished
        .lock()
        .ok()
        .as_ref()
        .and_then(|f| f.as_ref())
    {
        return finished.state.clone();
    }
    MeetingState::new(LIVE_MEETING_ID)
}

/// Stops the runtime (cancelling any pass in flight) and parks its result for `save_note`.
pub(crate) fn stop(state: &AppState) {
    let runtime = state.intel.runtime.lock().ok().and_then(|mut r| r.take());
    if let Some(runtime) = runtime {
        let finished = runtime.stop();
        if let Ok(mut slot) = state.intel.finished.lock() {
            *slot = Some(finished);
        }
    }
}

/// Stores the parked meeting state under the saved meeting's id, with its transcript refs remapped
/// from arrival order to the saved order. `retained` is the live buffer in arrival order. The
/// runtime saw a prefix of the same finals (blank lines dropped on both sides), so arrival index
/// `a` is the a-th non-blank retained final. Does nothing if no meeting is parked.
pub(crate) fn persist(
    state: &AppState,
    library: &mut Library,
    meeting_id: &str,
    retained: &[TranscriptSegment],
) -> Result<(), String> {
    persist_parked(&state.intel, library, meeting_id, retained)
}

fn persist_parked(
    intel: &IntelState,
    library: &mut Library,
    meeting_id: &str,
    retained: &[TranscriptSegment],
) -> Result<(), String> {
    let Some(finished) = intel.finished.lock().ok().and_then(|mut f| f.take()) else {
        return Ok(());
    };
    if finished.log.is_empty() && finished.candidate_log.is_empty() {
        return Ok(());
    }
    let starts: Vec<_> = retained
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| s.start)
        .collect();
    let saved = saved_positions(&starts);
    let live_prefix = format!("M{LIVE_MEETING_ID}:T");
    let to_saved = |r: &str| -> Option<String> {
        let arrival: usize = r.strip_prefix(&live_prefix)?.parse().ok()?;
        saved.get(arrival).map(|&idx| meeting_ref(meeting_id, idx))
    };
    let log = remap_refs(&finished.log, to_saved);
    let ops = log
        .iter()
        .map(|a| {
            Ok(StoredOp {
                seq: a.seq as i64,
                at_ms: a.at_ms,
                op: serde_json::to_string(&a.op).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if !ops.is_empty() {
        library
            .save_state_ops(meeting_id, &ops)
            .map_err(|e| e.to_string())?;
    }

    let entries = finished
        .candidate_log
        .iter()
        .enumerate()
        .map(|(seq, entry)| {
            let mut entry = entry.clone();
            let at_ms = match &mut entry {
                LogEntry::Considered {
                    at_ms, candidate, ..
                } => {
                    if let Some(c) = candidate {
                        c.source_refs = c
                            .source_refs
                            .iter()
                            .map(|r| to_saved(r).unwrap_or_else(|| r.clone()))
                            .collect();
                    }
                    *at_ms
                }
                LogEntry::Dismissed { at_ms, .. } => *at_ms,
            };
            Ok(StoredLogEntry {
                seq: seq as i64,
                at_ms,
                entry: serde_json::to_string(&entry).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if !entries.is_empty() {
        library
            .save_candidate_log(meeting_id, &entries)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Runs `f` on the live runtime. `false` if intelligence isn't running.
fn with_runtime(state: &AppState, f: impl FnOnce(&IntelRuntime)) -> Result<bool, String> {
    let guard = state
        .intel
        .runtime
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    Ok(match guard.as_ref() {
        Some(runtime) => {
            f(runtime);
            true
        }
        None => false,
    })
}

/// Wrapping Up: enters endgame and runs the gap audit now. Recording goes on. `false` if
/// intelligence isn't running.
#[tauri::command]
pub(crate) fn intel_wrap_up(state: State<'_, AppState>) -> Result<bool, String> {
    with_runtime(&state, IntelRuntime::wrap_up)
}

/// Sets or clears the meeting's scheduled end (epoch ms). `false` if intelligence isn't running.
#[tauri::command]
pub(crate) fn intel_set_scheduled_end(
    state: State<'_, AppState>,
    end_ms: Option<i64>,
) -> Result<bool, String> {
    with_runtime(&state, |r| r.set_scheduled_end(end_ms))
}

/// Records that the user dismissed a card. `false` if intelligence isn't running.
#[tauri::command]
pub(crate) fn intel_dismiss_card(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let guard = state
        .intel
        .runtime
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    Ok(match guard.as_ref() {
        Some(runtime) => {
            runtime.dismiss(id);
            true
        }
        None => false,
    })
}

/// Asks the running runtime for a pass now. `false` if intelligence isn't running.
#[tauri::command]
pub(crate) fn intel_analyze_now(state: State<'_, AppState>) -> Result<bool, String> {
    let guard = state
        .intel
        .runtime
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    Ok(match guard.as_ref() {
        Some(runtime) => {
            runtime.analyze_now();
            true
        }
        None => false,
    })
}

/// Answers a question about the current (or just-finished) live meeting from its transcript and
/// state, with checked citations. One question at a time; a new one cancels the last.
#[tauri::command]
pub(crate) async fn intel_ask(
    app: AppHandle,
    question: String,
    history: Vec<AskTurn>,
) -> Result<AskAnswerDto, String> {
    let question = question.trim().to_owned();
    if question.is_empty() {
        return Err("empty question".to_owned());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cancel = CancelToken::new();
        if let Ok(mut slot) = state.intel.ask_cancel.lock() {
            if let Some(previous) = slot.replace(cancel.clone()) {
                previous.cancel();
            }
        }
        let transcript = live_lines(
            &state
                .live_segments
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())?,
        );
        let meeting = current_state(&state);
        let retrieved = LibraryRetriever {
            app: app.clone(),
            project_id: None,
        }
        .retrieve(&question);
        let backend = FallbackBackend::codex_then_claude();
        let result = ask(
            &backend,
            &cancel,
            &AskInput {
                question: &question,
                history: &history,
                transcript: &transcript,
                state: &meeting,
                retrieved: &retrieved,
                timeout: std::time::Duration::from_secs(180),
            },
        )
        .map(|answer| AskAnswerDto {
            markdown: answer.to_markdown(),
            answer,
        })
        .map_err(|e| e.to_string());
        // A newer question cancels this one's token as it takes the slot, so an uncancelled token
        // means the slot still holds it.
        if !cancel.is_cancelled() {
            if let Ok(mut slot) = state.intel.ask_cancel.lock() {
                *slot = None;
            }
        }
        result
    })
    .await
    .map_err(|e| format!("ask task failed: {e}"))?
}

/// Cancels the question being answered, if any.
#[tauri::command]
pub(crate) fn intel_ask_cancel(state: State<'_, AppState>) {
    if let Some(cancel) = state
        .intel
        .ask_cancel
        .lock()
        .ok()
        .and_then(|mut c| c.take())
    {
        cancel.cancel();
    }
}

/// The live items of a saved meeting's state, rebuilt from its stored log. Empty if it has none.
#[tauri::command]
pub(crate) fn intel_saved_items(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<StateItem>, String> {
    let stored = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .state_ops(&id)
        .map_err(|e| e.to_string())?;
    let log = stored
        .into_iter()
        .map(|s| {
            Ok(AppliedOp {
                seq: s.seq as u64,
                at_ms: s.at_ms,
                op: serde_json::from_str(&s.op).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let meeting = MeetingState::replay(&id, &log).map_err(|e| e.to_string())?;
    Ok(meeting.live_items().into_iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wisp_core::transcript::{SegmentStatus, SpeakerId};
    use wisp_intel::{EpistemicStatus, ItemKind, ResolvedOp};

    fn seg(source: AudioSourceKind, start_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id: 0,
            text: text.to_owned(),
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(start_ms + 500),
            status: SegmentStatus::Final,
            source,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    #[test]
    fn live_lines_number_admitted_finals_like_the_runtime() {
        let retained = vec![
            seg(AudioSourceKind::System, 5000, "We host in Azure."),
            seg(AudioSourceKind::System, 5500, "  "),
            seg(AudioSourceKind::Microphone, 3000, "Where do you host?"),
        ];
        let lines = live_lines(&retained);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[1].idx, lines[1].start_ms), (1, 3000));
        assert_eq!(lines[1].speaker, "You");
        assert_eq!(lines[0].speaker, "Them");
    }

    #[test]
    fn speaker_labels_follow_the_source_and_diarization() {
        let mut s = seg(AudioSourceKind::Microphone, 0, "x");
        assert_eq!(speaker_label(&s), "You");
        s.source = AudioSourceKind::System;
        assert_eq!(speaker_label(&s), "Them");
        s.speaker = Some(SpeakerId(1));
        assert_eq!(speaker_label(&s), "Speaker 2");
    }

    #[test]
    fn a_parked_log_is_saved_with_refs_pointing_at_the_saved_segments() {
        let state_intel = IntelState::default();
        // Arrival order: a system line at 5s, a blank, a mic line that started at 3s.
        let retained = vec![
            seg(AudioSourceKind::System, 5000, "We host in Azure."),
            seg(AudioSourceKind::System, 5500, "  "),
            seg(AudioSourceKind::Microphone, 3000, "Where do you host?"),
        ];
        let add = AppliedOp {
            seq: 0,
            at_ms: 9,
            op: ResolvedOp::Add {
                id: "REQ-1".into(),
                kind: ItemKind::Requirement,
                text: "Hosting in Azure".into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                // Arrival indexes 0 and 1 (the blank never reached the runtime).
                source_refs: vec!["Mlive:T0".into(), "Mlive:T1".into()],
                related_items: vec![],
                owner: None,
                due: None,
            },
        };
        let finished_state = MeetingState::replay(LIVE_MEETING_ID, [&add]).unwrap();
        let mut filter = wisp_intel::InterventionFilter::default();
        filter.consider(
            wisp_intel::Candidate {
                kind: wisp_intel::CandidateKind::Conflict,
                title: "Hosting unclear".into(),
                detail: String::new(),
                suggested_question: None,
                source_refs: vec!["Mlive:T0".into()],
                cited: vec!["T0".into()],
                related_items: vec![],
                importance: 0.9,
                urgency: 0.9,
                confidence: 0.9,
                future_work_risk: 0.9,
            },
            7,
        );
        filter.dismiss("CARD-1", 8);
        *state_intel.finished.lock().unwrap() = Some(Finished {
            state: finished_state,
            log: vec![add],
            lines: 2,
            candidate_log: filter.log().to_vec(),
        });

        let mut library = Library::open_in_memory().unwrap();
        let meta = wisp_core::export::MeetingMeta {
            title: Some("t".into()),
            date: None,
            engine: None,
            language: None,
            summary: None,
        };
        let mut sorted = retained.clone();
        sorted.sort_by_key(|s| s.start);
        library.save_note("m1", &meta, 0, &sorted).unwrap();

        persist_parked(&state_intel, &mut library, "m1", &retained).unwrap();
        let stored = library.state_ops("m1").unwrap();
        assert_eq!(stored.len(), 1);
        let op: ResolvedOp = serde_json::from_str(&stored[0].op).unwrap();
        let ResolvedOp::Add { source_refs, .. } = op else {
            panic!("expected an add");
        };
        // Saved order: mic line (3s) is T0, system line (5s) is T1.
        assert_eq!(source_refs, ["Mm1:T1", "Mm1:T0"]);
        let logged = library.candidate_log("m1").unwrap();
        assert_eq!(logged.len(), 2);
        assert!(
            logged[0].entry.contains("\"Mm1:T1\""),
            "{}",
            logged[0].entry
        );
        assert!(logged[1].entry.contains("dismissed"));
        let (_, segments) = library.get_note("m1").unwrap().unwrap();
        assert_eq!(segments[1].text, "We host in Azure.");
        assert_eq!(segments[0].text, "Where do you host?");

        // The parked result is consumed: a re-save stores nothing new and keeps the log.
        persist_parked(&state_intel, &mut library, "m1", &retained).unwrap();
        assert_eq!(library.state_ops("m1").unwrap().len(), 1);
    }
}

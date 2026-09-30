//! Live meeting intelligence, wired into a live session.
//!
//! When the session starts with intelligence on, [`start`] spawns a [`wisp_intel::IntelRuntime`].
//! The live sink calls [`route_final`] for every admitted final — a quick lock and a channel send,
//! so a slow model can never hold up audio or transcription. Each pass result goes to the webview as
//! [`INTEL_EVENT`]. [`stop`] runs after capture teardown and parks the meeting's state and log until
//! `save_note` stores them with [`persist`], remapped to the saved transcript's segment ids. With
//! auto-save off nothing is stored, the same as the transcript.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use wisp_core::speakers::{line_speaker, speaker_display, SpeakerNames};
use wisp_core::transcript::{AudioSourceKind, SpeakerId, TranscriptSegment};
use wisp_intel::{
    apply_edits, ask, fallback_followups, generate_followups, interpret_reply, parse_reply,
    remap_refs, review_ops, saved_positions, AppliedOp, AskAnswer, AskInput, AskTurn, Card,
    EndgameTrigger, Finished, FollowUp, FollowUpClass, Gap, IntelRuntime, IntelUpdate, LogEntry,
    MeetingState, ProjectContext, Retriever, ReviewEdit, RuntimeConfig, SpeakerSuggestion,
    StateItem, TranscriptLine, LIVE_MEETING_ID,
};
use wisp_intel::{context_packet, meeting_record, memory_ref, state_json, ExportMeta};
use wisp_intel::{iso_date, project_brief, BriefInput, BriefMeeting};
use wisp_intel::{parse_summary, participants, summary_context, summary_request, SummaryMeta};
use wisp_intel::{propose_learning, LearningInput, Proposal};
use wisp_library::{meeting_ref, Library, RetrievalQuery, Snippet, StoredLogEntry, StoredOp};
use wisp_library::{MeetingKnowledge, MemoryEntry, Project};
use wisp_reasoning::CancelToken;

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
    /// The post-call review in progress: which saved meeting, and its follow-ups.
    review: Mutex<Option<(String, Vec<FollowUp>)>>,
    /// The last applied review's "save to project" follow-ups, for project learning.
    review_project: Mutex<Vec<FollowUp>>,
    /// The project of the current (or just-finished) live meeting.
    project: Mutex<Option<String>>,
    /// Screenshots attached to the current (or just-finished) live meeting.
    pub(crate) context: crate::context::ContextState,
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
    /// The live speaker-name suggestions (the whole list).
    SpeakerNames {
        suggestions: Vec<SpeakerSuggestion>,
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
            IntelUpdate::SpeakerNames(suggestions) => IntelUpdateDto::SpeakerNames { suggestions },
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

pub(crate) fn now_ms() -> i64 {
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
            // The meeting never started, so later calls must not be logged under it.
            crate::audit::set_live_meeting(self.state, None);
        }
    }
}

/// Starts the runtime for a new live session, using the Codex CLI and falling back to Claude Code.
/// `meeting_label` names the meeting in screenshot labels.
pub(crate) fn start(app: &AppHandle, project_id: Option<String>, meeting_label: Option<String>) {
    crate::context::begin(app, project_id.clone(), meeting_label);
    let emitter = app.clone();
    let state = app.state::<AppState>();
    let memory = project_memory(&state, project_id.as_deref());
    let focus = about_you(&state, project_id.as_deref());
    if let Ok(mut slot) = state.intel.project.lock() {
        slot.clone_from(&project_id);
    }
    let runtime = IntelRuntime::spawn(
        crate::reasoning::backend(&state),
        Box::new(LibraryRetriever {
            app: app.clone(),
            project_id,
        }),
        RuntimeConfig {
            memory,
            focus,
            ..RuntimeConfig::default()
        },
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
                IntelUpdate::NothingNew
                | IntelUpdate::WrapSuggested(_)
                | IntelUpdate::SpeakerNames(_) => {}
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

/// Moves the live meeting to `project_id` (or out of any project): later passes retrieve from it
/// and read its knowledge and instructions, new screenshots are filed there, and Ask and the export
/// use it. Saving files the meeting under the project the webview passes then. Returns whether
/// intelligence was running.
#[tauri::command]
pub(crate) fn intel_set_project(
    app: AppHandle,
    project_id: Option<String>,
) -> Result<bool, String> {
    let project_id = project_id.filter(|p| !p.is_empty());
    let state = app.state::<AppState>();
    if let Ok(mut slot) = state.intel.project.lock() {
        slot.clone_from(&project_id);
    }
    crate::context::set_project(&app, project_id.clone());
    let memory = project_memory(&state, project_id.as_deref());
    let focus = about_you(&state, project_id.as_deref());
    let retriever = Box::new(LibraryRetriever {
        app: app.clone(),
        project_id,
    });
    with_runtime(&state, |rt| {
        rt.set_project(ProjectContext {
            retriever,
            memory,
            focus,
        });
    })
}

/// The speaker label a line carries into the reasoning context: the name the user gave the speaker
/// when set (see [`line_speaker`] for the mic rule), else You / Them / Speaker N. A rename applies to
/// lines from then on, and [`speaker_renamed`] relabels the other side's lines the runtime holds.
fn speaker_label(segment: &TranscriptSegment, names: &SpeakerNames) -> String {
    line_speaker(
        segment.source == AudioSourceKind::Microphone,
        segment.speaker,
        names,
    )
}

/// The names the user gave the live session's speakers so far.
pub(crate) fn live_speaker_names(state: &AppState) -> SpeakerNames {
    state
        .live_speaker_names
        .lock()
        .map(|n| n.clone())
        .unwrap_or_default()
}

/// Tells the running runtime that live speaker `speaker` went from `before` to `after` (the names
/// map around a rename), so lines it holds under the old label read as the new one.
pub(crate) fn speaker_renamed(
    state: &AppState,
    speaker: u32,
    before: &SpeakerNames,
    after: &SpeakerNames,
) {
    let (from, to) = rename_labels(speaker, before, after);
    let _ = with_runtime(state, |r| r.rename_speaker(from, to));
}

/// The label a diarized far-end speaker's lines carry before and after a rename.
fn rename_labels(speaker: u32, before: &SpeakerNames, after: &SpeakerNames) -> (String, String) {
    let id = SpeakerId(speaker);
    (speaker_display(id, before), speaker_display(id, after))
}

/// Records that the user dismissed suggesting `name` for `speaker` (a label like "Speaker 2").
/// `false` if intelligence isn't running.
#[tauri::command]
pub(crate) fn intel_dismiss_speaker_name(
    state: State<'_, AppState>,
    speaker: String,
    name: String,
) -> Result<bool, String> {
    with_runtime(&state, |r| r.dismiss_speaker_name(speaker, name))
}

/// Hands one admitted final to the running runtime, if any. Never blocks on a model.
pub(crate) fn route_final(app: &AppHandle, segment: &TranscriptSegment) {
    let state = app.state::<AppState>();
    let speaker = speaker_label(segment, &live_speaker_names(&state));
    let Ok(guard) = state.intel.runtime.lock() else {
        return;
    };
    if let Some(runtime) = guard.as_ref() {
        runtime.push_final(
            speaker,
            segment.start.as_millis() as i64,
            segment.text.clone(),
        );
    }
}

/// The live transcript as the runtime numbers it: admitted finals in arrival order, blank lines
/// dropped, so `T<n>` means the same line to Ask as to the state's evidence.
fn live_lines(retained: &[TranscriptSegment], names: &SpeakerNames) -> Vec<TranscriptLine> {
    retained
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .enumerate()
        .map(|(i, s)| TranscriptLine {
            idx: i as i64,
            start_ms: s.start.as_millis() as i64,
            speaker: speaker_label(s, names),
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

/// The longest Stop waits for the last pass over the meeting's final lines.
const FINAL_PASS_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Stops the runtime after one last pass over unanalyzed lines (bounded by [`FINAL_PASS_LIMIT`])
/// and parks its result for `save_note`.
pub(crate) fn stop(state: &AppState) {
    let runtime = state.intel.runtime.lock().ok().and_then(|mut r| r.take());
    if let Some(runtime) = runtime {
        let finished = runtime.finish(FINAL_PASS_LIMIT);
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

/// The live transcript so far, numbered as the runtime numbers it.
pub(crate) fn recent_lines(state: &AppState) -> Vec<TranscriptLine> {
    let names = live_speaker_names(state);
    state
        .live_segments
        .lock()
        .map(|s| live_lines(&s, &names))
        .unwrap_or_default()
}

/// Runs `f` on the live runtime. `false` if intelligence isn't running.
pub(crate) fn with_runtime(
    state: &AppState,
    f: impl FnOnce(&IntelRuntime),
) -> Result<bool, String> {
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
        let names = live_speaker_names(&state);
        let transcript = live_lines(
            &state
                .live_segments
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())?,
            &names,
        );
        let meeting = current_state(&state);
        let project_id = state.intel.project.lock().ok().and_then(|p| p.clone());
        let memory = project_memory(&state, project_id.as_deref());
        let retrieved = LibraryRetriever {
            app: app.clone(),
            project_id,
        }
        .retrieve(&question);
        let backend = crate::reasoning::backend(&state);
        // Screenshots are shown only when retrieval picked them for this question.
        let images = if backend.capabilities().vision {
            state.intel.context.images()
        } else {
            Vec::new()
        };
        let result = ask(
            backend.as_ref(),
            &cancel,
            &AskInput {
                question: &question,
                history: &history,
                transcript: &transcript,
                state: &meeting,
                retrieved: &retrieved,
                memory: &memory,
                images: &images,
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

/// A saved meeting's state log, parsed.
pub(crate) fn stored_log(library: &Library, id: &str) -> Result<Vec<AppliedOp>, String> {
    library
        .state_ops(id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| {
            Ok(AppliedOp {
                seq: s.seq as u64,
                at_ms: s.at_ms,
                op: serde_json::from_str(&s.op).map_err(|e| e.to_string())?,
            })
        })
        .collect()
}

/// A saved meeting's state and its transcript lines (empty once pruned).
fn saved_meeting(
    state: &AppState,
    id: &str,
) -> Result<(MeetingState, Vec<TranscriptLine>), String> {
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let log = stored_log(&library, id)?;
    let meeting = MeetingState::replay(id, &log).map_err(|e| e.to_string())?;
    let names = library.speaker_names(id).map_err(|e| e.to_string())?;
    let lines = library
        .get_note(id)
        .map_err(|e| e.to_string())?
        .map(|(_, segments)| {
            segments
                .iter()
                .map(|s| TranscriptLine::from_segment_named(s, &names))
                .collect()
        })
        .unwrap_or_default();
    Ok((meeting, lines))
}

/// The live items of a saved meeting's state, rebuilt from its stored log. Empty if it has none.
#[tauri::command]
pub(crate) fn intel_saved_items(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<StateItem>, String> {
    let (meeting, _) = saved_meeting(&state, &id)?;
    Ok(meeting.live_items().into_iter().cloned().collect())
}

/// Every item of a saved meeting's state, superseded and withdrawn ones included, by kind then id.
/// Empty if it has none.
#[tauri::command]
pub(crate) fn intel_saved_state(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<StateItem>, String> {
    let (meeting, _) = saved_meeting(&state, &id)?;
    Ok(all_items(meeting))
}

/// Every item of a state, superseded and withdrawn ones included, by kind then id.
pub(crate) fn all_items(meeting: MeetingState) -> Vec<StateItem> {
    let mut items: Vec<StateItem> = meeting.items.into_values().collect();
    items.sort_by_key(|i| (i.kind, item_number(&i.id)));
    items
}

/// The number part of an item id, for ordering `REQ-2` before `REQ-10`.
fn item_number(id: &str) -> u32 {
    id.rsplit_once('-')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// A new summary, and whether it had to be made from the transcript (the meeting has no state).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SummaryDto {
    markdown: String,
    from_transcript: bool,
}

/// Summarizes a saved meeting in one reasoning call over its state (the end of its transcript
/// when it has none), and stores the result as its summary, replacing any earlier one. `when` is
/// the display date, formatted by the webview.
#[tauri::command]
pub(crate) async fn meeting_summarize(
    app: AppHandle,
    id: String,
    when: Option<String>,
) -> Result<SummaryDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (meeting, lines) = saved_meeting(&state, &id)?;
        if meeting.live_items().is_empty() && lines.is_empty() {
            return Err("nothing to summarize: this meeting has no state or transcript".to_owned());
        }
        let title = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .get_note(&id)
            .map_err(|e| e.to_string())?
            .map(|(note, _)| note.title)
            .unwrap_or_default();
        let meta = SummaryMeta {
            title,
            when: when.unwrap_or_default(),
            participants: participants(&lines),
        };
        let context = summary_context(&meta, &meeting, &lines);
        let backend = crate::reasoning::backend_for(&state, Some(id.clone()));
        let response = backend
            .invoke(
                &summary_request(&context, std::time::Duration::from_secs(180)),
                &CancelToken::new(),
            )
            .map_err(|e| e.to_string())?;
        let markdown = parse_summary(response.output)?.to_markdown();
        state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?
            .set_summary(&id, &markdown)
            .map_err(|e| e.to_string())?;
        Ok(SummaryDto {
            markdown,
            from_transcript: context.from_transcript,
        })
    })
    .await
    .map_err(|e| format!("summary task failed: {e}"))?
}

/// A review's follow-ups, and where they came from: `"model"`, or `"state"` when no model was
/// available (then `note` says why).
#[derive(Serialize)]
pub(crate) struct ReviewDto {
    followups: Vec<FollowUp>,
    source: &'static str,
    note: Option<String>,
}

/// Starts the post-call review of a saved meeting: proposes its follow-ups.
#[tauri::command]
pub(crate) async fn intel_review_start(app: AppHandle, id: String) -> Result<ReviewDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (meeting, lines) = saved_meeting(&state, &id)?;
        let about = about_you(&state, meeting_project(&state, &id).as_deref());
        let backend = crate::reasoning::backend_for(&state, Some(id.clone()));
        let (followups, source, note) = match generate_followups(
            backend.as_ref(),
            &CancelToken::new(),
            &meeting,
            &lines,
            about.as_deref(),
            std::time::Duration::from_secs(180),
        ) {
            Ok(list) => (list, "model", None),
            Err(e) => (fallback_followups(&meeting), "state", Some(e.to_string())),
        };
        if let Ok(mut slot) = state.intel.review.lock() {
            *slot = Some((id, followups.clone()));
        }
        Ok(ReviewDto {
            followups,
            source,
            note,
        })
    })
    .await
    .map_err(|e| format!("review task failed: {e}"))?
}

/// What a reply changed.
#[derive(Serialize)]
pub(crate) struct ReplyDto {
    followups: Vec<FollowUp>,
    understood: Vec<ReviewEdit>,
    /// `"local"` when the reply was read without a model.
    via: &'static str,
}

/// Applies a plain-words correction ("1 and 4 are mine. Drop 5.") to the review's follow-ups.
#[tauri::command]
pub(crate) async fn intel_review_reply(app: AppHandle, reply: String) -> Result<ReplyDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (meeting_id, followups) = state
            .intel
            .review
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?
            .clone()
            .ok_or("no review in progress")?;
        let (edits, via) = match parse_reply(&reply, followups.len()) {
            Some(edits) => (edits, "local"),
            None => (
                interpret_reply(
                    crate::reasoning::backend_for(&state, Some(meeting_id)).as_ref(),
                    &CancelToken::new(),
                    &followups,
                    &reply,
                    std::time::Duration::from_secs(120),
                )
                .map_err(|e| e.to_string())?,
                "model",
            ),
        };
        let mut guard = state
            .intel
            .review
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?;
        let (_, list) = guard.as_mut().ok_or("no review in progress")?;
        apply_edits(list, &edits);
        Ok(ReplyDto {
            followups: list.clone(),
            understood: edits,
            via,
        })
    })
    .await
    .map_err(|e| format!("review task failed: {e}"))?
}

/// Sets one follow-up's class directly (the list's chips).
#[tauri::command]
pub(crate) fn intel_review_set(
    state: State<'_, AppState>,
    n: usize,
    class: FollowUpClass,
) -> Result<Vec<FollowUp>, String> {
    let mut guard = state
        .intel
        .review
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    let (_, list) = guard.as_mut().ok_or("no review in progress")?;
    apply_edits(list, &[ReviewEdit { n, class }]);
    Ok(list.clone())
}

/// Applies the reviewed follow-ups to the saved meeting's state (appended to its log) and ends the
/// review. Returns how many state changes that made.
#[tauri::command]
pub(crate) fn intel_review_apply(state: State<'_, AppState>) -> Result<usize, String> {
    let (id, followups) = state
        .intel
        .review
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .clone()
        .ok_or("no review in progress")?;
    let mut library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let count = apply_review(&mut library, &id, &followups, now_ms())?;
    if let Ok(mut slot) = state.intel.review_project.lock() {
        *slot = followups
            .iter()
            .filter(|f| f.class == FollowUpClass::ProjectMemory)
            .cloned()
            .collect();
    }
    // Applied once: a second apply would add the same new items again.
    if let Ok(mut slot) = state.intel.review.lock() {
        *slot = None;
    }
    Ok(count)
}

/// [`intel_review_apply`] against a library, for tests.
fn apply_review(
    library: &mut Library,
    id: &str,
    followups: &[FollowUp],
    now: i64,
) -> Result<usize, String> {
    let mut log = stored_log(library, id)?;
    let meeting = MeetingState::replay(id, &log).map_err(|e| e.to_string())?;
    let ops = review_ops(&meeting, followups);
    let count = ops.len();
    let first = log.len() as u64;
    log.extend(ops.into_iter().enumerate().map(|(i, op)| AppliedOp {
        seq: first + i as u64,
        at_ms: now,
        op,
    }));
    MeetingState::replay(id, &log).map_err(|e| e.to_string())?;
    save_log(library, id, &log)?;
    Ok(count)
}

/// Stores a saved meeting's whole state log.
pub(crate) fn save_log(library: &mut Library, id: &str, log: &[AppliedOp]) -> Result<(), String> {
    let stored = log
        .iter()
        .map(|a| {
            Ok(StoredOp {
                seq: a.seq as i64,
                at_ms: a.at_ms,
                op: serde_json::to_string(&a.op).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    library
        .save_state_ops(id, &stored)
        .map_err(|e| e.to_string())
}

/// A project's accepted memory; empty with no project or on error.
fn project_memory(state: &AppState, project_id: Option<&str>) -> Vec<MemoryEntry> {
    let Some(project) = project_id else {
        return Vec::new();
    };
    state
        .library
        .lock()
        .ok()
        .and_then(|l| l.list_memory(project).ok())
        .unwrap_or_default()
}

/// What matters to the user in `project_id` (see [`wisp_intel::about_you`]): "About me" from
/// Settings plus the project's name and instructions, read fresh each time.
fn about_you(state: &AppState, project_id: Option<&str>) -> Option<String> {
    let about_me = state.reasoning.about_me();
    let (name, instructions) = project_id
        .and_then(|id| {
            let library = state.library.lock().ok()?;
            let instructions = library.project_instructions(id).ok().flatten()?;
            Some((project_name(&library, Some(id)), instructions))
        })
        .unwrap_or_default();
    wisp_intel::about_you(&about_me, name.as_deref(), &instructions)
}

/// The project a saved meeting belongs to, if any.
fn meeting_project(state: &AppState, id: &str) -> Option<String> {
    let library = state.library.lock().ok()?;
    library.get_note(id).ok()??.0.project_id
}

/// Every project, for the project selector.
#[tauri::command]
pub(crate) fn list_projects(state: State<'_, AppState>) -> Result<Vec<Project>, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .list_projects()
        .map_err(|e| e.to_string())
}

/// Creates a project named `name` (trimmed, unique).
#[tauri::command]
pub(crate) fn create_project(state: State<'_, AppState>, name: String) -> Result<Project, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a project needs a name".to_owned());
    }
    let now = now_ms();
    let id = format!("p-{now:x}");
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .create_project(&id, name, now)
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                format!("a project named \"{name}\" already exists")
            } else {
                e.to_string()
            }
        })
}

/// Renames a project (trimmed, unique).
#[tauri::command]
pub(crate) fn rename_project(
    state: State<'_, AppState>,
    id: String,
    name: String,
) -> Result<bool, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a project needs a name".to_owned());
    }
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .rename_project(&id, name)
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                format!("a project named \"{name}\" already exists")
            } else {
                e.to_string()
            }
        })
}

/// A project's instructions: what matters to the user there. Empty when none were written.
#[tauri::command]
pub(crate) fn get_project_instructions(
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .project_instructions(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no such project".to_owned())
}

/// Replaces a project's instructions (trimmed; empty clears them). Used from the next meeting on.
#[tauri::command]
pub(crate) fn set_project_instructions(
    state: State<'_, AppState>,
    id: String,
    instructions: String,
) -> Result<bool, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .set_project_instructions(&id, &instructions)
        .map_err(|e| e.to_string())
}

/// Retitles a saved meeting. With `if_title`, only while its title is still that (for automatic
/// titles, which must not replace one the user typed meanwhile).
#[tauri::command]
pub(crate) fn rename_note(
    state: State<'_, AppState>,
    id: String,
    title: String,
    if_title: Option<String>,
) -> Result<bool, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a meeting needs a title".to_owned());
    }
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    match if_title {
        Some(expected) => library.rename_meeting_if(&id, title, &expected),
        None => library.rename_meeting(&id, title),
    }
    .map_err(|e| e.to_string())
}

/// Files a saved meeting under `project_id`, or under no project when it's empty or absent. Project
/// knowledge already learned from the meeting stays where it was learned.
#[tauri::command]
pub(crate) fn set_note_project(
    state: State<'_, AppState>,
    id: String,
    project_id: Option<String>,
) -> Result<bool, String> {
    let project = project_id.as_deref().filter(|p| !p.is_empty());
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .set_meeting_project(&id, project)
        .map_err(|e| {
            if e.to_string().contains("FOREIGN KEY") {
                "that project no longer exists".to_owned()
            } else {
                e.to_string()
            }
        })
}

/// How `project_id`'s knowledge relates to the saved meeting `id`: entries learned only from it
/// (which can move with it) and entries that also cite other evidence (which stay).
#[tauri::command]
pub(crate) fn note_knowledge(
    state: State<'_, AppState>,
    id: String,
    project_id: String,
) -> Result<MeetingKnowledge, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .meeting_knowledge(&id, &project_id)
        .map_err(|e| e.to_string())
}

/// Moves the knowledge learned only from meeting `id` from one project to another. Returns how
/// many entries moved.
#[tauri::command]
pub(crate) fn move_note_knowledge(
    state: State<'_, AppState>,
    id: String,
    from_project: String,
    to_project: String,
) -> Result<usize, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .move_meeting_knowledge(&id, &from_project, &to_project)
        .map_err(|e| e.to_string())
}

/// A memory entry with whether each piece of its evidence still exists.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryView {
    #[serde(flatten)]
    entry: MemoryEntry,
    /// Per provenance ref, in order: `true` once retention deleted the raw source.
    expired: Vec<bool>,
}

/// A project's accepted knowledge, with which sources have expired.
#[tauri::command]
pub(crate) fn list_project_memory(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Vec<MemoryView>, String> {
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let entries = library
        .list_memory(&project_id)
        .map_err(|e| e.to_string())?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let expired = entry
                .provenance
                .iter()
                .map(|p| !matches!(library.snippet_for_ref(&p.source_ref), Ok(Some(_))))
                .collect();
            MemoryView { entry, expired }
        })
        .collect())
}

/// Deletes one piece of project knowledge.
#[tauri::command]
pub(crate) fn delete_project_memory(state: State<'_, AppState>, id: i64) -> Result<bool, String> {
    state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .delete_memory(id)
        .map_err(|e| e.to_string())
}

/// Proposes what the saved meeting's project should remember. Errors if the meeting isn't in a
/// project.
#[tauri::command]
pub(crate) async fn intel_learning_propose(
    app: AppHandle,
    id: String,
) -> Result<Vec<Proposal>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (note, project) = {
            let library = state
                .library
                .lock()
                .map_err(|_| "library lock poisoned".to_owned())?;
            let (note, _) = library
                .get_note(&id)
                .map_err(|e| e.to_string())?
                .ok_or("no such meeting")?;
            let project = note
                .project_id
                .clone()
                .ok_or("this meeting isn't in a project")?;
            (note, project)
        };
        let (meeting, lines) = saved_meeting(&state, &id)?;
        let memory = project_memory(&state, Some(&project));
        let about = about_you(&state, Some(&project));
        let followups = state
            .intel
            .review_project
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default();
        propose_learning(
            crate::reasoning::backend_for(&state, Some(id.clone())).as_ref(),
            &CancelToken::new(),
            &LearningInput {
                state: &meeting,
                transcript: &lines,
                memory: &memory,
                followups: &followups,
                meeting_label: &note.title,
                about: about.as_deref(),
                timeout: std::time::Duration::from_secs(180),
            },
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("learning task failed: {e}"))?
}

/// Stores the accepted proposals in the saved meeting's project. Returns how many were stored.
#[tauri::command]
pub(crate) fn intel_learning_save(
    state: State<'_, AppState>,
    id: String,
    proposals: Vec<Proposal>,
) -> Result<usize, String> {
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let project = library
        .get_note(&id)
        .map_err(|e| e.to_string())?
        .and_then(|(note, _)| note.project_id)
        .ok_or("this meeting isn't in a project")?;
    save_learning(&library, &project, &id, &proposals, now_ms())
}

/// Stores accepted, non-blank proposals.
fn save_learning(
    library: &Library,
    project: &str,
    meeting_id: &str,
    proposals: &[Proposal],
    now: i64,
) -> Result<usize, String> {
    let mut stored = 0;
    for p in proposals
        .iter()
        .filter(|p| p.accepted && !p.text.trim().is_empty())
    {
        library
            .add_memory(project, &p.to_memory(meeting_id), now)
            .map_err(|e| e.to_string())?;
        stored += 1;
    }
    Ok(stored)
}

/// Everything an export is made from.
struct ExportSource {
    meeting_id: String,
    meta: ExportMeta,
    state: MeetingState,
    log: Vec<AppliedOp>,
    lines: Vec<TranscriptLine>,
    memory: Vec<MemoryEntry>,
}

/// Longest evidence quote in an export, in characters.
const QUOTE_CHARS: usize = 400;

fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

impl ExportSource {
    /// Quotes a canonical ref: this meeting's lines and project memory from what is loaded, other
    /// refs through `lookup` (the library). `None` once the text is gone.
    fn quote(&self, r: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
        let own = format!("M{}:T", self.meeting_id);
        if let Some(idx) = r.strip_prefix(&own).and_then(|n| n.parse::<i64>().ok()) {
            return self.lines.iter().find(|l| l.idx == idx).map(|l| {
                let s = l.start_ms.max(0) / 1000;
                format!(
                    "{:02}:{:02} {}: {}",
                    s / 60,
                    s % 60,
                    l.speaker,
                    clip(&l.text)
                )
            });
        }
        if let Some(entry) = self.memory.iter().find(|m| memory_ref(m.id) == r) {
            return Some(format!("Project knowledge: {}", clip(&entry.text)));
        }
        lookup(r).map(|t| clip(&t))
    }

    /// `kind` is `record` (Markdown), `packet` (the AI context packet) or `json`.
    fn render(
        &self,
        kind: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<String, String> {
        match kind {
            "record" => Ok(meeting_record(&self.meta, &self.state)),
            "packet" => Ok(context_packet(
                &self.meta,
                &self.state,
                &self.memory,
                &|r: &str| self.quote(r, &lookup),
            )),
            "json" => serde_json::to_string_pretty(&state_json(&self.meta, &self.state, &self.log))
                .map_err(|e| e.to_string()),
            other => Err(format!("unknown export kind: {other}")),
        }
    }
}

fn project_name(library: &Library, id: Option<&str>) -> Option<String> {
    let id = id?;
    let name = library
        .list_projects()
        .ok()?
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.name);
    Some(name.unwrap_or_else(|| id.to_owned()))
}

/// The live (or just-stopped, not yet saved) meeting when `id` is `None`, else the saved meeting.
fn export_source(
    state: &AppState,
    id: Option<&str>,
    title: Option<String>,
    when: Option<String>,
) -> Result<ExportSource, String> {
    let title = title.filter(|t| !t.trim().is_empty());
    let when = when.unwrap_or_default();
    let Some(id) = id else {
        let running = state
            .intel
            .runtime
            .lock()
            .ok()
            .and_then(|r| r.as_ref().map(|r| r.snapshot_with_log()));
        let (meeting, log) = match running {
            Some(pair) => pair,
            None => state
                .intel
                .finished
                .lock()
                .ok()
                .and_then(|f| f.as_ref().map(|f| (f.state.clone(), f.log.clone())))
                .ok_or("no meeting intelligence to export")?,
        };
        let names = live_speaker_names(state);
        let lines = live_lines(
            &state
                .live_segments
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())?,
            &names,
        );
        let project = state.intel.project.lock().ok().and_then(|p| p.clone());
        let memory = project_memory(state, project.as_deref());
        let project = state
            .library
            .lock()
            .ok()
            .and_then(|l| project_name(&l, project.as_deref()));
        return Ok(ExportSource {
            meeting_id: LIVE_MEETING_ID.to_owned(),
            meta: ExportMeta {
                title: title.unwrap_or_else(|| "Current meeting".to_owned()),
                when,
                project,
                focus: None,
                summary: None,
            },
            state: meeting,
            log,
            lines,
            memory,
        });
    };
    let (note, log, lines, project) = {
        let library = state
            .library
            .lock()
            .map_err(|_| "library lock poisoned".to_owned())?;
        let (note, segments) = library
            .get_note(id)
            .map_err(|e| e.to_string())?
            .ok_or("no such meeting")?;
        let log = stored_log(&library, id)?;
        let names = library.speaker_names(id).map_err(|e| e.to_string())?;
        let lines = segments
            .iter()
            .map(|s| TranscriptLine::from_segment_named(s, &names))
            .collect();
        let project = project_name(&library, note.project_id.as_deref());
        (note, log, lines, project)
    };
    let meeting = MeetingState::replay(id, &log).map_err(|e| e.to_string())?;
    let memory = project_memory(state, note.project_id.as_deref());
    Ok(ExportSource {
        meeting_id: id.to_owned(),
        meta: ExportMeta {
            title: title.unwrap_or(note.title),
            when,
            project,
            focus: None,
            summary: note.summary,
        },
        state: meeting,
        log,
        lines,
        memory,
    })
}

/// A stored segment as a transcript segment, for the transcript formatters.
fn stored_segment(s: &wisp_library::Segment) -> TranscriptSegment {
    let ms = |v: i64| std::time::Duration::from_millis(v.max(0) as u64);
    TranscriptSegment {
        id: s.idx.max(0) as u64,
        text: s.text.clone(),
        start: ms(s.start_ms),
        end: ms(s.end_ms),
        status: wisp_core::transcript::SegmentStatus::Final,
        source: match s.source.as_str() {
            "mic" => AudioSourceKind::Microphone,
            "system" => AudioSourceKind::System,
            _ => AudioSourceKind::File,
        },
        speaker: s.speaker.and_then(|n| u32::try_from(n).ok()).map(SpeakerId),
        confidence: None,
        words: Vec::new(),
        aux_text: None,
    }
}

/// The transcript as Markdown: the live (or just-stopped) meeting's when `id` is `None`, else the
/// saved meeting's, led by its summary.
fn render_transcript(
    state: &AppState,
    id: Option<&str>,
    title: Option<String>,
    when: Option<String>,
) -> Result<String, String> {
    let title = title.filter(|t| !t.trim().is_empty());
    let (mut segments, names, meta) = match id {
        None => {
            let segments = state
                .live_segments
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())?
                .clone();
            let meta = wisp_core::export::MeetingMeta {
                title: Some(title.unwrap_or_else(|| "Current meeting".to_owned())),
                date: when,
                ..Default::default()
            };
            (segments, live_speaker_names(state), meta)
        }
        Some(id) => {
            let library = state
                .library
                .lock()
                .map_err(|_| "library lock poisoned".to_owned())?;
            let (note, stored) = library
                .get_note(id)
                .map_err(|e| e.to_string())?
                .ok_or("no such meeting")?;
            let names = library.speaker_names(id).map_err(|e| e.to_string())?;
            let meta = wisp_core::export::MeetingMeta {
                title: Some(title.unwrap_or(note.title)),
                date: when,
                engine: note.engine,
                language: note.language,
                summary: note.summary,
            };
            (stored.iter().map(stored_segment).collect(), names, meta)
        }
    };
    if segments.is_empty() {
        return Err("this meeting has no transcript".to_owned());
    }
    segments.sort_by_key(|s| s.start);
    Ok(wisp_core::export::format_markdown_named(
        &segments, &meta, &names,
    ))
}

/// The saved meeting's summary as a document. The live meeting has none until it is saved.
fn render_summary(
    state: &AppState,
    id: Option<&str>,
    title: Option<String>,
    when: Option<String>,
) -> Result<String, String> {
    let id = id.ok_or("the meeting has no summary yet")?;
    let note = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?
        .get_note(id)
        .map_err(|e| e.to_string())?
        .ok_or("no such meeting")?
        .0;
    let summary = note
        .summary
        .filter(|s| !s.trim().is_empty())
        .ok_or("the meeting has no summary yet")?;
    let title = title.filter(|t| !t.trim().is_empty()).unwrap_or(note.title);
    let mut out = format!("# {title}\n\n");
    if let Some(when) = when.filter(|w| !w.trim().is_empty()) {
        out.push_str(&format!("_{when}_\n\n"));
    }
    out.push_str(summary.trim());
    out.push('\n');
    Ok(out)
}

/// `kind` is `summary`, `record`, `transcript`, `packet` or `json`.
fn render_export(
    state: &AppState,
    id: Option<&str>,
    kind: &str,
    title: Option<String>,
    when: Option<String>,
) -> Result<String, String> {
    match kind {
        "summary" => return render_summary(state, id, title, when),
        "transcript" => return render_transcript(state, id, title, when),
        _ => {}
    }
    let source = export_source(state, id, title, when)?;
    source.render(kind, |r| {
        state
            .library
            .lock()
            .ok()?
            .snippet_for_ref(r)
            .ok()
            .flatten()
            .map(|s| s.text)
    })
}

/// A meeting as a document: `kind` is `summary`, `record`, `transcript`, `packet` or `json`. `id`
/// `None` means the live (or just-stopped) meeting. `when` is the display date, formatted by the webview.
#[tauri::command]
pub(crate) fn intel_export(
    state: State<'_, AppState>,
    id: Option<String>,
    kind: String,
    title: Option<String>,
    when: Option<String>,
) -> Result<String, String> {
    render_export(&state, id.as_deref(), &kind, title, when)
}

/// Saves [`intel_export`]'s document to a file the user picks. The backend builds the content and
/// shows the dialog, so the webview never names a destination. Returns `false` on cancel.
#[tauri::command]
pub(crate) async fn intel_export_save(
    app: AppHandle,
    id: Option<String>,
    kind: String,
    title: Option<String>,
    when: Option<String>,
    default_name: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let content = render_export(&app.state::<AppState>(), id.as_deref(), &kind, title, when)?;
        let ext = if kind == "json" { "json" } else { "md" };
        let Some(picked) = app
            .dialog()
            .file()
            .set_file_name(format!("{default_name}.{ext}"))
            .add_filter(ext.to_uppercase(), &[ext])
            .blocking_save_file()
        else {
            return Ok(false);
        };
        let dest = picked.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
        Ok(true)
    })
    .await
    .map_err(|e| format!("export task failed: {e}"))?
}

/// The project brief (see [`wisp_intel::project_brief`]) as Markdown. `offset_minutes` is the
/// webview's UTC offset (east positive), so dates read as the user's calendar.
fn render_brief(state: &AppState, project_id: &str, offset_minutes: i32) -> Result<String, String> {
    let library = state
        .library
        .lock()
        .map_err(|_| "library lock poisoned".to_owned())?;
    let instructions = library
        .project_instructions(project_id)
        .map_err(|e| e.to_string())?
        .ok_or("no such project")?;
    let project = project_name(&library, Some(project_id)).unwrap_or_default();
    let memory = library.list_memory(project_id).map_err(|e| e.to_string())?;
    let manual = library
        .list_project_items(project_id)
        .map_err(|e| e.to_string())?;
    let meetings = brief_meetings(&library, project_id, offset_minutes)?;
    let now = now_ms();
    Ok(project_brief(&BriefInput {
        project: &project,
        date: &iso_date(now, offset_minutes),
        now_ms: now,
        instructions: &instructions,
        memory: &memory,
        meetings: &meetings,
        manual: &manual,
    }))
}

/// A project's saved meetings with their states replayed, for the brief and the overview.
pub(crate) fn brief_meetings(
    library: &Library,
    project_id: &str,
    offset_minutes: i32,
) -> Result<Vec<BriefMeeting>, String> {
    let mut summaries = library
        .project_summaries(project_id)
        .map_err(|e| e.to_string())?;
    Ok(library
        .list_notes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|n| n.project_id.as_deref() == Some(project_id))
        .map(|n| {
            // A meeting whose log won't replay still counts; it just has no items.
            let log = stored_log(library, &n.id).unwrap_or_default();
            let state =
                MeetingState::replay(&n.id, &log).unwrap_or_else(|_| MeetingState::new(&n.id));
            BriefMeeting {
                when: iso_date(n.started_at_ms, offset_minutes),
                original_text: wisp_intel::original_texts(&n.id, &log),
                summary: summaries.remove(&n.id),
                id: n.id,
                title: n.title,
                started_at_ms: n.started_at_ms,
                state,
            }
        })
        .collect())
}

/// A project's brief, for the preview.
/// Off the main thread: the brief replays every meeting in the project.
#[tauri::command]
pub(crate) async fn project_brief_markdown(
    app: AppHandle,
    project_id: String,
    offset_minutes: i32,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        render_brief(&app.state::<AppState>(), &project_id, offset_minutes)
    })
    .await
    .map_err(|e| format!("brief task failed: {e}"))?
}

/// Saves the project brief to a Markdown file the user picks, like [`intel_export_save`]. Returns
/// `false` on cancel.
#[tauri::command]
pub(crate) async fn project_brief_save(
    app: AppHandle,
    project_id: String,
    offset_minutes: i32,
    default_name: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let content = render_brief(&app.state::<AppState>(), &project_id, offset_minutes)?;
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
        std::fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
        Ok(true)
    })
    .await
    .map_err(|e| format!("brief task failed: {e}"))?
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
    fn a_review_is_appended_to_the_saved_log() {
        let mut library = Library::open_in_memory().unwrap();
        let add = AppliedOp {
            seq: 0,
            at_ms: 1,
            op: ResolvedOp::Add {
                id: "COM-1".into(),
                kind: ItemKind::Commitment,
                text: "Send traffic numbers".into(),
                status: EpistemicStatus::Stated,
                confidence: 0.8,
                source_refs: vec!["Mm:T0".into()],
                related_items: vec![],
                owner: None,
                due: None,
            },
        };
        library
            .save_state_ops(
                "m",
                &[StoredOp {
                    seq: 0,
                    at_ms: 1,
                    op: serde_json::to_string(&add.op).unwrap(),
                }],
            )
            .unwrap();
        let followups = vec![
            FollowUp {
                n: 1,
                text: "Send traffic numbers".into(),
                class: FollowUpClass::Theirs,
                owner: None,
                due: None,
                source_refs: vec!["Mm:T0".into()],
                item_id: Some("COM-1".into()),
            },
            FollowUp {
                n: 2,
                text: "Is retention 90 days?".into(),
                class: FollowUpClass::OpenQuestion,
                owner: None,
                due: None,
                source_refs: vec!["Mm:T0".into()],
                item_id: None,
            },
        ];
        assert_eq!(apply_review(&mut library, "m", &followups, 99).unwrap(), 2);
        let log = stored_log(&library, "m").unwrap();
        assert_eq!(log.iter().map(|a| a.seq).collect::<Vec<_>>(), [0, 1, 2]);
        let meeting = MeetingState::replay("m", &log).unwrap();
        assert_eq!(
            meeting.item("COM-1").unwrap().owner.as_deref(),
            Some("Them")
        );
        assert_eq!(meeting.item("Q-1").unwrap().created_at_ms, 99);
    }

    #[test]
    fn only_accepted_proposals_are_saved_with_their_provenance() {
        let library = Library::open_in_memory().unwrap();
        library.create_project("p", "Acme", 0).unwrap();
        let p = |text: &str, accepted: bool| Proposal {
            kind: "requirement".into(),
            text: text.into(),
            status: "inferred".into(),
            confidence: 0.8,
            provenance: vec![wisp_library::ProvenanceRef {
                source_ref: "Mm:T0".into(),
                label: "Note · Sept 26 00:00, Them".into(),
                sha256: "0".repeat(64),
            }],
            accepted,
        };
        let n = save_learning(
            &library,
            "p",
            "m",
            &[p("Azure only", true), p("Dropped", false), p("  ", true)],
            5,
        )
        .unwrap();
        assert_eq!(n, 1);
        let memory = library.list_memory("p").unwrap();
        assert_eq!(memory[0].text, "Azure only");
        assert_eq!(memory[0].meeting_id.as_deref(), Some("m"));
        assert_eq!(memory[0].provenance[0].label, "Note · Sept 26 00:00, Them");
    }

    #[test]
    fn live_lines_number_admitted_finals_like_the_runtime() {
        let retained = vec![
            seg(AudioSourceKind::System, 5000, "We host in Azure."),
            seg(AudioSourceKind::System, 5500, "  "),
            seg(AudioSourceKind::Microphone, 3000, "Where do you host?"),
        ];
        let lines = live_lines(&retained, &SpeakerNames::new());
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[1].idx, lines[1].start_ms), (1, 3000));
        assert_eq!(lines[1].speaker, "You");
        assert_eq!(lines[0].speaker, "Them");
    }

    #[test]
    fn a_rename_relabels_from_the_old_label_to_the_new() {
        let none = SpeakerNames::new();
        let named: SpeakerNames = [(1, "Laurie".to_owned())].into_iter().collect();
        assert_eq!(
            rename_labels(1, &none, &named),
            ("Speaker 2".to_owned(), "Laurie".to_owned())
        );
        assert_eq!(
            rename_labels(1, &named, &none),
            ("Laurie".to_owned(), "Speaker 2".to_owned())
        );
    }

    #[test]
    fn speaker_names_reach_the_webview_as_their_own_kind() {
        let dto = IntelUpdateDto::from(IntelUpdate::SpeakerNames(vec![SpeakerSuggestion {
            speaker: "Speaker 2".into(),
            speaker_id: 1,
            name: "Laurie".into(),
            confidence: 0.8,
            evidence: vec!["T0".into()],
            quote: Some("Speaker 1: Good afternoon Laurie".into()),
        }]));
        let v = serde_json::to_value(dto).unwrap();
        assert_eq!(v["kind"], "speakerNames");
        assert_eq!(v["suggestions"][0]["speakerId"], 1);
        assert_eq!(v["suggestions"][0]["name"], "Laurie");
    }

    #[test]
    fn speaker_labels_follow_the_source_and_diarization() {
        let none = SpeakerNames::new();
        let mut s = seg(AudioSourceKind::Microphone, 0, "x");
        assert_eq!(speaker_label(&s, &none), "You");
        s.source = AudioSourceKind::System;
        assert_eq!(speaker_label(&s, &none), "Them");
        s.speaker = Some(SpeakerId(1));
        assert_eq!(speaker_label(&s, &none), "Speaker 2");
        let names: SpeakerNames = [(1, "Bob".to_owned())].into_iter().collect();
        assert_eq!(speaker_label(&s, &names), "Bob");
        // A diarized, named mic speaker (a shared room mic) reads as its name; unnamed stays "You".
        s.source = AudioSourceKind::Microphone;
        assert_eq!(speaker_label(&s, &names), "Bob");
        assert_eq!(speaker_label(&s, &none), "You");
    }

    #[test]
    fn exports_quote_own_lines_and_memory_and_ask_the_library_for_the_rest() {
        let log = vec![AppliedOp {
            seq: 0,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: "R1".into(),
                kind: ItemKind::Requirement,
                text: "Must run in Azure".into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                source_refs: vec![
                    "Mlive:T0".into(),
                    "P7".into(),
                    "S3:C1".into(),
                    "S9:C9".into(),
                ],
                related_items: vec![],
                owner: None,
                due: None,
            },
        }];
        let source = ExportSource {
            meeting_id: LIVE_MEETING_ID.to_owned(),
            meta: ExportMeta {
                title: "Kickoff".into(),
                ..ExportMeta::default()
            },
            state: MeetingState::replay(LIVE_MEETING_ID, &log).unwrap(),
            log,
            lines: live_lines(
                &[seg(AudioSourceKind::System, 65_000, "It has to be Azure.")],
                &SpeakerNames::new(),
            ),
            memory: vec![MemoryEntry {
                id: 7,
                project_id: "p".into(),
                kind: "constraint".into(),
                text: "Customer is on Azure".into(),
                status: "stated".into(),
                confidence: 1.0,
                provenance: vec![],
                meeting_id: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            }],
        };
        let lookup = |r: &str| (r == "S3:C1").then(|| "Tenant: contoso".to_owned());
        let packet = source.render("packet", lookup).unwrap();
        assert!(
            packet.contains("01:05 Them: It has to be Azure."),
            "{packet}"
        );
        assert!(packet.contains("Project knowledge: Customer is on Azure"));
        assert!(packet.contains("Tenant: contoso"));
        assert!(
            packet.contains("1 source(s) no longer available"),
            "S9:C9 is gone"
        );

        assert!(source
            .render("record", lookup)
            .unwrap()
            .contains("Must run in Azure"));
        let json: serde_json::Value =
            serde_json::from_str(&source.render("json", lookup).unwrap()).unwrap();
        assert_eq!(json["log"].as_array().unwrap().len(), 1);
        assert!(source.render("pdf", lookup).is_err());
        assert_eq!(
            clip(&"x".repeat(QUOTE_CHARS + 5)).chars().count(),
            QUOTE_CHARS + 1
        );
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
                headline: "Hosting unclear".into(),
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

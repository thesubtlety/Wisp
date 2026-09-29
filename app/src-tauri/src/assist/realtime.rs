//! The realtime assist: an OpenAI realtime model that hears the same live audio as transcription
//! (the post-AEC mic + system streams, mixed to one mono stream) and gets each committed,
//! speaker-attributed final as authoritative text alongside it. A worker thread paces its replies
//! (throttled, or on demand) and streams them to the UI over the `assist://` events.
//!
//! The live session drives it through four hooks: [`AssistTaps`] tees each processed stream,
//! [`store_assist_mix`] parks the mix for [`start_assist_realtime`], [`route_assist_final`]
//! forwards each final while a worker runs, and [`take_assist_teardown`] lifts everything out on
//! Stop.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager, State};

use wisp_audio::{tee, ChannelSource, MeetingMixer, Resampler, Tee};
use wisp_core::audio::{AudioFrame, AudioSource, AudioSourceInfo};
use wisp_core::channel::FrameReceiver;
use wisp_core::engine::StreamingAsrEngine;
use wisp_core::transcript::{AudioSourceKind, TranscriptSegment};
use wisp_engine_cloud::{assist_realtime_param_specs, build_assist_engine, REALTIME_URL_ENV};
use wisp_reasoning::{is_loopback, now_ms, AuditRecord, AuditSink, CallInfo};

use super::{ASSIST_DELTA_EVENT, ASSIST_ERROR_EVENT, ASSIST_TEXT_EVENT};
use crate::{build_param_values, cloud_key, param_spec_dto, AppState, ParamSpecDto};

/// The realtime assist's share of [`AppState`]: its live audio taps and mix, its worker, and the
/// channel the live sink feeds finals into. Empty between live sessions.
#[derive(Default)]
pub(crate) struct AssistState {
    /// Fan-out tees feeding the realtime AI assist its copy of each transcription stream — kept alive
    /// for the session's duration (dropping them closes the assist branches).
    tees: Mutex<Vec<Tee>>,
    /// The mixed live audio (mic + system) for the realtime assist, available while a live session
    /// runs; the assist's realtime engine consumes it when started. `None` between sessions.
    audio: Mutex<Option<Box<dyn AudioSource>>>,
    /// The running realtime AI-assist worker (its stop flag + thread), or `None` when assist is idle.
    /// The worker owns the mix source while running and hands it back on stop, so assist can restart.
    worker: Mutex<Option<AssistWorker>>,
    /// While the realtime assist runs, the channel the live session pushes each diarized final into, so
    /// the assist injects authoritative, speaker-attributed text alongside the audio it hears. `None`
    /// when no assist is running (the live sink then skips the routing).
    finals_tx: Mutex<Option<std::sync::mpsc::Sender<String>>>,
}

/// The mono rate the assist mix runs at — OpenAI Realtime's native input, so the engine passes it
/// through without a second resample. Both tapped streams are converted to it before summing.
const ASSIST_MIX_RATE: u32 = 24_000;

/// The live audio for the realtime AI assist (option B): the same post-processed streams the
/// transcription gets (mic after AEC + system), summed into one mono [`ASSIST_MIX_RATE`] stream.
///
/// The two taps arrive at *different* rates (the AEC mic at 16 kHz, the raw system at the capture
/// rate, e.g. 48 kHz) and in differently-sized frames, so each is resampled to the common rate first;
/// the secondary is accumulated in a small jitter buffer and mixed sample-aligned (never decimated, no
/// rate-mismatched sum). `primary` drives the cadence (`recv`, blocking) — its drop-oldest tee means an
/// idle assist never stalls or backs up the capture.
struct MixSource {
    primary: FrameReceiver,
    primary_rs: Resampler,
    secondary: Option<FrameReceiver>,
    secondary_rs: Resampler,
    /// Resampled secondary samples waiting to be mixed (bounded; oldest dropped past the cap).
    secondary_buf: VecDeque<f32>,
    /// Soft-knee + depth-capped-ducking mixer (no clip distortion, both speakers stay intelligible).
    mixer: MeetingMixer,
    info: AudioSourceInfo,
}

impl AudioSource for MixSource {
    fn info(&self) -> AudioSourceInfo {
        self.info.clone()
    }

    fn next_frame(&mut self) -> wisp_core::error::Result<Option<AudioFrame>> {
        let Some(frame) = self.primary.recv() else {
            return Ok(None);
        };
        let timestamp = frame.timestamp;
        let mut mono = self.primary_rs.process(&frame);

        if let Some(sec) = &self.secondary {
            // Drain every buffered secondary frame (resampled to the mix rate) — never decimate it —
            // capping the jitter buffer so a slightly-faster stream can't grow it without bound.
            while let Some(other) = sec.try_recv() {
                let resampled = self.secondary_rs.process(&other);
                self.secondary_buf.extend(resampled);
            }
            let cap = ASSIST_MIX_RATE as usize / 2; // ~0.5 s
            while self.secondary_buf.len() > cap {
                self.secondary_buf.pop_front();
            }

            // Mix in time order, as many secondary samples as this frame holds — soft-limited so a loud
            // mic+system sum never clips, with light depth-capped ducking so the dominant talker stays
            // clear while the other side is never lost.
            let take = mono.len().min(self.secondary_buf.len());
            let secondary: Vec<f32> = self.secondary_buf.drain(..take).collect();
            self.mixer.mix(&mut mono, &secondary);
        }

        Ok(Some(AudioFrame::new(mono, ASSIST_MIX_RATE, 1, timestamp)))
    }
}

/// The realtime assist's taps on one live session's processed streams: the branch each
/// [`tap`](Self::tap) tees off, plus the tee handles that keep those branches alive. Filled while
/// `start_session` wires its sources, then handed to [`store_assist_mix`].
#[derive(Default)]
pub(crate) struct AssistTaps {
    tees: Vec<Tee>,
    branches: Vec<FrameReceiver>,
}

impl AssistTaps {
    /// Tees a processed transcription `source` so the assist can hear the same audio: returns the
    /// source for transcription (a [`ChannelSource`] over one branch) and stashes the other branch
    /// plus the tee handle. The tee is drop-oldest, so an unread assist branch never stalls or
    /// backs up the capture.
    ///
    /// When `want` is false (no real-time assist armed for this session) the source is returned
    /// untouched — no tee, no pump thread — so an ordinary live session's audio path is
    /// byte-for-byte the original.
    pub(crate) fn tap(&mut self, source: Box<dyn AudioSource>, want: bool) -> Box<dyn AudioSource> {
        if !want {
            return source;
        }

        let info = source.info();
        let (handle, main_rx, assist_rx) = tee(source);

        self.tees.push(handle);
        self.branches.push(assist_rx);
        Box::new(ChannelSource::new(main_rx, info))
    }
}

/// Combines the tapped live branches into one mono [`MixSource`] and parks it (plus the tee handles
/// that keep the taps alive) on [`AssistState`] for the realtime assist to pick up. With no
/// branches (no source started) it clears any stale mix instead.
pub(crate) fn store_assist_mix(state: &AssistState, taps: AssistTaps) -> Result<(), String> {
    let AssistTaps { tees, mut branches } = taps;

    let mix: Option<Box<dyn AudioSource>> = if branches.is_empty() {
        None
    } else {
        let primary = branches.remove(0);
        let secondary = if branches.is_empty() {
            None
        } else {
            Some(branches.remove(0))
        };
        let info = AudioSourceInfo {
            kind: AudioSourceKind::Microphone,
            name: "Assist mix".to_owned(),
        };

        Some(Box::new(MixSource {
            primary,
            primary_rs: Resampler::new(ASSIST_MIX_RATE),
            secondary,
            secondary_rs: Resampler::new(ASSIST_MIX_RATE),
            secondary_buf: VecDeque::new(),
            mixer: MeetingMixer::new(),
            info,
        }))
    };

    *state
        .audio
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = mix;
    *state
        .tees
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = tees;

    Ok(())
}

/// The realtime assist's live resources lifted out of [`AssistState`] by [`take_assist_teardown`],
/// for the Stop path to join off the command.
pub(crate) struct AssistTeardown {
    worker: Option<AssistWorker>,
    audio: Option<Box<dyn AudioSource>>,
    tees: Vec<Tee>,
}

impl AssistTeardown {
    /// Closes the assist taps, drops any parked mix, then joins the worker.
    pub(crate) fn shutdown(self) {
        let AssistTeardown {
            worker,
            audio,
            tees,
        } = self;

        // Close the assist taps + drop any parked mix BEFORE joining the worker: it blocks on its
        // mixed `recv` fed by these taps, so closing them lets it observe end-of-stream and exit
        // even if the capture device wedged — otherwise the join could hang on a recv nothing
        // satisfies.
        drop(tees);
        drop(audio);
        if let Some(worker) = worker {
            let _ = worker.stop_join();
        }
    }
}

/// Lifts the realtime assist's resources out of `state`, leaving it empty — fast and non-blocking
/// (quick lock + take/`None`; dropping the finals sender just closes its channel). The caller joins
/// them later via [`AssistTeardown::shutdown`].
pub(crate) fn take_assist_teardown(state: &AssistState) -> Result<AssistTeardown, String> {
    // Stop routing finals first so the sink doesn't push into a worker that's about to be joined.
    *state
        .finals_tx
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = None;
    let tees = std::mem::take(
        &mut *state
            .tees
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?,
    );
    let audio = state
        .audio
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .take();
    let worker = take_assist_worker(state)?;

    Ok(AssistTeardown {
        worker,
        audio,
        tees,
    })
}

/// The advanced parameter specs the **realtime** assist exposes (turn-detection endpointing + noise
/// reduction) — the realtime counterpart of [`assist_params`]. OpenAI-realtime-only, like the realtime
/// assist itself, so it takes no provider/model.
///
/// [`assist_params`]: super::chat::assist_params
#[tauri::command]
pub(crate) fn assist_realtime_params() -> Vec<ParamSpecDto> {
    assist_realtime_param_specs()
        .iter()
        .map(param_spec_dto)
        .collect()
}

/// How often the realtime assist may volunteer an ambient reply, when there's new speech since the
/// last one — so it rides along the conversation without firing on every utterance (noisy + costly).
const ASSIST_THROTTLE: std::time::Duration = std::time::Duration::from_secs(15);

/// If a triggered reply never completes within this long (stalled stream), stop waiting on it so the
/// cadence resumes — a safety valve, not the normal path (replies finish in a second or two).
const ASSIST_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A running realtime AI-assist worker: the stop flag its loop polls, a one-shot flag the UI sets to
/// pull a reply on demand, and the thread handle — which *returns the mix source* on join, so the
/// assist can be stopped and restarted within one live session without re-tapping the capture.
struct AssistWorker {
    stop: Arc<std::sync::atomic::AtomicBool>,
    hint: Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<Box<dyn AudioSource>>,
}

impl AssistWorker {
    /// Signals the loop to stop, joins the thread, and hands back the mix source it owned — `None`
    /// only if the thread panicked (then the source is gone with it).
    fn stop_join(self) -> Option<Box<dyn AudioSource>> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle.join().ok()
    }
}

/// One line of authoritative, speaker-attributed text for the realtime assist — the mic is "Me", the
/// system side is "Them" plus the live diarizer's 1-based speaker number when known. Mirrors the
/// frontend's on-screen attribution so the injected anchor reads the same way the user sees it.
fn assist_final_text(segment: &TranscriptSegment) -> String {
    let who = match segment.source {
        AudioSourceKind::Microphone => "Me".to_owned(),
        AudioSourceKind::System => match segment.speaker {
            Some(id) => format!("Them (Speaker {})", id.0 + 1),
            None => "Them".to_owned(),
        },
        _ => "Speaker".to_owned(),
    };

    format!("{who}: {}", segment.text)
}

/// Routes one diarized final into the running realtime assist, if any — best-effort (a closed channel
/// just means the assist stopped). Called from the live sink for every admitted final.
pub(crate) fn route_assist_final(app: &AppHandle, segment: &TranscriptSegment) {
    let state = app.state::<AppState>();
    let Ok(guard) = state.assist.finals_tx.lock() else {
        return;
    };

    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(assist_final_text(segment));
    }
}

/// Takes the running assist worker out of state, if any — the caller stops + joins it.
fn take_assist_worker(state: &AssistState) -> Result<Option<AssistWorker>, String> {
    Ok(state
        .worker
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .take())
}

/// The realtime assist loop — the hybrid, controlled-cadence engine room. Each iteration:
/// 1. injects any new diarized finals as authoritative text context (`engine.inject_text`),
/// 2. feeds one audio frame (prosody + low latency) and drains any in-flight reply,
/// 3. decides whether to trigger a reply now — on a manual pull, or throttled when there's new speech.
///
/// Replies fire on `response.create` (the session is configured `create_response:false`), so the
/// model answers on this cadence instead of every utterance. Exits when stopped or when the capture
/// closes (the live session ended), returning the source so the assist can restart.
///
/// When the loop ends, the session goes to the activity log as one call: the transcript lines
/// injected as context and every reply.
fn spawn_assist_worker(
    app: AppHandle,
    mut engine: Box<dyn StreamingAsrEngine>,
    mut source: Box<dyn AudioSource>,
    finals_rx: std::sync::mpsc::Receiver<String>,
    audit: (AuditSink, CallInfo),
) -> AssistWorker {
    use std::sync::atomic::{AtomicBool, Ordering};

    let stop = Arc::new(AtomicBool::new(false));
    let hint = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let hint_worker = Arc::clone(&hint);

    let handle = std::thread::spawn(move || {
        let mut last_trigger = std::time::Instant::now();
        let mut new_speech = false; // a final arrived since the last reply → worth a throttled reply
        let mut awaiting = false; // a reply is in flight (don't stack another)
        let mut awaiting_since = std::time::Instant::now();
        let mut streamed = 0usize; // chars of the in-progress reply already streamed to the UI
        let (sink, mut info) = audit;
        let (at_ms, started) = (now_ms(), std::time::Instant::now());
        let mut replies: Vec<String> = Vec::new();

        while !stop_worker.load(Ordering::Relaxed) {
            // 1. Inject every authoritative final waiting — the diarized anchor the model trusts.
            while let Ok(text) = finals_rx.try_recv() {
                engine.inject_text(&text);
                info.context.push('\n');
                info.context.push_str(&text);
                new_speech = true;
            }

            // 2. Feed audio + drain any reply. Continuous capture means a frame is always close behind,
            // so the stop flag is honoured within one frame; `None` = the live tee was dropped → stop.
            let frame = match source.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => break,
            };
            let result = engine.accept_waveform(frame.sample_rate, &frame.samples);
            if result.is_endpoint {
                let text = result.text.trim();
                if !text.is_empty() {
                    let _ = app.emit(ASSIST_TEXT_EVENT, text.to_owned());
                    replies.push(text.to_owned());
                }
                streamed = 0; // the reply closed; the next one starts a fresh stream
                awaiting = false; // the reply completed
            } else {
                // Stream the in-progress reply as it grows: emit only the new suffix (char-safe for CJK),
                // so the feed fills token-by-token instead of waiting for the whole reply.
                let total = result.text.chars().count();
                if total > streamed {
                    let delta: String = result.text.chars().skip(streamed).collect();
                    let _ = app.emit(ASSIST_DELTA_EVENT, delta);
                    streamed = total;
                }
            }

            // A stalled reply must not wedge the cadence forever.
            if awaiting && awaiting_since.elapsed() >= ASSIST_RESPONSE_TIMEOUT {
                awaiting = false;
            }

            // 3. Trigger a reply only when idle: a manual pull always fires; otherwise throttle, and
            // only when there's been new speech since the last one (never reply to silence).
            if !awaiting {
                let manual = hint_worker.swap(false, Ordering::Relaxed);
                let throttled = new_speech && last_trigger.elapsed() >= ASSIST_THROTTLE;
                if manual || throttled {
                    engine.request_response();
                    last_trigger = std::time::Instant::now();
                    awaiting = true;
                    awaiting_since = last_trigger;
                    new_speech = false;
                }
            }
        }

        sink(AuditRecord::finished(
            info,
            at_ms,
            started.elapsed(),
            Ok((replies.join("\n\n"), None)),
        ));
        source
    });

    AssistWorker { stop, hint, handle }
}

/// Starts the realtime AI assist over the live session's audio. Off the main thread — the WebSocket
/// handshake blocks ~1-2s, which would freeze the UI (the Connecting spinner can't animate).
#[tauri::command]
pub(crate) async fn start_assist_realtime(
    app: AppHandle,
    provider: String,
    model: String,
    instructions: String,
    params: HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        start_assist_realtime_blocking(app, provider, model, instructions, params)
    })
    .await
    .map_err(|e| format!("assist start task failed: {e}"))?
}

/// Connects an OpenAI realtime `model` that listens to the same mic+system mix as transcription and
/// answers each turn under `instructions` (the user's assist prompt). Requires a live session (for the
/// audio to tap) and that the assist isn't already running. Surfaces a missing key / bad model / failed
/// handshake as an error; on a build failure the mix source is restored so a retry can run.
fn start_assist_realtime_blocking(
    app: AppHandle,
    provider: String,
    model: String,
    instructions: String,
    params: HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    let state = app.state::<AppState>();

    if assist_worker_running(&state.assist)? {
        return Err("the realtime assist is already running".to_owned());
    }

    // Realtime assist speaks OpenAI's `response.output_text` protocol; other providers' live APIs
    // differ, so they aren't wired yet (a chat model runs the polling assist instead).
    if provider != "openai" {
        return Err("realtime assist currently supports OpenAI realtime models".to_owned());
    }

    let key = cloud_key(&state, &provider)?;

    // The model's full instruction is exactly what the user sees + edits in the assist prompt — no
    // hidden backend preamble. (The frontend's realtime prompt carries the grounding/anti-conversational
    // rules, visibly, so every detail is the user's to read and tune.)
    let source = state
        .assist
        .audio
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .take()
        .ok_or("start a live session before the realtime assist")?;

    let app_err = app.clone();
    let on_error: Box<dyn Fn(&str) + Send> = Box::new(move |msg: &str| {
        let _ = app_err.emit(ASSIST_ERROR_EVENT, msg.to_owned());
    });

    let assist_params = build_param_values(&assist_realtime_param_specs(), &params);
    let audit = crate::audit::sink(&state, crate::audit::live_meeting(&state));
    let info = realtime_call_info(&model, &instructions);
    let engine = match build_assist_engine(&model, &key, &instructions, &assist_params, on_error) {
        Ok(engine) => engine,
        Err(e) => {
            audit(AuditRecord::finished(
                info,
                now_ms(),
                std::time::Duration::ZERO,
                Err(e.to_string()),
            ));
            *state
                .assist
                .audio
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())? = Some(source);
            return Err(e.to_string());
        }
    };

    // Open the finals channel so the live sink starts feeding the assist its diarized anchors.
    let (finals_tx, finals_rx) = std::sync::mpsc::channel::<String>();
    *state
        .assist
        .finals_tx
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = Some(finals_tx);

    let worker = spawn_assist_worker(app.clone(), engine, source, finals_rx, (audit, info));
    *state
        .assist
        .worker
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = Some(worker);

    Ok(())
}

/// Heads the logged context of a realtime session: what it sent that the log can't show.
const REALTIME_AUDIO_NOTE: &str =
    "[Live meeting audio streamed to the model for the whole session; audio is not stored here. \
     Transcript lines sent as text follow. Tokens are estimated from text only.]";

/// What the activity log records about a realtime session, before any transcript line is sent.
fn realtime_call_info(model: &str, instructions: &str) -> CallInfo {
    let local = std::env::var(REALTIME_URL_ENV).is_ok_and(|url| is_loopback(&url));
    CallInfo {
        task: "realtime_assist".to_owned(),
        backend: "OpenAI Realtime".to_owned(),
        model: Some(model.to_owned()),
        local,
        instructions: instructions.to_owned(),
        context: REALTIME_AUDIO_NOTE.to_owned(),
        images: Vec::new(),
    }
}

/// Pulls a realtime-assist reply on demand — the "give me a hint now" button. Sets the worker's
/// one-shot flag, which its loop consumes on the next frame. No-op error if the assist isn't running.
#[tauri::command]
pub(crate) fn assist_hint_now(state: State<'_, AppState>) -> Result<(), String> {
    let guard = state
        .assist
        .worker
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;

    match guard.as_ref() {
        Some(worker) => {
            worker
                .hint
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        None => Err("the realtime assist isn't running".to_owned()),
    }
}

/// Whether an assist worker is currently running (a peek that doesn't take it).
fn assist_worker_running(state: &AssistState) -> Result<bool, String> {
    Ok(state
        .worker
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .is_some())
}

/// Stops the realtime AI assist and restores the mix source so it can be started again within the same
/// live session. No-op when the assist isn't running. Off the main thread — the join may take a moment.
#[tauri::command]
pub(crate) async fn stop_assist_realtime(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || stop_assist_realtime_blocking(app))
        .await
        .map_err(|e| format!("assist stop task failed: {e}"))?
}

fn stop_assist_realtime_blocking(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Stop the live sink from routing finals before joining the worker (it's about to be gone).
    *state
        .assist
        .finals_tx
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = None;

    if let Some(worker) = take_assist_worker(&state.assist)? {
        // Restore the source so the assist can restart while the session is still live (a session-level
        // Stop clears it separately).
        if let Some(source) = worker.stop_join() {
            *state
                .assist
                .audio
                .lock()
                .map_err(|_| "state lock poisoned".to_owned())? = Some(source);
        }
    }

    Ok(())
}

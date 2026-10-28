//! Push-to-talk dictation: hold a global hotkey, speak, release — Wisp transcribes on-device and
//! pastes the text into whatever app has focus.
//!
//! Two paths, picked by [`choose_dictation_engine`]:
//! - **Apple speech** (macOS 26+): the streaming pipeline (mic → Apple engine → finals) with a sink
//!   that accumulates text instead of driving the UI.
//! - **Local batch model** (e.g. Parakeet v3): the mic is buffered while the key is held; on release
//!   the clip is transcribed once with `transcribe_clip`, off the shortcut thread.
//!
//! Either way the text is inserted via `wisp-textinject`, which needs Accessibility permission.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use wisp_audio::{MicSource, Resampler, TARGET_SAMPLE_RATE};
use wisp_core::audio::AudioSource;
use wisp_core::engine::{AsrEngine, ClipOptions};
use wisp_core::model::{ModelFamily, ModelId, ModelStore};
use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptEvent};
use wisp_pipeline::Session;

use crate::{
    apple_speech_available, build_apple_speech_engine, build_engine, custom_descriptor,
    permissions, resolve_local_model, AppState,
};

/// Default push-to-talk hotkey — hold to dictate, release to insert. The user can change it.
pub(crate) const DEFAULT_DICTATION_HOTKEY: &str = "CmdOrCtrl+Shift+D";

/// Label of the floating "listening" indicator window. It's created once at startup, kept
/// transparent and click-through, and only shows the pill while the hotkey is held (see
/// [`show_indicator`]). A fixed label so [`AppHandle::get_webview_window`] can find it.
const INDICATOR_LABEL: &str = "dictation-indicator";

/// The local model dictation prefers when it is installed and the active model isn't a batch model.
const PREFERRED_BATCH_MODEL: &str = "parakeet-v3";

/// Longest clip the batch path keeps (at 16 kHz). Audio past the cap is dropped, bounding memory
/// and decode time if the key is held down by accident.
const MAX_CLIP_SECONDS: usize = 120;

/// Clips shorter than this are an accidental tap, not speech — skip the decode.
const MIN_CLIP_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 3 / 10;

/// Silence appended to the clip so the decoder doesn't drop the last word at the cut.
const TAIL_PADDING_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 3 / 10;

/// Below this RMS level a clip is treated as silence and not decoded. Whisper tends to invent a
/// phrase ("Thank you.") for silence, which would then be pasted. About -46 dBFS: quieter than any
/// speech a laptop mic picks up, louder than a quiet room.
const MIN_CLIP_RMS: f32 = 0.005;

/// Serializes model loads so a warm-up and a key release never load twice. Separate from the cache
/// lock, which is only held briefly, so dropping the cache never waits on a load.
static LOADING: Mutex<()> = Mutex::new(());

/// Bumped on every drop. A load that started before a drop does not install its engine.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Checks a push-to-talk hotkey string before it's registered, returning the parsed shortcut or a
/// user-facing reason it can't be used:
/// - it doesn't parse as a shortcut,
/// - it's Wisp's Capture Context shortcut (that combination is already taken),
/// - it has no modifier and isn't a function key — a bare printable key, registered *globally*,
///   would be swallowed in every app, so the user could no longer type it anywhere.
///
/// Comparison against Capture Context is done on the parsed shortcut, so spelling (`Cmd` vs `Super`,
/// token order) doesn't matter.
pub(crate) fn validate_hotkey(hotkey: &str) -> Result<Shortcut, String> {
    let shortcut: Shortcut = hotkey.trim().parse().map_err(|_| {
        format!(
            "“{}” isn't a usable shortcut — try another combination",
            hotkey.trim()
        )
    })?;

    if let Ok(capture) = crate::context::CAPTURE_SHORTCUT.parse::<Shortcut>() {
        if shortcut == capture {
            return Err(
                "That combination is reserved for Capture Context. Pick a different one."
                    .to_owned(),
            );
        }
    }

    if shortcut.mods.is_empty() && !is_function_key(&shortcut.key.to_string()) {
        return Err(
            "Add a modifier (⌘, ⌥, ⌃ or ⇧). A plain key would be captured in every app.".to_owned(),
        );
    }

    Ok(shortcut)
}

/// Whether a key's W3C code name (`shortcut.key.to_string()`, e.g. `"F5"`, `"KeyD"`) is a function
/// key — the only keys safe to use bare, since they don't type a character.
fn is_function_key(code_name: &str) -> bool {
    code_name.len() > 1
        && code_name.starts_with('F')
        && code_name[1..].bytes().all(|b| b.is_ascii_digit())
}

/// The persisted dictation settings: the chosen hotkey and whether push-to-talk is on. Saved to
/// local app data so the choice survives a restart (the dictated text itself is never stored —
/// it's pasted into the focused app and gone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DictationSettings {
    pub enabled: bool,
    pub hotkey: String,
}

impl Default for DictationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hotkey: DEFAULT_DICTATION_HOTKEY.to_owned(),
        }
    }
}

impl DictationSettings {
    /// Loads the settings, falling back to defaults when the file is missing or unreadable. A saved
    /// hotkey that no longer validates (e.g. it now collides with Capture Context) is reset to the
    /// default so a bad value can't wedge dictation.
    pub(crate) fn load(path: &Path) -> Self {
        let mut settings = fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str::<DictationSettings>(&json).ok())
            .unwrap_or_default();
        if validate_hotkey(&settings.hotkey).is_err() {
            settings.hotkey = DEFAULT_DICTATION_HOTKEY.to_owned();
        }
        settings
    }

    /// Persists the settings. Best-effort; the error names where the write failed.
    fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| format!("save dictation settings: {e}"))
    }
}

/// Which engine push-to-talk dictation runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DictationEngine {
    /// Apple on-device speech, streamed (macOS 26+).
    AppleSpeech,
    /// An installed local batch model, run once per clip.
    Local(ModelId),
}

/// Whether `family` is an on-device engine that transcribes a whole clip at once.
fn is_local_batch(family: ModelFamily) -> bool {
    matches!(
        family,
        ModelFamily::Whisper
            | ModelFamily::WhisperCpp
            | ModelFamily::SenseVoice
            | ModelFamily::Paraformer
            | ModelFamily::Parakeet
    )
}

/// Picks dictation's engine. Apple speech wins when available. Otherwise use a local batch model from
/// `installed` (in catalog order): the active model if it is one, else Parakeet v3, else the first.
/// `None` means dictation can't run here.
pub(crate) fn choose_dictation_engine(
    apple_available: bool,
    installed: &[(ModelId, ModelFamily)],
    active: Option<&ModelId>,
) -> Option<DictationEngine> {
    if apple_available {
        return Some(DictationEngine::AppleSpeech);
    }
    let batch: Vec<&ModelId> = installed
        .iter()
        .filter(|(_, family)| is_local_batch(*family))
        .map(|(id, _)| id)
        .collect();
    let pick = active
        .and_then(|a| batch.iter().find(|id| **id == a))
        .or_else(|| batch.iter().find(|id| id.as_str() == PREFERRED_BATCH_MODEL))
        .or_else(|| batch.first())?;
    Some(DictationEngine::Local((*pick).clone()))
}

/// The installed local ASR models (catalog first, then custom imports), with their families.
fn installed_models(state: &AppState) -> Vec<(ModelId, ModelFamily)> {
    let mut models: Vec<(ModelId, ModelFamily)> = state
        .store
        .available()
        .into_iter()
        .filter(|d| !d.files.is_empty() && state.store.local_path(&d.id).is_some())
        .map(|d| (d.id, d.family))
        .collect();
    if let Ok(custom) = state.custom_models.lock() {
        models.extend(
            custom
                .iter()
                .filter_map(custom_descriptor)
                .map(|d| (d.id, d.family)),
        );
    }
    models
}

/// The engine dictation would use right now, given the host and the installed/active models.
fn current_choice(state: &AppState) -> Option<DictationEngine> {
    let active = state.active.lock().ok().and_then(|a| a.clone());
    choose_dictation_engine(
        apple_speech_available(),
        &installed_models(state),
        active.as_ref(),
    )
}

/// A loaded engine, shared between the cache and a dictation in progress.
type SharedEngine = Arc<Mutex<Box<dyn AsrEngine>>>;

/// A loaded batch engine, kept between dictations so each key press doesn't reload the model.
pub(crate) struct CachedEngine {
    model: ModelId,
    language: String,
    engine: SharedEngine,
}

/// The dictation engine cache held in `AppState`.
pub(crate) type EngineCache = Mutex<Option<CachedEngine>>;

/// The cached engine for `model` + `language`, if that is what's loaded.
fn cached_engine(
    state: &AppState,
    model: &ModelId,
    language: &str,
) -> Result<Option<SharedEngine>, String> {
    let cache = state
        .dictation_engine
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    Ok(cache
        .as_ref()
        .filter(|c| c.model == *model && c.language == language)
        .map(|c| c.engine.clone()))
}

/// Returns the cached engine for `model` + `language`, loading it (and replacing any other) if
/// needed. The load runs outside the cache lock; see [`LOADING`] and [`GENERATION`].
fn engine_for(state: &AppState, model: &ModelId, language: &str) -> Result<SharedEngine, String> {
    if let Some(engine) = cached_engine(state, model, language)? {
        return Ok(engine);
    }
    let _loading = LOADING
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    // Another load may have finished while this one waited.
    if let Some(engine) = cached_engine(state, model, language)? {
        return Ok(engine);
    }
    let generation = GENERATION.load(Ordering::SeqCst);
    let (descriptor, dir) = resolve_local_model(state, model)?;
    let engine = build_engine(&descriptor, &dir, language).map_err(|e| e.to_string())?;
    let engine = Arc::new(Mutex::new(engine));
    let mut cache = state
        .dictation_engine
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;
    if GENERATION.load(Ordering::SeqCst) == generation {
        *cache = Some(CachedEngine {
            model: model.clone(),
            language: language.to_owned(),
            engine: engine.clone(),
        });
    }
    Ok(engine)
}

/// Drops the cached batch engine (dictation off, the model set changed, or a live session needs
/// the memory). It reloads on next use. Never waits on a load in progress.
pub(crate) fn drop_cached_engine(state: &AppState) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut cache) = state.dictation_engine.lock() {
        *cache = None;
    }
}

/// Loads the batch engine in the background so the first dictation doesn't wait for it.
fn warm_engine(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        let state = app.state::<AppState>();
        let Some(DictationEngine::Local(model)) = current_choice(&state) else {
            return;
        };
        let language = current_language(&state);
        if let Err(e) = engine_for(&state, &model, &language) {
            eprintln!("wisp: dictation model load failed: {e}");
        }
    });
}

fn current_language(state: &AppState) -> String {
    state.language.lock().map(|l| l.clone()).unwrap_or_default()
}

/// Mono 16 kHz samples, capped at a fixed length: audio past the cap is dropped.
struct ClipBuffer {
    samples: Vec<f32>,
    cap: usize,
}

/// Root-mean-square level of `samples` (0 for none).
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

impl ClipBuffer {
    fn new(cap: usize) -> Self {
        Self {
            samples: Vec::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[f32]) {
        let room = self.cap.saturating_sub(self.samples.len());
        self.samples
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// The clip ready to decode — tail-padded — or `None` if it's too short to hold speech.
    fn into_clip(self) -> Option<Vec<f32>> {
        if self.samples.len() < MIN_CLIP_SAMPLES || rms(&self.samples) < MIN_CLIP_RMS {
            return None;
        }
        let mut clip = self.samples;
        clip.resize(clip.len() + TAIL_PADDING_SAMPLES, 0.0);
        Some(clip)
    }
}

/// The mic capture of a batch dictation: a thread pulling frames into a [`ClipBuffer`] until stopped.
pub(crate) struct BatchCapture {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<ClipBuffer>,
}

impl BatchCapture {
    fn start(mut mic: Box<dyn AudioSource>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let handle = thread::spawn(move || {
            let mut buffer = ClipBuffer::new(MAX_CLIP_SECONDS * TARGET_SAMPLE_RATE as usize);
            let mut resampler = Resampler::new(TARGET_SAMPLE_RATE);
            while !stop_for_thread.load(Ordering::Relaxed) {
                match mic.next_frame() {
                    Ok(Some(frame)) => buffer.push(&resampler.process(&frame)),
                    _ => break,
                }
            }
            buffer
        });
        Self { stop, handle }
    }

    /// Stops capturing and returns what was recorded. The mic delivers frames every few ms, so the
    /// thread notices the stop flag almost at once.
    fn finish(self) -> ClipBuffer {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_else(|_| ClipBuffer::new(0))
    }
}

/// A running dictation, while the hotkey is held.
pub(crate) enum Dictation {
    /// Apple speech: the streaming session and the text it has accumulated.
    Streaming {
        session: Session,
        buffer: Arc<Mutex<DictationBuffer>>,
    },
    /// A local batch model: the mic capture, decoded by `model` on release.
    Batch {
        capture: BatchCapture,
        model: ModelId,
    },
}

/// Accumulated dictation text: committed finals plus the still-open partial. They're combined on
/// release so the last (not-yet-finalised) words aren't lost when the key comes up.
#[derive(Default)]
pub(crate) struct DictationBuffer {
    committed: Vec<String>,
    pending: String,
}

impl DictationBuffer {
    fn push_final(&mut self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.committed.push(text.to_owned());
        }
        self.pending.clear();
    }

    fn set_partial(&mut self, text: &str) {
        self.pending = text.trim().to_owned();
    }

    /// The full dictated text — finals then the open partial — joined with the spacing rule.
    fn collect(&self) -> String {
        let mut pieces = self.committed.clone();
        let pending = self.pending.trim();
        if !pending.is_empty() {
            pieces.push(pending.to_owned());
        }
        join_pieces(&pieces)
    }
}

/// Joins transcript pieces, inserting a space only before an ASCII-word-leading piece (so spaced
/// languages read correctly while CJK runs together).
fn join_pieces(pieces: &[String]) -> String {
    let mut out = String::new();
    for piece in pieces {
        let needs_space = !out.is_empty()
            && piece
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if needs_space {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Accumulates the streaming session's segments into `buffer`: finals commit, partials replace the
/// pending tail. (No UI emission — dictation's output is the paste, not the feed.)
fn dictation_sink(buffer: Arc<Mutex<DictationBuffer>>) -> wisp_pipeline::EventSink {
    Box::new(move |event| {
        if let TranscriptEvent::Segment(segment) = event {
            if let Ok(mut buffer) = buffer.lock() {
                match segment.status {
                    SegmentStatus::Final => buffer.push_final(&segment.text),
                    _ => buffer.set_partial(&segment.text),
                }
            }
        }
    })
}

/// Starts capturing on key-down: Apple speech streams the mic into a session whose finals accumulate
/// in a buffer; a local batch model just buffers the mic. A no-op if already dictating (key-repeat);
/// refused while a meeting is live.
fn start_dictation(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();

    if state
        .dictation
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .is_some()
    {
        return Ok(());
    }
    if !state
        .sessions
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .is_empty()
    {
        return Err("stop the live session before dictating".to_owned());
    }

    let choice = current_choice(&state).ok_or("no on-device dictation engine is available")?;
    let mic: Box<dyn AudioSource> = Box::new(MicSource::from_default().map_err(|e| e.to_string())?);

    let dictation = match choice {
        DictationEngine::AppleSpeech => {
            let engine =
                build_apple_speech_engine(&current_language(&state)).map_err(|e| e.to_string())?;
            let buffer = Arc::new(Mutex::new(DictationBuffer::default()));
            let session = Session::spawn_streaming(
                engine,
                mic,
                dictation_sink(buffer.clone()),
                None,
                AudioSourceKind::Microphone,
            );
            Dictation::Streaming { session, buffer }
        }
        DictationEngine::Local(model) => Dictation::Batch {
            capture: BatchCapture::start(mic),
            model,
        },
    };

    *state
        .dictation
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = Some(dictation);
    show_indicator(app);
    Ok(())
}

/// Stops capturing on key-up and pastes the text into the frontmost app. The batch decode runs on its
/// own thread so the shortcut handler returns at once.
fn finish_dictation(app: &AppHandle) {
    hide_indicator(app);
    let state = app.state::<AppState>();
    let Some(dictation) = state.dictation.lock().ok().and_then(|mut d| d.take()) else {
        return;
    };

    match dictation {
        Dictation::Streaming { session, buffer } => {
            let _ = session.stop();
            let text = buffer
                .lock()
                .map(|buffer| buffer.collect())
                .unwrap_or_default();
            paste(&text);
        }
        Dictation::Batch { capture, model } => {
            let app = app.clone();
            thread::spawn(move || {
                let Some(clip) = capture.finish().into_clip() else {
                    return;
                };
                match transcribe_batch(&app.state::<AppState>(), &model, &clip) {
                    Ok(text) => paste(&text),
                    Err(e) => eprintln!("wisp: dictation transcription failed: {e}"),
                }
            });
        }
    }
}

/// Decodes a 16 kHz clip with the cached engine for `model` and joins the segments into one string.
fn transcribe_batch(state: &AppState, model: &ModelId, clip: &[f32]) -> Result<String, String> {
    let language = current_language(state);
    let accurate = state.live_accurate.lock().map(|a| *a).unwrap_or(false);
    let engine = engine_for(state, model, &language)?;
    let mut engine = engine
        .lock()
        .map_err(|_| "dictation engine lock poisoned".to_owned())?;
    let result = engine
        .transcribe_clip(
            clip,
            TARGET_SAMPLE_RATE,
            ClipOptions::new(false, accurate, ""),
        )
        .map_err(|e| e.to_string())?;
    let pieces: Vec<String> = result
        .segments
        .iter()
        .map(|s| s.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    Ok(join_pieces(&pieces))
}

fn paste(text: &str) {
    if text.is_empty() {
        return;
    }
    if let Err(e) = wisp_textinject::paste_text(text) {
        eprintln!("wisp: dictation paste failed: {e}");
    }
}

/// Routes a hotkey press/release into start/finish. Errors are non-fatal (logged) — a missed dictation
/// must never crash the app.
fn on_shortcut(app: &AppHandle, state: ShortcutState) {
    match state {
        ShortcutState::Pressed => {
            if let Err(e) = start_dictation(app) {
                eprintln!("wisp: dictation start skipped: {e}");
            }
        }
        ShortcutState::Released => finish_dictation(app),
    }
}

/// Fades the "listening" pill in (key pressed) by toggling a class in the overlay page. Best-effort:
/// a missing or not-yet-loaded window just means no pill this time — never an error that reaches the
/// user. The `&&` guard keeps the eval harmless before the page has defined the function.
fn show_indicator(app: &AppHandle) {
    set_indicator(app, true);
}

/// Fades the "listening" pill out (key released).
fn hide_indicator(app: &AppHandle) {
    set_indicator(app, false);
}

fn set_indicator(app: &AppHandle, on: bool) {
    if let Some(window) = app.get_webview_window(INDICATOR_LABEL) {
        let _ = window.eval(format!(
            "window.wispIndicator && window.wispIndicator({on})"
        ));
    }
}

/// Builds the floating indicator overlay and restores the persisted on/off state. Called once from
/// the app's setup hook. The overlay is macOS-only (dictation's paste path is, and the transparent
/// window needs the macOS private API); restoring the shortcut just reuses [`set_dictation_enabled`].
pub(crate) fn init(app: &AppHandle, settings: &DictationSettings) {
    #[cfg(target_os = "macos")]
    if let Err(e) = build_indicator(app) {
        eprintln!("wisp: dictation indicator window failed to build: {e}");
    }

    if settings.enabled {
        // Reuse the command so registration, engine warm-up, and persistence stay in one place. On
        // failure (no engine, hotkey refused) it leaves dictation off, keeping the reported state honest.
        if let Err(e) = set_dictation_enabled(app.clone(), true, Some(settings.hotkey.clone())) {
            eprintln!("wisp: restoring dictation on launch failed: {e}");
        }
    }
}

/// Creates the always-on-top, transparent, click-through overlay that hosts the pill. It stays
/// "visible" to the OS the whole time but renders nothing until [`show_indicator`] fades the pill in,
/// so it never steals focus from the app you're dictating into (which would misdirect the paste).
#[cfg(target_os = "macos")]
fn build_indicator(app: &AppHandle) -> tauri::Result<()> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    if app.get_webview_window(INDICATOR_LABEL).is_some() {
        return Ok(());
    }

    const WIDTH: f64 = 188.0;
    const HEIGHT: f64 = 46.0;
    /// Gap above the bottom edge of the screen.
    const BOTTOM_MARGIN: f64 = 96.0;

    let window = WebviewWindowBuilder::new(
        app,
        INDICATOR_LABEL,
        WebviewUrl::App("dictation.html".into()),
    )
    .title("")
    .inner_size(WIDTH, HEIGHT)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .focused(false)
    .skip_taskbar(true)
    .resizable(false)
    .shadow(false)
    .visible_on_all_workspaces(true)
    .build()?;

    // Let every click pass through to whatever is underneath — the pill is purely informational.
    let _ = window.set_ignore_cursor_events(true);

    // Park it near the bottom-centre of the primary display.
    if let Ok(Some(monitor)) = window.primary_monitor() {
        let size = monitor.size().to_logical::<f64>(monitor.scale_factor());
        let x = ((size.width - WIDTH) / 2.0).max(0.0);
        let y = (size.height - HEIGHT - BOTTOM_MARGIN).max(0.0);
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }

    Ok(())
}

/// The global-shortcut plugin: the Capture Context shortcut (registered only during a meeting
/// with intelligence on) captures on press; any other registered shortcut is the push-to-talk
/// dictation key. Built for the app's `Wry` runtime.
pub(crate) fn shortcut_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, shortcut, event| {
            let capture = crate::context::CAPTURE_SHORTCUT.parse::<Shortcut>().ok();
            if capture.as_ref() == Some(shortcut) {
                if event.state() == ShortcutState::Pressed {
                    crate::context::on_shortcut(app);
                }
                return;
            }
            on_shortcut(app, event.state())
        })
        .build()
}

/// Dictation availability + current config, for the settings UI.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DictationStatus {
    /// On-device dictation can run here (Apple speech, or an installed local batch model).
    available: bool,
    /// Which engine dictation uses: `"apple"`, `"local"`, or `None` when unavailable.
    engine_kind: Option<&'static str>,
    /// The local model's short display name (e.g. "Parakeet v3"), when `engine_kind` is `"local"`.
    engine_name: Option<String>,
    /// Accessibility permission is granted (needed to paste into other apps).
    accessibility_ok: bool,
    /// The hotkey is currently registered.
    enabled: bool,
    /// The configured push-to-talk hotkey.
    hotkey: String,
}

/// The short UI name for a local model: the display name up to its first " · " detail.
fn short_model_name(state: &AppState, model: &ModelId) -> String {
    resolve_local_model(state, model)
        .ok()
        .map(|(d, _)| d.display_name)
        .and_then(|name| name.split(" · ").next().map(|n| n.trim().to_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| model.as_str().to_owned())
}

fn status(state: &AppState) -> Result<DictationStatus, String> {
    let choice = current_choice(state);
    let (engine_kind, engine_name) = match &choice {
        Some(DictationEngine::AppleSpeech) => (Some("apple"), None),
        Some(DictationEngine::Local(model)) => {
            (Some("local"), Some(short_model_name(state, model)))
        }
        None => (None, None),
    };
    Ok(DictationStatus {
        available: choice.is_some(),
        engine_kind,
        engine_name,
        accessibility_ok: permissions::accessibility_authorized(),
        enabled: *state
            .dictation_enabled
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?,
        hotkey: state
            .dictation_hotkey
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())?
            .clone(),
    })
}

#[tauri::command]
pub(crate) fn dictation_status(state: State<'_, AppState>) -> Result<DictationStatus, String> {
    status(&state)
}

/// Enables or disables push-to-talk dictation, optionally changing the hotkey. Registers/unregisters
/// the global shortcut accordingly and returns the updated status. Enabling with a local model starts
/// loading it in the background; disabling drops it.
#[tauri::command]
pub(crate) fn set_dictation_enabled(
    app: AppHandle,
    enabled: bool,
    hotkey: Option<String>,
) -> Result<DictationStatus, String> {
    let state = app.state::<AppState>();
    let previous = state
        .dictation_hotkey
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .clone();

    // Validate a new hotkey before storing it, so a bad or conflicting combination never becomes the
    // saved setting. An empty/omitted value keeps the current hotkey.
    if let Some(hotkey) = hotkey
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
    {
        validate_hotkey(&hotkey)?;
        *state
            .dictation_hotkey
            .lock()
            .map_err(|_| "state lock poisoned".to_owned())? = hotkey;
    }
    let hotkey = state
        .dictation_hotkey
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .clone();

    let shortcuts = app.global_shortcut();
    // Only the dictation key: the capture shortcut may be registered for a running meeting.
    if let Ok(old) = previous.parse::<Shortcut>() {
        if shortcuts.is_registered(old) {
            let _ = shortcuts.unregister(old);
        }
    }

    if enabled {
        if current_choice(&state).is_none() {
            return Err(
                "dictation needs Apple on-device speech (macOS 26 or newer) or a \
                 downloaded on-device model such as Parakeet v3"
                    .to_owned(),
            );
        }
        // Re-validate: guards a hotkey persisted before a rule changed, as well as the stored default.
        let shortcut = validate_hotkey(&hotkey)?;
        shortcuts
            .register(shortcut)
            .map_err(|e| format!("could not register the hotkey: {e}"))?;
        warm_engine(&app);
    } else {
        drop_cached_engine(&state);
    }

    *state
        .dictation_enabled
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = enabled;

    // Persist the choice so it survives a restart. Best-effort — a write failure is logged, not fatal.
    let result = status(&state);
    if let Err(e) = (DictationSettings { enabled, hotkey }).save(&state.dictation_settings_path) {
        eprintln!("wisp: could not persist dictation settings: {e}");
    }

    result
}

/// Opens System Settings → Privacy → Accessibility so the user can grant the paste permission.
#[tauri::command]
pub(crate) fn open_accessibility_settings() -> Result<(), String> {
    permissions::open_privacy_settings("accessibility")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkey_is_valid() {
        assert!(validate_hotkey(DEFAULT_DICTATION_HOTKEY).is_ok());
    }

    #[test]
    fn validate_accepts_a_modified_combo_regardless_of_spelling() {
        assert!(validate_hotkey("CmdOrCtrl+Shift+D").is_ok());
        // The browser recorder sends W3C code names; the parser accepts them too.
        assert!(validate_hotkey("Control+Alt+KeyK").is_ok());
        assert!(validate_hotkey("Shift+Digit5").is_ok());
    }

    #[test]
    fn validate_allows_a_bare_function_key_but_not_a_bare_printable_key() {
        // Function keys don't type a character, so they're safe to grab globally with no modifier.
        assert!(validate_hotkey("F5").is_ok());
        // A bare letter/digit/space would be swallowed everywhere — refused.
        assert!(validate_hotkey("KeyD").is_err());
        assert!(validate_hotkey("D").is_err());
        assert!(validate_hotkey("Space").is_err());
        assert!(validate_hotkey("Digit1").is_err());
    }

    #[test]
    fn validate_rejects_the_capture_context_shortcut() {
        // Exact string, and an equivalent spelling of the same chord, both collide.
        assert!(validate_hotkey(crate::context::CAPTURE_SHORTCUT).is_err());
        assert!(validate_hotkey("Cmd+Alt+Shift+KeyC").is_err());
    }

    #[test]
    fn validate_rejects_unparseable_input() {
        assert!(validate_hotkey("").is_err());
        assert!(validate_hotkey("not a hotkey").is_err());
        assert!(validate_hotkey("Shift+").is_err());
    }

    #[test]
    fn is_function_key_matches_only_f_plus_digits() {
        assert!(is_function_key("F1"));
        assert!(is_function_key("F12"));
        assert!(!is_function_key("F"));
        assert!(!is_function_key("KeyF"));
        assert!(!is_function_key("Find"));
    }

    #[test]
    fn settings_round_trip_and_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictation.json");

        // Missing file → defaults.
        let loaded = DictationSettings::load(&path);
        assert!(!loaded.enabled);
        assert_eq!(loaded.hotkey, DEFAULT_DICTATION_HOTKEY);

        // A saved value round-trips.
        let saved = DictationSettings {
            enabled: true,
            hotkey: "Control+Alt+KeyK".to_owned(),
        };
        saved.save(&path).unwrap();
        assert_eq!(DictationSettings::load(&path), saved);

        // A persisted hotkey that no longer validates is reset, but the enabled flag is kept.
        DictationSettings {
            enabled: true,
            hotkey: crate::context::CAPTURE_SHORTCUT.to_owned(),
        }
        .save(&path)
        .unwrap();
        let loaded = DictationSettings::load(&path);
        assert!(loaded.enabled);
        assert_eq!(loaded.hotkey, DEFAULT_DICTATION_HOTKEY);

        // Garbage file → defaults.
        fs::write(&path, "}{ not json").unwrap();
        assert_eq!(DictationSettings::load(&path), DictationSettings::default());
    }

    #[test]
    fn join_pieces_spaces_ascii_but_runs_cjk_together() {
        assert_eq!(
            join_pieces(&["hello".to_owned(), "world".to_owned()]),
            "hello world"
        );
        assert_eq!(
            join_pieces(&["你好".to_owned(), "世界".to_owned()]),
            "你好世界"
        );
        // Mixed: no space before the CJK piece, a space before the ASCII one.
        assert_eq!(
            join_pieces(&["開會".to_owned(), "OK".to_owned()]),
            "開會 OK"
        );
    }

    #[test]
    fn buffer_combines_finals_with_the_open_partial() {
        let mut buffer = DictationBuffer::default();
        buffer.set_partial("hello wor");
        buffer.push_final("hello world");
        buffer.set_partial("how are");
        // committed "hello world" + still-open partial "how are".
        assert_eq!(buffer.collect(), "hello world how are");
    }

    fn id(s: &str) -> ModelId {
        ModelId(s.to_owned())
    }

    #[test]
    fn apple_speech_wins_when_available() {
        let installed = [(id("parakeet-v3"), ModelFamily::Parakeet)];
        assert_eq!(
            choose_dictation_engine(true, &installed, None),
            Some(DictationEngine::AppleSpeech)
        );
        assert_eq!(
            choose_dictation_engine(true, &[], None),
            Some(DictationEngine::AppleSpeech)
        );
    }

    #[test]
    fn active_batch_model_is_used() {
        let installed = [
            (id("parakeet-v3"), ModelFamily::Parakeet),
            (id("whisper-small"), ModelFamily::Whisper),
        ];
        assert_eq!(
            choose_dictation_engine(false, &installed, Some(&id("whisper-small"))),
            Some(DictationEngine::Local(id("whisper-small")))
        );
    }

    #[test]
    fn parakeet_v3_preferred_when_active_is_not_a_usable_batch_model() {
        let installed = [
            (id("whisper-small"), ModelFamily::Whisper),
            (id("streaming-zipformer"), ModelFamily::StreamingTransducer),
            (id("parakeet-v3"), ModelFamily::Parakeet),
        ];
        // Active is a streaming model: not usable for batch dictation.
        assert_eq!(
            choose_dictation_engine(false, &installed, Some(&id("streaming-zipformer"))),
            Some(DictationEngine::Local(id("parakeet-v3")))
        );
        // Active is not installed (or is Apple speech on an old Mac).
        assert_eq!(
            choose_dictation_engine(false, &installed, Some(&id("apple-speech"))),
            Some(DictationEngine::Local(id("parakeet-v3")))
        );
    }

    #[test]
    fn first_batch_model_is_the_fallback() {
        let installed = [
            (id("streaming-zipformer"), ModelFamily::StreamingTransducer),
            (id("sense-voice"), ModelFamily::SenseVoice),
            (id("whisper-small"), ModelFamily::Whisper),
        ];
        assert_eq!(
            choose_dictation_engine(false, &installed, None),
            Some(DictationEngine::Local(id("sense-voice")))
        );
    }

    #[test]
    fn nothing_usable_means_unavailable() {
        let installed = [
            (id("streaming-zipformer"), ModelFamily::StreamingTransducer),
            (id("diarize"), ModelFamily::Diarization),
            (id("gtcrn"), ModelFamily::Denoise),
        ];
        assert_eq!(choose_dictation_engine(false, &installed, None), None);
        assert_eq!(choose_dictation_engine(false, &[], None), None);
    }

    #[test]
    fn clip_buffer_stops_at_its_cap() {
        let mut buffer = ClipBuffer::new(10);
        buffer.push(&[0.1; 6]);
        buffer.push(&[0.2; 6]);
        buffer.push(&[0.3; 6]);
        assert_eq!(buffer.samples.len(), 10);
        // The start of the clip is kept; the overflow is dropped.
        assert_eq!(buffer.samples[5], 0.1);
        assert_eq!(buffer.samples[9], 0.2);
    }

    #[test]
    fn empty_or_tiny_clip_is_skipped() {
        assert_eq!(ClipBuffer::new(1_000_000).into_clip(), None);
        let mut tap = ClipBuffer::new(1_000_000);
        tap.push(&vec![0.1; MIN_CLIP_SAMPLES - 1]);
        assert_eq!(tap.into_clip(), None);
    }

    #[test]
    fn a_silent_clip_is_skipped() {
        let mut quiet = ClipBuffer::new(1_000_000);
        quiet.push(&vec![0.001; MIN_CLIP_SAMPLES * 4]);
        assert_eq!(quiet.into_clip(), None);
    }

    #[test]
    fn speech_clip_is_tail_padded_with_silence() {
        let mut buffer = ClipBuffer::new(1_000_000);
        buffer.push(&vec![0.5; MIN_CLIP_SAMPLES]);
        let clip = buffer.into_clip().expect("long enough to decode");
        assert_eq!(clip.len(), MIN_CLIP_SAMPLES + TAIL_PADDING_SAMPLES);
        assert_eq!(clip[MIN_CLIP_SAMPLES - 1], 0.5);
        assert_eq!(*clip.last().unwrap(), 0.0);
    }

    #[test]
    fn batch_capture_buffers_the_source_until_it_ends() {
        use std::time::Duration;
        use wisp_core::audio::{AudioFrame, AudioSourceInfo};

        /// Two stereo 48 kHz frames, then end of stream.
        struct Fake(usize);
        impl AudioSource for Fake {
            fn info(&self) -> AudioSourceInfo {
                AudioSourceInfo {
                    kind: AudioSourceKind::Microphone,
                    name: "fake".to_owned(),
                }
            }
            fn next_frame(&mut self) -> wisp_core::error::Result<Option<AudioFrame>> {
                if self.0 == 0 {
                    return Ok(None);
                }
                self.0 -= 1;
                Ok(Some(AudioFrame::new(
                    vec![0.25; 48_000 * 2],
                    48_000,
                    2,
                    Duration::ZERO,
                )))
            }
        }

        let capture = BatchCapture::start(Box::new(Fake(2)));
        // Let the fake source run dry before stopping.
        thread::sleep(Duration::from_millis(200));
        let buffer = capture.finish();
        // 2 s of 48 kHz stereo becomes ~2 s of 16 kHz mono.
        let expected = 2 * TARGET_SAMPLE_RATE as usize;
        assert!(
            buffer.samples.len().abs_diff(expected) < 100,
            "got {} samples",
            buffer.samples.len()
        );
    }
}

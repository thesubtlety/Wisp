//! Meeting detection shell (macOS 14.2+): every couple of seconds, read which apps run microphone
//! input, classify them, and feed [`wisp_core::meeting::MeetingDetector`]. Its events become a UI
//! event (the in-app banner), a tray status, and a system notification. Nothing here ever starts
//! or stops recording; the page asks the user first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use wisp_core::meeting::{Confidence, DetectedMeeting, MeetingEvent};

#[cfg(target_os = "macos")]
mod probe;

/// A meeting started while Wisp is idle: payload [`MeetingDto`].
pub(crate) const DETECTED_EVENT: &str = "meeting://detected";
/// The recorded meeting ended: payload [`MeetingDto`].
pub(crate) const ENDED_EVENT: &str = "meeting://ended";
/// The detected meeting went away before the user answered.
pub(crate) const GONE_EVENT: &str = "meeting://gone";

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Detection settings and the latest confirmed meeting, shared by the poller, tray, and commands.
pub(crate) struct MeetingState {
    /// The user's "Detect meetings" setting. Off until the page sends the saved value.
    enabled: AtomicBool,
    /// Whether this OS can list audio processes at all.
    supported: AtomicBool,
    /// The meeting of the current detection episode, for the tray status line.
    current: Mutex<Option<DetectedMeeting>>,
}

impl Default for MeetingState {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            supported: AtomicBool::new(cfg!(target_os = "macos")),
            current: Mutex::new(None),
        }
    }
}

impl MeetingState {
    pub(crate) fn current(&self) -> Option<DetectedMeeting> {
        self.current.lock().ok().and_then(|m| m.clone())
    }
}

/// A detected meeting, for the UI.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingDto {
    app: String,
    service: Option<String>,
    /// A browser holds the mic, but its window titles were unreadable.
    low_confidence: bool,
}

impl From<&DetectedMeeting> for MeetingDto {
    fn from(m: &DetectedMeeting) -> Self {
        Self {
            app: m.app.clone(),
            service: m.service.clone(),
            low_confidence: m.confidence == Confidence::Low,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingDetectionStatus {
    supported: bool,
    enabled: bool,
}

/// Turns detection on or off (the page sends the saved setting at startup and on every change).
#[tauri::command]
pub(crate) fn set_meeting_detection(
    state: State<'_, MeetingState>,
    enabled: bool,
) -> MeetingDetectionStatus {
    state.enabled.store(enabled, Ordering::SeqCst);
    MeetingDetectionStatus {
        supported: state.supported.load(Ordering::SeqCst),
        enabled,
    }
}

/// Starts the background poller. A no-op off macOS or where Core Audio can't list processes.
pub(crate) fn spawn_poller(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        if probe::audio_processes().is_err() {
            app.state::<MeetingState>()
                .supported
                .store(false, Ordering::SeqCst);
            return;
        }
        let app = app.clone();
        std::thread::spawn(move || poll_loop(&app));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[cfg(target_os = "macos")]
fn poll_loop(app: &AppHandle) {
    use wisp_core::meeting::{classify, MeetingDetector};

    let started = std::time::Instant::now();
    let own_pid = std::process::id() as i32;
    let own_bundle = app.config().identifier.clone();
    let mut detector = MeetingDetector::new();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let state = app.state::<MeetingState>();
        if !state.enabled.load(Ordering::SeqCst) {
            if detector.current().is_some() {
                detector.reset();
                publish_current(app, None);
                let _ = app.emit(GONE_EVENT, ());
            }
            continue;
        }
        let Ok(mic) = probe::mic_processes(own_pid) else {
            continue;
        };
        let mic: Vec<_> = mic
            .into_iter()
            .filter(|p| p.bundle_id != own_bundle)
            .collect();
        // Window titles are only needed when someone holds the mic.
        let windows = if mic.is_empty() {
            Vec::new()
        } else {
            probe::app_windows(own_pid)
        };
        let event = detector.observe(
            started.elapsed(),
            classify(&mic, &windows),
            crate::live_session_running(app),
        );
        publish_current(app, detector.current().cloned());
        if let Some(event) = event {
            on_event(app, event);
        }
    }
}

/// Stores the episode's meeting and refreshes the tray when it changed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn publish_current(app: &AppHandle, meeting: Option<DetectedMeeting>) {
    let state = app.state::<MeetingState>();
    let changed = match state.current.lock() {
        Ok(mut current) if *current != meeting => {
            *current = meeting;
            true
        }
        _ => false,
    };
    if changed {
        crate::tray::refresh(app);
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn on_event(app: &AppHandle, event: MeetingEvent) {
    match event {
        MeetingEvent::Detected(m) => {
            let _ = app.emit(DETECTED_EVENT, MeetingDto::from(&m));
            notify(app, "Meeting detected", &detected_body(&m));
        }
        MeetingEvent::Ended(m) => {
            let _ = app.emit(ENDED_EVENT, MeetingDto::from(&m));
            notify(
                app,
                "Meeting ended",
                &format!("{} released the microphone. Stop transcribing?", m.label()),
            );
        }
        MeetingEvent::Gone => {
            let _ = app.emit(GONE_EVENT, ());
        }
    }
}

fn detected_body(m: &DetectedMeeting) -> String {
    let who = match (&m.service, m.confidence) {
        (Some(service), _) => format!("{} ({service})", m.app),
        (None, Confidence::Low) => format!("A browser ({})", m.app),
        (None, Confidence::High) => m.app.clone(),
    };
    format!("{who} is using the microphone. Open Wisp to start transcribing.")
}

/// Posts a system notification. Clicking it brings Wisp forward (the OS activates the sender).
fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::{NotificationExt, PermissionState};
    let notifications = app.notification();
    if !matches!(
        notifications.permission_state(),
        Ok(PermissionState::Granted)
    ) && !matches!(
        notifications.request_permission(),
        Ok(PermissionState::Granted)
    ) {
        return;
    }
    if let Err(e) = notifications.builder().title(title).body(body).show() {
        eprintln!("wisp: meeting notification failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_match_the_page() {
        // The page listens on these exact strings.
        assert_eq!(DETECTED_EVENT, "meeting://detected");
        assert_eq!(ENDED_EVENT, "meeting://ended");
        assert_eq!(GONE_EVENT, "meeting://gone");
    }

    #[test]
    fn notification_names_the_app_and_service() {
        let meet = DetectedMeeting {
            app: "Chrome".to_owned(),
            service: Some("Google Meet".to_owned()),
            confidence: Confidence::High,
        };
        assert_eq!(
            detected_body(&meet),
            "Chrome (Google Meet) is using the microphone. Open Wisp to start transcribing."
        );
        let low = DetectedMeeting {
            service: None,
            confidence: Confidence::Low,
            ..meet
        };
        assert!(detected_body(&low).starts_with("A browser (Chrome)"));
    }
}

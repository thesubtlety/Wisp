//! The macOS menu bar icon: a status line, Start/Stop, Capture Context, Open Wisp, Quit.
//!
//! Start, Stop, and Capture go through the page (events below), so a session started here behaves
//! exactly like one started from the window (project, title, intelligence). The page reports the
//! running state back with [`set_tray_recording`].
//!
//! The icon is a monochrome template image (`icons/tray-template.png`, 44×44 black on transparent),
//! so macOS tints it for light and dark menu bars.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{App, AppHandle, Emitter, Manager, Wry};

use crate::meeting::MeetingState;

/// Asks the page to start a live session (same as its Start button).
pub(crate) const START_EVENT: &str = "tray://start";
/// Asks the page to stop the live session.
pub(crate) const STOP_EVENT: &str = "tray://stop";
/// Asks the page to capture a screenshot as meeting context.
pub(crate) const CAPTURE_EVENT: &str = "tray://capture";

const ID_TOGGLE: &str = "tray-toggle";
const ID_CAPTURE: &str = "tray-capture";
const ID_OPEN: &str = "tray-open";
const ID_QUIT: &str = "tray-quit";

/// Menu bar title shown next to the icon while recording.
const RECORDING_TITLE: &str = "●";

/// The live tray handles, plus the running state the page reported.
#[derive(Default)]
pub(crate) struct TrayState {
    recording: AtomicBool,
    items: Mutex<Option<TrayItems>>,
}

#[derive(Clone)]
struct TrayItems {
    icon: TrayIcon<Wry>,
    status: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    capture: MenuItem<Wry>,
}

/// Builds the menu bar icon (macOS only; elsewhere the window is the only surface).
pub(crate) fn build(app: &App) -> tauri::Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let status = MenuItem::with_id(app, "tray-status", "Idle", false, None::<&str>)?;
    let toggle = MenuItem::with_id(app, ID_TOGGLE, "Start transcribing", true, None::<&str>)?;
    let capture = MenuItem::with_id(app, ID_CAPTURE, "Capture Context", false, None::<&str>)?;
    let open = MenuItem::with_id(app, ID_OPEN, "Open Wisp", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit Wisp", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &capture,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &quit,
        ],
    )?;
    let icon = TrayIconBuilder::with_id("wisp-tray")
        .icon(tauri::include_image!("icons/tray-template.png"))
        .icon_as_template(true)
        .tooltip("Wisp")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event)
        .build(app)?;
    if let Ok(mut items) = app.state::<TrayState>().items.lock() {
        *items = Some(TrayItems {
            icon,
            status,
            toggle,
            capture,
        });
    }
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        ID_TOGGLE => {
            if app.state::<TrayState>().recording.load(Ordering::SeqCst) {
                let _ = app.emit(STOP_EVENT, ());
            } else {
                show_main_window(app);
                let _ = app.emit(START_EVENT, ());
            }
        }
        ID_CAPTURE => {
            let _ = app.emit(CAPTURE_EVENT, ());
        }
        ID_OPEN => show_main_window(app),
        ID_QUIT => app.exit(0),
        _ => {}
    }
}

/// Shows, unminimizes, and focuses the main window.
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// The page reports whether a live session is running, so the menu offers Start or Stop.
#[tauri::command]
pub(crate) fn set_tray_recording(app: AppHandle, recording: bool) {
    app.state::<TrayState>()
        .recording
        .store(recording, Ordering::SeqCst);
    refresh(&app);
}

/// Redraws the status line, Start/Stop, Capture, and the recording dot from the current state.
pub(crate) fn refresh(app: &AppHandle) {
    let tray = app.state::<TrayState>();
    let recording = tray.recording.load(Ordering::SeqCst);
    let meeting = app.state::<MeetingState>().current();
    // Clone the handles out so no lock is held while the menu updates hop to the main thread.
    let Some(items) = tray.items.lock().ok().and_then(|items| items.clone()) else {
        return;
    };
    let _ = items
        .status
        .set_text(wisp_core::meeting::status_line(recording, meeting.as_ref()));
    let _ = items.toggle.set_text(if recording {
        "Stop transcribing"
    } else {
        "Start transcribing"
    });
    let _ = items.capture.set_enabled(recording);
    let _ = items.icon.set_title(recording.then_some(RECORDING_TITLE));
    let _ = items.icon.set_tooltip(Some(if recording {
        "Wisp — recording"
    } else {
        "Wisp"
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_match_the_page() {
        // The page listens on these exact strings.
        assert_eq!(START_EVENT, "tray://start");
        assert_eq!(STOP_EVENT, "tray://stop");
        assert_eq!(CAPTURE_EVENT, "tray://capture");
    }
}

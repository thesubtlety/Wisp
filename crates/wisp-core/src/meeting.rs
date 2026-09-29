//! Meeting detection logic: decide from a snapshot of "who uses the microphone" whether a meeting
//! is on, and turn a stream of those snapshots into prompts.
//!
//! Pure and platform-free. The app polls the OS (Core Audio process list + window titles), builds
//! [`MicProcess`] / [`WindowInfo`] values, calls [`classify`], and feeds the result to a
//! [`MeetingDetector`] with a timestamp. The detector never starts anything; it only says when to
//! ask the user.

use std::time::Duration;

/// A process that is running microphone input right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicProcess {
    pub pid: i32,
    /// The process's bundle id (may be a helper's, e.g. `com.google.Chrome.helper`); empty if unknown.
    pub bundle_id: String,
}

/// One app window: its owner's bundle id and its title (`None` when the OS withholds titles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub owner_bundle: String,
    pub title: Option<String>,
}

/// How sure the detector is that the mic user is a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// A known meeting app, or a browser with a meeting window open.
    High,
    /// A browser uses the mic, but its window titles are unreadable.
    Low,
}

/// A meeting the detector believes is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedMeeting {
    /// The app holding the mic, as shown to the user (`"Zoom"`, `"Chrome"`).
    pub app: String,
    /// The meeting service when it differs from the app (`"Google Meet"` in Chrome).
    pub service: Option<String>,
    pub confidence: Confidence,
}

impl DetectedMeeting {
    /// The name to show for the meeting: the service if known, else the app.
    pub fn label(&self) -> &str {
        self.service.as_deref().unwrap_or(&self.app)
    }
}

/// Native meeting apps: bundle id (or its prefix, for helper processes) → display name.
const MEETING_APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "Zoom"),
    ("com.microsoft.teams2", "Microsoft Teams"),
    ("com.microsoft.teams", "Microsoft Teams"),
    ("com.cisco.webexmeetingsapp", "Webex"),
    ("com.webex.meetingmanager", "Webex"),
    ("Cisco-Systems.Spark", "Webex"),
    ("com.apple.FaceTime", "FaceTime"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.hnc.Discord", "Discord"),
    ("net.whatsapp.WhatsApp", "WhatsApp"),
    ("com.skype.skype", "Skype"),
];

/// Browsers: the bundle id (or prefix) of the process that holds the mic → the bundle id that owns
/// the windows, and the display name. Safari captures audio in WebKit's GPU process.
const BROWSERS: &[(&str, &str, &str)] = &[
    ("com.google.Chrome", "com.google.Chrome", "Chrome"),
    ("com.apple.Safari", "com.apple.Safari", "Safari"),
    ("com.apple.WebKit.GPU", "com.apple.Safari", "Safari"),
    (
        "company.thebrowser.Browser",
        "company.thebrowser.Browser",
        "Arc",
    ),
    ("com.microsoft.edgemac", "com.microsoft.edgemac", "Edge"),
    ("com.brave.Browser", "com.brave.Browser", "Brave"),
    ("org.mozilla.firefox", "org.mozilla.firefox", "Firefox"),
];

/// Window-title fragments that mark a web meeting → the service name.
const MEETING_TITLES: &[(&str, &str)] = &[
    ("Meet – ", "Google Meet"),
    ("Meet - ", "Google Meet"),
    ("meet.google.com", "Google Meet"),
    ("Google Meet", "Google Meet"),
    ("Zoom", "Zoom"),
    ("Microsoft Teams", "Microsoft Teams"),
    ("Webex", "Webex"),
    ("Whereby", "Whereby"),
    ("Jitsi", "Jitsi Meet"),
];

/// `bundle` is `id` or one of its helpers (`id.helper`, `id.helper.Renderer`), ignoring case.
fn bundle_matches(bundle: &str, id: &str) -> bool {
    let bundle = bundle.to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    bundle == id
        || bundle
            .strip_prefix(&id)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// The meeting service a window title shows, if any.
pub fn meeting_service_in_title(title: &str) -> Option<&'static str> {
    MEETING_TITLES
        .iter()
        .find(|(fragment, _)| title.contains(fragment))
        .map(|(_, service)| *service)
}

/// Decides whether any microphone user is a meeting. A native meeting app wins over a browser; a
/// browser counts only with a meeting window open, or at low confidence when no window titles are
/// readable at all.
/// Other apps using the mic (dictation tools, recorders) are ignored.
pub fn classify(mic: &[MicProcess], windows: &[WindowInfo]) -> Option<DetectedMeeting> {
    let native = mic.iter().find_map(|p| {
        MEETING_APPS
            .iter()
            .find(|(id, _)| bundle_matches(&p.bundle_id, id))
            .map(|(_, name)| DetectedMeeting {
                app: (*name).to_owned(),
                service: None,
                confidence: Confidence::High,
            })
    });
    if native.is_some() {
        return native;
    }
    // Without Screen Recording permission the OS hides every other app's window titles.
    let titles_readable = windows.iter().any(|w| w.title.is_some());
    for p in mic {
        let Some((_, owner, name)) = BROWSERS
            .iter()
            .find(|(id, _, _)| bundle_matches(&p.bundle_id, id))
        else {
            continue;
        };
        let titles: Vec<&str> = windows
            .iter()
            .filter(|w| bundle_matches(&w.owner_bundle, owner))
            .filter_map(|w| w.title.as_deref())
            .collect();
        if let Some(service) = titles.iter().find_map(|t| meeting_service_in_title(t)) {
            return Some(DetectedMeeting {
                app: (*name).to_owned(),
                service: Some(service.to_owned()),
                confidence: Confidence::High,
            });
        }
        if !titles_readable {
            return Some(DetectedMeeting {
                app: (*name).to_owned(),
                service: None,
                confidence: Confidence::Low,
            });
        }
    }
    None
}

/// How long a meeting must hold the mic before the user is asked.
pub const DETECT_AFTER: Duration = Duration::from_secs(4);
/// How long the mic must stay released before a meeting counts as over.
pub const END_AFTER: Duration = Duration::from_secs(10);

/// What the app should do after a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeetingEvent {
    /// A meeting started and Wisp is idle: offer to start transcribing (once per meeting).
    Detected(DetectedMeeting),
    /// The meeting Wisp is recording ended: offer to stop (never stop by itself).
    Ended(DetectedMeeting),
    /// A detected meeting went away while Wisp was idle: clear the offer.
    Gone,
}

/// Turns timed snapshots into [`MeetingEvent`]s: debounces a new meeting for [`DETECT_AFTER`],
/// prompts once per episode, and ends the episode after [`END_AFTER`] without a meeting.
#[derive(Debug, Default)]
pub struct MeetingDetector {
    /// When the current candidate meeting was first seen (continuously since then).
    seen_since: Option<Duration>,
    /// The confirmed meeting of this episode, once it held the mic for [`DETECT_AFTER`].
    confirmed: Option<DetectedMeeting>,
    /// When the confirmed meeting was last seen missing (start of the grace period).
    missing_since: Option<Duration>,
}

impl MeetingDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The meeting of the current episode, once confirmed.
    pub fn current(&self) -> Option<&DetectedMeeting> {
        self.confirmed.as_ref()
    }

    /// Forget everything (detection turned off).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds one snapshot taken at `now` (any monotonic clock). `recording` is whether Wisp is
    /// transcribing right now.
    pub fn observe(
        &mut self,
        now: Duration,
        meeting: Option<DetectedMeeting>,
        recording: bool,
    ) -> Option<MeetingEvent> {
        match meeting {
            Some(meeting) => self.on_present(now, meeting, recording),
            None => self.on_absent(now, recording),
        }
    }

    fn on_present(
        &mut self,
        now: Duration,
        meeting: DetectedMeeting,
        recording: bool,
    ) -> Option<MeetingEvent> {
        self.missing_since = None;
        if let Some(confirmed) = &mut self.confirmed {
            // Same episode; keep the freshest description (e.g. the service became known).
            *confirmed = meeting;
            return None;
        }
        let since = *self.seen_since.get_or_insert(now);
        if now.saturating_sub(since) < DETECT_AFTER {
            return None;
        }
        self.confirmed = Some(meeting.clone());
        // Already recording: the episode is tracked (for the end offer) but never prompted.
        (!recording).then_some(MeetingEvent::Detected(meeting))
    }

    fn on_absent(&mut self, now: Duration, recording: bool) -> Option<MeetingEvent> {
        let Some(confirmed) = &self.confirmed else {
            // A blip shorter than the debounce: start over.
            self.seen_since = None;
            return None;
        };
        let missing = *self.missing_since.get_or_insert(now);
        if now.saturating_sub(missing) < END_AFTER {
            return None;
        }
        let ended = confirmed.clone();
        self.reset();
        Some(if recording {
            MeetingEvent::Ended(ended)
        } else {
            MeetingEvent::Gone
        })
    }
}

/// The tray's status line.
pub fn status_line(recording: bool, meeting: Option<&DetectedMeeting>) -> String {
    match (recording, meeting) {
        (true, _) => "Recording".to_owned(),
        (false, Some(m)) => format!("Meeting detected: {}", m.label()),
        (false, None) => "Idle".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic(bundle: &str) -> MicProcess {
        MicProcess {
            pid: 42,
            bundle_id: bundle.to_owned(),
        }
    }

    fn window(owner: &str, title: Option<&str>) -> WindowInfo {
        WindowInfo {
            owner_bundle: owner.to_owned(),
            title: title.map(str::to_owned),
        }
    }

    fn zoom() -> DetectedMeeting {
        DetectedMeeting {
            app: "Zoom".to_owned(),
            service: None,
            confidence: Confidence::High,
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn native_meeting_apps_and_their_helpers_are_meetings() {
        for bundle in [
            "us.zoom.xos",
            "com.microsoft.teams2",
            "com.microsoft.teams2.helper",
            "com.apple.FaceTime",
            "com.hnc.Discord.helper",
        ] {
            let got = classify(&[mic(bundle)], &[]).expect(bundle);
            assert_eq!(got.confidence, Confidence::High, "{bundle}");
        }
        assert_eq!(classify(&[mic("us.zoom.xos")], &[]).unwrap().app, "Zoom");
    }

    #[test]
    fn prefix_must_end_at_a_dot() {
        // "com.microsoft.teamsX" is not Teams, and a dictation tool is not a meeting.
        assert_eq!(classify(&[mic("com.microsoft.teamsx")], &[]), None);
        assert_eq!(classify(&[mic("com.example.dictate")], &[]), None);
        assert_eq!(classify(&[mic("")], &[]), None);
    }

    #[test]
    fn browser_counts_only_with_a_meeting_window() {
        let procs = [mic("com.google.Chrome.helper")];
        let meet = [
            window("com.google.Chrome", Some("Inbox - Gmail")),
            window("com.google.Chrome", Some("Meet – abc-defg-hij")),
        ];
        let got = classify(&procs, &meet).unwrap();
        assert_eq!(got.app, "Chrome");
        assert_eq!(got.service.as_deref(), Some("Google Meet"));
        assert_eq!(got.label(), "Google Meet");
        assert_eq!(got.confidence, Confidence::High);

        let no_meeting = [window("com.google.Chrome", Some("YouTube"))];
        assert_eq!(classify(&procs, &no_meeting), None);

        // A meeting title in a different app's window does not count for Chrome.
        let other_app = [window("com.apple.Safari", Some("Meet – abc"))];
        assert_eq!(classify(&procs, &other_app), None);
    }

    #[test]
    fn browser_without_readable_titles_is_low_confidence() {
        let got = classify(
            &[mic("com.apple.WebKit.GPU")],
            &[window("com.apple.Safari", None)],
        )
        .unwrap();
        assert_eq!(got.app, "Safari");
        assert_eq!(got.service, None);
        assert_eq!(got.confidence, Confidence::Low);
        // No windows at all is the same: nothing to read.

        let got = classify(&[mic("org.mozilla.firefox")], &[]).unwrap();
        assert_eq!(got.confidence, Confidence::Low);
    }

    #[test]
    fn native_app_beats_browser() {
        let got = classify(
            &[mic("com.google.Chrome.helper"), mic("us.zoom.xos")],
            &[window("com.google.Chrome", None)],
        )
        .unwrap();
        assert_eq!(got.app, "Zoom");
    }

    #[test]
    fn title_patterns() {
        assert_eq!(
            meeting_service_in_title("Meet - xyz - Google Chrome"),
            Some("Google Meet")
        );
        assert_eq!(
            meeting_service_in_title("https://meet.google.com/abc"),
            Some("Google Meet")
        );
        assert_eq!(meeting_service_in_title("Zoom Meeting"), Some("Zoom"));
        assert_eq!(
            meeting_service_in_title("Chat | Microsoft Teams"),
            Some("Microsoft Teams")
        );
        assert_eq!(meeting_service_in_title("Whereby"), Some("Whereby"));
        assert_eq!(meeting_service_in_title("Jitsi Meet"), Some("Jitsi Meet"));
        assert_eq!(meeting_service_in_title("Weekly notes"), None);
    }

    #[test]
    fn prompts_once_after_the_debounce() {
        let mut d = MeetingDetector::new();
        assert_eq!(d.observe(secs(0), Some(zoom()), false), None);
        assert_eq!(d.observe(secs(2), Some(zoom()), false), None);
        assert_eq!(
            d.observe(secs(4), Some(zoom()), false),
            Some(MeetingEvent::Detected(zoom()))
        );
        assert_eq!(d.current(), Some(&zoom()));
        for t in 5..60 {
            assert_eq!(d.observe(secs(t), Some(zoom()), false), None);
        }
    }

    #[test]
    fn a_short_blip_never_prompts() {
        let mut d = MeetingDetector::new();
        d.observe(secs(0), Some(zoom()), false);
        d.observe(secs(2), None, false);
        // The clock restarts after the gap.
        assert_eq!(d.observe(secs(4), Some(zoom()), false), None);
        assert_eq!(d.observe(secs(6), Some(zoom()), false), None);
        assert_eq!(
            d.observe(secs(8), Some(zoom()), false),
            Some(MeetingEvent::Detected(zoom()))
        );
    }

    #[test]
    fn never_prompts_while_recording_and_not_later_in_the_same_episode() {
        let mut d = MeetingDetector::new();
        d.observe(secs(0), Some(zoom()), true);
        assert_eq!(d.observe(secs(5), Some(zoom()), true), None);
        // The user stops Wisp mid-meeting: no prompt for this same meeting.
        assert_eq!(d.observe(secs(7), Some(zoom()), false), None);
    }

    #[test]
    fn rearms_after_the_meeting_releases_the_mic() {
        let mut d = MeetingDetector::new();
        d.observe(secs(0), Some(zoom()), false);
        d.observe(secs(4), Some(zoom()), false);
        // A short release inside the grace period keeps the episode.
        assert_eq!(d.observe(secs(6), None, false), None);
        assert_eq!(d.observe(secs(8), Some(zoom()), false), None);
        // A long release ends it.
        assert_eq!(d.observe(secs(10), None, false), None);
        assert_eq!(d.observe(secs(20), None, false), Some(MeetingEvent::Gone));
        assert_eq!(d.current(), None);
        // The next meeting prompts again.
        d.observe(secs(30), Some(zoom()), false);
        assert_eq!(
            d.observe(secs(34), Some(zoom()), false),
            Some(MeetingEvent::Detected(zoom()))
        );
    }

    #[test]
    fn offers_to_stop_when_the_recorded_meeting_ends() {
        let mut d = MeetingDetector::new();
        d.observe(secs(0), Some(zoom()), false);
        d.observe(secs(4), Some(zoom()), false);
        // The user accepted; Wisp records.
        d.observe(secs(6), Some(zoom()), true);
        assert_eq!(d.observe(secs(8), None, true), None);
        assert_eq!(d.observe(secs(17), None, true), None);
        assert_eq!(
            d.observe(secs(18), None, true),
            Some(MeetingEvent::Ended(zoom()))
        );
        // Offered once.
        assert_eq!(d.observe(secs(40), None, true), None);
    }

    #[test]
    fn keeps_the_freshest_description() {
        let mut d = MeetingDetector::new();
        let low = DetectedMeeting {
            app: "Chrome".to_owned(),
            service: None,
            confidence: Confidence::Low,
        };
        let meet = DetectedMeeting {
            service: Some("Google Meet".to_owned()),
            confidence: Confidence::High,
            ..low.clone()
        };
        d.observe(secs(0), Some(low.clone()), false);
        d.observe(secs(4), Some(low), false);
        d.observe(secs(6), Some(meet.clone()), false);
        assert_eq!(d.current(), Some(&meet));
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_line(false, None), "Idle");
        assert_eq!(status_line(true, Some(&zoom())), "Recording");
        let meet = DetectedMeeting {
            app: "Chrome".to_owned(),
            service: Some("Google Meet".to_owned()),
            confidence: Confidence::High,
        };
        assert_eq!(
            status_line(false, Some(&meet)),
            "Meeting detected: Google Meet"
        );
    }
}

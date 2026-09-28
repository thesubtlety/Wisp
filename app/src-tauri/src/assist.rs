//! The AI assist, kept apart from the app root. The **chat** assist ([`chat`]) runs a prompt over a
//! transcript the UI hands it — once, or streamed into the feed, map-reduced when the transcript
//! would overflow the model's context. The **realtime** assist ([`realtime`]) listens to the live
//! meeting audio through an OpenAI realtime model. Both reply over the `assist://` events below.
//!
//! The rest of the app reaches the assist through the Tauri commands registered in `run()` and a
//! few live-session hooks: [`AssistTaps`] taps each processed stream, [`store_assist_mix`] parks
//! the mix, [`route_assist_final`] feeds each committed final, and [`take_assist_teardown`] lifts
//! it all out on Stop. Cloud endpoints keep their chat tuning as [`AssistParams`].

pub(crate) mod chat;
pub(crate) mod realtime;

pub(crate) use chat::{normalize_assist, AssistParams};
pub(crate) use realtime::{
    route_assist_final, store_assist_mix, take_assist_teardown, AssistState, AssistTaps,
    AssistTeardown,
};

/// Event channel the UI listens on for realtime AI-assist responses — one finalised reply per turn,
/// payload is the response text. A reply that streamed in via [`ASSIST_DELTA_EVENT`] is closed by this.
const ASSIST_TEXT_EVENT: &str = "assist://text";

/// Event channel for an incremental chunk of the in-progress assist reply (payload is the new text to
/// append) — so a reply streams into the feed as it generates instead of popping in whole at the end.
const ASSIST_DELTA_EVENT: &str = "assist://delta";

/// Event channel for a realtime AI-assist error (bad key/model, server error, dropped socket) — kept
/// separate from [`LIVE_ERROR_EVENT`] so the assist pane shows its own failures, not the transcript's.
///
/// [`LIVE_ERROR_EVENT`]: crate::LIVE_ERROR_EVENT
const ASSIST_ERROR_EVENT: &str = "assist://error";

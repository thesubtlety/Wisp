//! Model-independent reasoning for Wisp.
//!
//! A caller builds a [`ReasoningRequest`] with an explicit context and a JSON Schema for the
//! output. A [`ReasoningBackend`] turns it into JSON that satisfies that schema, or an error.
//! Nothing here knows about Tauri, audio, or the UI.
//!
//! [`CodexCliBackend`] runs the user's installed, logged-in `codex` CLI once per request, in a
//! fresh temp directory that holds only that request. In subscription mode it drops API-key
//! environment variables so a stray key can't move the run onto API billing.
//! [`ClaudeCodeBackend`] does the same with `claude -p`: no tools, bounded turns, user settings
//! ignored. [`FallbackBackend`] tries backends in order (Codex first, then Claude, say).
//! [`ScriptedBackend`] is a deterministic stand-in for tests. The CLIs are found on PATH widened to
//! the usual install locations, since a Finder-launched app inherits almost none.

mod backend;
mod claude;
mod codex;
mod fallback;
mod json;
mod locate;
mod runner;
mod schema;
mod scripted;
mod workspace;

pub use backend::{
    render_prompt, CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError,
    ReasoningRequest, ReasoningResponse, TaskKind,
};
pub use claude::{ClaudeCodeBackend, ClaudeConfig};
pub use codex::{CodexCliBackend, CodexConfig};
pub use fallback::FallbackBackend;
pub use json::extract_json_object;
pub use locate::{find_program, search_dirs};
pub use runner::{run_command, CommandSpec, RunOutput, SUBSCRIPTION_STRIPPED_ENV};
pub use schema::validate;
pub use scripted::ScriptedBackend;
pub use workspace::Workspace;

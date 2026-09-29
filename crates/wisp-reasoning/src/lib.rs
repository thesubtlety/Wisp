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
//! ignored. [`OpenAiCompatBackend`] talks to a model behind an OpenAI-compatible chat endpoint,
//! usually a local one (Ollama, llama.cpp, LM Studio). [`FallbackBackend`] tries backends in order
//! (Codex first, then Claude, say); [`TaskRouter`] sends each task kind to its own backend.
//! [`AuditingBackend`] records every call it passes on. [`ScriptedBackend`] is a deterministic
//! stand-in for tests. The CLIs are found on PATH widened to
//! the usual install locations, since a Finder-launched app inherits almost none.

mod audit;
mod backend;
mod claude;
mod codex;
mod fallback;
mod json;
mod local;
mod locate;
mod routed;
mod runner;
mod schema;
mod scripted;
mod workspace;

pub use audit::{
    audited, estimate_tokens, now_ms, task_label, AuditRecord, AuditSink, AuditingBackend, CallInfo,
};
pub use backend::{
    check_images, image_media_type, render_prompt, CancelToken, Capabilities, Health,
    ReasoningBackend, ReasoningError, ReasoningRequest, ReasoningResponse, TaskKind, TokenUsage,
    MAX_IMAGE_BYTES,
};
pub use claude::{ClaudeCodeBackend, ClaudeConfig};
pub use codex::{CodexCliBackend, CodexConfig};
pub use fallback::FallbackBackend;
pub use json::extract_json_object;
pub use local::{is_loopback, LocalConfig, OpenAiCompatBackend};
pub use locate::{find_program, search_dirs};
pub use routed::TaskRouter;
pub use runner::{run_command, CommandSpec, RunOutput, SUBSCRIPTION_STRIPPED_ENV};
pub use schema::{for_model, validate, OPTIONAL_MARK};
pub use scripted::ScriptedBackend;
pub use workspace::Workspace;

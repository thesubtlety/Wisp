//! The chat assist: a one-shot or streamed chat-completion task (summary, action items, or a custom
//! prompt) over a transcript the UI hands in, against any provider that speaks OpenAI-compatible
//! `/chat/completions` — a catalog provider or a user's custom endpoint. A transcript longer than
//! the endpoint's context window is map-reduced rather than truncated.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use wisp_core::cloud::CloudProvider;
use wisp_core::params::ParamValues;
use wisp_engine_cloud::{
    assist_param_specs, chat_completion_stream, chat_completion_with_usage, ChatRequest,
};
use wisp_reasoning::{
    audited, is_loopback, task_label, AuditSink, CallInfo, CancelToken, ReasoningBackend,
    ReasoningRequest, TaskKind, TokenUsage,
};

use super::{ASSIST_DELTA_EVENT, ASSIST_TEXT_EVENT};
use crate::{build_param_values, param_spec_dto, resolve_cloud_provider, AppState, ParamSpecDto};

/// The assist provider id that runs on Settings › Reasoning (the user's Codex / Claude Code
/// subscription, or the local model) instead of an API key.
pub(crate) const SUBSCRIPTION_PROVIDER: &str = "subscription";

/// The subscription backends take far more than this; it keeps a normal meeting in one call and
/// map-reduces only a very long one.
const SUBSCRIPTION_CONTEXT_TOKENS: u32 = 200_000;

/// The window assumed for a local-only model whose endpoint sets none: small, so a long meeting
/// map-reduces rather than overflowing it.
const LOCAL_CONTEXT_TOKENS: u32 = 8_192;

/// How long one subscription assist call may run (a CLI call takes seconds to a minute).
const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Where an assist call goes.
enum AssistTarget {
    /// An OpenAI-compatible `/chat/completions` endpoint.
    Http {
        provider: Box<CloudProvider>,
        model: String,
        key: String,
        /// Where each call is logged.
        audit: AuditSink,
    },
    /// The reasoning backend from Settings › Reasoning. Replies arrive whole, not streamed.
    Subscription(Arc<dyn ReasoningBackend>),
}

/// The AI notes/assist tuning a custom endpoint carries (its chat model's knobs). All optional —
/// an empty/`None` field falls back to a built-in default and is never sent to the provider. Used
/// only by [`run_llm_task`]; transcription has its own per-model parameter panel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssistParams {
    /// Sampling temperature for the chat model (omitted from the request when unset, so a model that
    /// only accepts its default temperature — e.g. a reasoning model — isn't sent one).
    #[serde(default)]
    temperature: Option<f64>,
    /// Cap on the reply length, in tokens (omitted from the request when unset).
    #[serde(default)]
    max_tokens: Option<u32>,
    /// The model's context window, in tokens. When set and a transcript would exceed it, the assist
    /// runs map-reduce (summarize chunks, then combine) instead of one over-long request.
    #[serde(default)]
    pub(crate) context_tokens: Option<u32>,
    /// Nucleus sampling cutoff (omitted from the request when unset).
    #[serde(default)]
    top_p: Option<f64>,
    /// Repetition penalty: positive discourages reusing the same words (omitted when unset).
    #[serde(default)]
    frequency_penalty: Option<f64>,
    /// Topic-novelty penalty: positive pushes toward new subjects (omitted when unset).
    #[serde(default)]
    presence_penalty: Option<f64>,
    /// A standing instruction prepended to every assist task on this endpoint (persona, language,
    /// style). Empty for none.
    #[serde(default)]
    system_prompt: String,
}

/// Clamps assist params to sane ranges (and drops zero token caps to "unset") so a hand-edited or
/// stale value can never reach the provider as something invalid.
pub(crate) fn normalize_assist(mut assist: AssistParams) -> AssistParams {
    assist.temperature = assist.temperature.map(|t| t.clamp(0.0, 2.0));
    assist.top_p = assist.top_p.map(|p| p.clamp(0.0, 1.0));
    assist.frequency_penalty = assist.frequency_penalty.map(|p| p.clamp(-2.0, 2.0));
    assist.presence_penalty = assist.presence_penalty.map(|p| p.clamp(-2.0, 2.0));
    assist.max_tokens = assist.max_tokens.filter(|&n| n > 0);
    assist.context_tokens = assist.context_tokens.filter(|&n| n > 0);
    assist.system_prompt = assist.system_prompt.trim().to_owned();
    assist
}

/// The advanced parameter specs the chat assist exposes (temperature / top_p / max reply tokens), for
/// the generic settings panel — the assist-side counterpart of [`batch_params`]. Vendor-agnostic (every
/// assist provider speaks the same OpenAI-compatible chat tuning), so it takes no provider/model.
///
/// [`batch_params`]: crate::batch_params
#[tauri::command]
pub(crate) fn assist_params() -> Vec<ParamSpecDto> {
    assist_param_specs().iter().map(param_spec_dto).collect()
}

/// Runs a one-shot LLM task (summary, action items, or a custom prompt) over `transcript` using the
/// chat model of cloud `provider` — typically a user's custom OpenAI-compatible endpoint (their
/// gateway, a local Ollama, …). Runs off the main thread (the call is a slow HTTP round-trip).
#[tauri::command]
pub(crate) async fn run_llm_task(
    app: AppHandle,
    provider: String,
    model: String,
    system_prompt: String,
    transcript: String,
    params: HashMap<String, serde_json::Value>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_llm_task_blocking(app, &provider, &model, &system_prompt, &transcript, &params)
    })
    .await
    .map_err(|e| format!("LLM task failed: {e}"))?
}

/// The blocking body of [`run_llm_task`]: resolve the provider (catalog or custom endpoint) and its
/// key, then call the chat model with the task's system prompt and the transcript.
fn run_llm_task_blocking(
    app: AppHandle,
    provider_id: &str,
    model: &str,
    system_prompt: &str,
    transcript: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<String, String> {
    if transcript.trim().is_empty() {
        return Err("there's no transcript to work on yet".to_owned());
    }

    let state = app.state::<AppState>();
    let (target, assist) = resolve_assist_target(&state, provider_id, model)?;
    let assist = overlay_assist_params(assist, &build_param_values(&assist_param_specs(), params));

    // Prepend the endpoint's standing instruction (persona / language / style) to the task prompt.
    let system = combine_system(&assist.system_prompt, system_prompt);

    run_assist(&target, &system, transcript, &assist)
}

/// Resolves where an assist call goes and its tuning — the common preamble for every assist call.
/// [`SUBSCRIPTION_PROVIDER`] goes to the reasoning backend. Otherwise it's the cloud provider
/// (catalog or custom endpoint) with its on-device key: a catalog provider uses default tuning; a
/// custom endpoint carries its own (temperature, context size, system prompt, …).
fn resolve_assist_target(
    state: &AppState,
    provider_id: &str,
    model: &str,
) -> Result<(AssistTarget, AssistParams), String> {
    if provider_id == SUBSCRIPTION_PROVIDER {
        let context_tokens = match crate::reasoning::local_only_context(state) {
            Some(window) => window.unwrap_or(LOCAL_CONTEXT_TOKENS),
            None => SUBSCRIPTION_CONTEXT_TOKENS,
        };
        let assist = AssistParams {
            context_tokens: Some(context_tokens),
            ..AssistParams::default()
        };
        return Ok((
            AssistTarget::Subscription(crate::reasoning::backend(state)),
            assist,
        ));
    }

    let endpoints = state
        .cloud_custom_endpoints
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?;

    let provider = resolve_cloud_provider(provider_id, &endpoints)
        .ok_or_else(|| format!("unknown provider {provider_id}"))?;

    let saved = state
        .cloud_keys
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())?
        .get(provider_id)
        .cloned();
    let key = assist_key(&provider, saved)?;

    let assist = endpoints
        .iter()
        .find(|e| e.id == provider_id)
        .map(|e| e.assist.clone())
        .unwrap_or_default();

    let target = AssistTarget::Http {
        provider: Box::new(provider),
        model: model.to_owned(),
        key,
        audit: crate::audit::sink(state, crate::audit::live_meeting(state)),
    };
    Ok((target, assist))
}

/// The key to send: the saved one, or none for an endpoint on this machine (Ollama, LM Studio),
/// which needs none. Any other endpoint without a saved key is an error.
fn assist_key(provider: &CloudProvider, saved: Option<String>) -> Result<String, String> {
    match saved.filter(|k| !k.trim().is_empty()) {
        Some(key) => Ok(key),
        None if is_loopback(&provider.base_url) => Ok(String::new()),
        None => Err(format!("no API key saved for {}", provider.id)),
    }
}

/// Overlays the user's advanced assist params (from the settings panel) onto the resolved endpoint
/// tuning: a knob the user moved off its default is present in `params` and overrides; one left at its
/// default isn't present, so the endpoint's own value (often "unset" → the model's own optimum) stands.
/// Each override is clamped here, since these bypass the save-time [`normalize_assist`]. A `max_tokens`
/// of 0 means "no cap" (its default), so it's treated as unset.
fn overlay_assist_params(mut assist: AssistParams, params: &ParamValues) -> AssistParams {
    if params.contains("temperature") {
        assist.temperature = Some(params.float("temperature", 1.0).clamp(0.0, 2.0));
    }

    if params.contains("top_p") {
        assist.top_p = Some(params.float("top_p", 1.0).clamp(0.0, 1.0));
    }

    if params.contains("frequency_penalty") {
        assist.frequency_penalty = Some(params.float("frequency_penalty", 0.0).clamp(-2.0, 2.0));
    }

    if params.contains("presence_penalty") {
        assist.presence_penalty = Some(params.float("presence_penalty", 0.0).clamp(-2.0, 2.0));
    }

    if params.contains("max_tokens") {
        let n = params.int("max_tokens", 0).clamp(0, i64::from(u32::MAX));
        if n > 0 {
            assist.max_tokens = Some(n as u32);
        }
    }

    assist
}

/// Streams a chat assist task into the feed: resolve the provider, then run a single chat call with
/// `stream: true`, emitting each chunk as [`ASSIST_DELTA_EVENT`] and the full reply as
/// [`ASSIST_TEXT_EVENT`]. A transcript too long for one call falls back to (non-streamed) map-reduce and
/// emits the combined result whole. Off the main thread — the call is a slow streamed round-trip.
#[tauri::command]
pub(crate) async fn run_assist_stream(
    app: AppHandle,
    provider: String,
    model: String,
    system_prompt: String,
    transcript: String,
    params: HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_assist_stream_blocking(app, &provider, &model, &system_prompt, &transcript, &params)
    })
    .await
    .map_err(|e| format!("assist stream task failed: {e}"))?
}

fn run_assist_stream_blocking(
    app: AppHandle,
    provider_id: &str,
    model: &str,
    system_prompt: &str,
    transcript: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    if transcript.trim().is_empty() {
        return Err("there's no transcript to work on yet".to_owned());
    }

    let state = app.state::<AppState>();
    let (target, assist) = resolve_assist_target(&state, provider_id, model)?;
    let assist = overlay_assist_params(assist, &build_param_values(&assist_param_specs(), params));

    let system = combine_system(&assist.system_prompt, system_prompt);

    // A transcript that fits one call streams token-by-token; one too long map-reduces (each chunk
    // whole, no per-token stream) and the combined result is emitted as the single final reply. The
    // subscription backends don't stream, so their reply is emitted whole too.
    let text = match (&target, input_char_budget(assist.context_tokens)) {
        (_, Some(budget)) if transcript.chars().count() > budget => {
            map_reduce_assist(&target, &system, transcript, &assist, budget)?
        }
        (AssistTarget::Subscription(_), _) => chat_once(&target, &system, transcript, &assist)?,
        (
            AssistTarget::Http {
                provider,
                model,
                key,
                audit,
            },
            _,
        ) => {
            let app_delta = app.clone();
            let req = ChatRequest {
                system: &system,
                user: transcript,
                temperature: assist.temperature,
                max_tokens: assist.max_tokens,
                top_p: assist.top_p,
                frequency_penalty: assist.frequency_penalty,
                presence_penalty: assist.presence_penalty,
            };
            // A stream reports no usage, so its tokens are estimated.
            audited(
                audit,
                call_info(provider, model, &system, transcript),
                || {
                    chat_completion_stream(provider, model, key, &req, |chunk| {
                        let _ = app_delta.emit(ASSIST_DELTA_EVENT, chunk.to_owned());
                    })
                },
                |text| (text.clone(), None),
            )
            .map_err(|e| e.to_string())?
        }
    };

    let _ = app.emit(ASSIST_TEXT_EVENT, text.trim().to_owned());
    Ok(())
}

/// Approximate characters per token — used only to decide when a transcript needs map-reduce. A
/// rough cross-language middle (English ~4, CJK ~1–2); 3 errs toward chunking, which is the safe way
/// to be wrong (an extra round-trip beats overflowing the model's context).
const ASSIST_CHARS_PER_TOKEN: usize = 3;

/// The character budget for one assist request given a context window in tokens, or `None` when no
/// window is set (0 counts as unset). Reserves ~40% of the window for the system prompt and reply.
fn input_char_budget(context_tokens: Option<u32>) -> Option<usize> {
    context_tokens
        .filter(|&t| t > 0)
        .map(|t| (t as usize * ASSIST_CHARS_PER_TOKEN * 3) / 5)
}

/// Splits `text` into chunks of at most `budget` characters, breaking only at line boundaries (one
/// transcript turn per line) so an utterance is never cut mid-sentence. A single over-long line
/// becomes its own chunk.
fn chunk_by_chars(text: &str, budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();

    for line in text.lines() {
        if !cur.is_empty() && cur.chars().count() + 1 + line.chars().count() > budget {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }

    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Prepends an endpoint's standing instruction to a task's system prompt (blank-safe).
fn combine_system(standing: &str, task: &str) -> String {
    let standing = standing.trim();

    if standing.is_empty() {
        task.to_owned()
    } else {
        format!("{standing}\n\n{task}")
    }
}

/// Runs an assist task, transparently map-reducing when `context_tokens` is set and the transcript
/// would overflow it: split into chunks, run the task on each, then combine the partial results.
fn run_assist(
    target: &AssistTarget,
    system: &str,
    transcript: &str,
    assist: &AssistParams,
) -> Result<String, String> {
    match input_char_budget(assist.context_tokens) {
        Some(budget) if transcript.chars().count() > budget => {
            map_reduce_assist(target, system, transcript, assist, budget)
        }
        _ => chat_once(target, system, transcript, assist),
    }
}

/// One assist call. Over HTTP it carries the endpoint's tuning — temperature / max_tokens / top_p are
/// each sent only when set, so a model that rejects a non-default temperature isn't sent one. The
/// subscription backends take no tuning.
fn chat_once(
    target: &AssistTarget,
    system: &str,
    user: &str,
    assist: &AssistParams,
) -> Result<String, String> {
    let (provider, model, key, audit) = match target {
        AssistTarget::Http {
            provider,
            model,
            key,
            audit,
        } => (provider, model, key, audit),
        AssistTarget::Subscription(backend) => {
            return subscription_once(backend.as_ref(), system, user)
        }
    };
    let req = ChatRequest {
        system,
        user,
        temperature: assist.temperature,
        max_tokens: assist.max_tokens,
        top_p: assist.top_p,
        frequency_penalty: assist.frequency_penalty,
        presence_penalty: assist.presence_penalty,
    };
    audited(
        audit,
        call_info(provider, model, system, user),
        || chat_completion_with_usage(provider, model, key, &req),
        |(text, usage)| {
            let usage = usage.map(|u| TokenUsage {
                input: u.prompt_tokens,
                output: u.completion_tokens,
            });
            (text.clone(), usage)
        },
    )
    .map(|(text, _)| text)
    .map_err(|e| e.to_string())
}

/// What the activity log records about one assist call over HTTP. The key is never part of it.
fn call_info(provider: &CloudProvider, model: &str, system: &str, user: &str) -> CallInfo {
    CallInfo {
        task: task_label(TaskKind::Assist),
        backend: provider.display_name.clone(),
        model: Some(model.to_owned()),
        local: is_loopback(&provider.base_url),
        instructions: system.to_owned(),
        context: user.to_owned(),
        images: Vec::new(),
    }
}

/// Map-reduce for a transcript that exceeds the context window: run the task on each chunk (map),
/// then combine the partials under the original instruction (reduce). Covers the whole transcript
/// rather than truncating it.
fn map_reduce_assist(
    target: &AssistTarget,
    system: &str,
    transcript: &str,
    assist: &AssistParams,
    budget: usize,
) -> Result<String, String> {
    let chunks = chunk_by_chars(transcript, budget);

    let mut partials = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let part = chat_once(target, system, chunk, assist)?;
        partials.push(format!(
            "=== Part {}/{} ===\n{}",
            i + 1,
            chunks.len(),
            part.trim()
        ));
    }

    let reduce_system = format!(
        "Below are results from running the same instruction on consecutive parts of one long \
         transcript. Combine them into a single coherent result that follows this instruction, \
         merging duplicates and keeping it faithful:\n\n{system}"
    );

    chat_once(target, &reduce_system, &partials.join("\n\n"), assist)
}

/// One assist call on the reasoning backend: the task prompt as instructions, the transcript as
/// context, and the reply as a single `text` field.
fn subscription_once(
    backend: &dyn ReasoningBackend,
    system: &str,
    transcript: &str,
) -> Result<String, String> {
    let resp = backend
        .invoke(&assist_request(system, transcript), &CancelToken::new())
        .map_err(|e| e.to_string())?;
    assist_text(&resp.output)
}

fn assist_request(system: &str, transcript: &str) -> ReasoningRequest {
    ReasoningRequest {
        task: TaskKind::Assist,
        instructions: system.to_owned(),
        context: transcript.to_owned(),
        output_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["text"],
            "properties": {"text": {"type": "string"}}
        }),
        timeout: SUBSCRIPTION_TIMEOUT,
        images: Vec::new(),
    }
}

/// The reply text from a validated `{ "text": … }` output. An empty reply is an error, so the feed
/// never shows a blank answer.
fn assist_text(output: &serde_json::Value) -> Result<String, String> {
    output
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the assist returned no text".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_core::params::ParamValue;

    #[test]
    fn assist_helpers_budget_chunk_combine_and_normalize() {
        // No window (or 0) disables map-reduce; a window reserves headroom (3 chars/token × 3/5).
        assert_eq!(input_char_budget(None), None);
        assert_eq!(input_char_budget(Some(0)), None);
        assert_eq!(input_char_budget(Some(1000)), Some(1800));

        // Chunking breaks only at line boundaries, stays under budget, and never drops content.
        let text = "aaaa\nbbbb\ncccc\ndddd";
        let chunks = chunk_by_chars(text, 9);
        assert!(chunks.iter().all(|c| c.chars().count() <= 9));
        assert_eq!(chunks.join("\n"), text);

        // A single over-long line becomes its own chunk rather than being split mid-line.
        let long = "x".repeat(50);
        assert_eq!(chunk_by_chars(&long, 10).len(), 1);

        // The standing instruction is prepended, blank-safe.
        assert_eq!(combine_system("  ", "task"), "task");
        assert_eq!(
            combine_system("Reply in Cantonese.", "Summarize."),
            "Reply in Cantonese.\n\nSummarize."
        );

        // Normalize clamps ranges and drops zero token caps to "unset".
        let n = normalize_assist(AssistParams {
            temperature: Some(5.0),
            max_tokens: Some(0),
            context_tokens: Some(8000),
            top_p: Some(-1.0),
            frequency_penalty: Some(3.0),
            presence_penalty: Some(-3.0),
            system_prompt: "  hi  ".to_owned(),
        });
        assert_eq!(n.temperature, Some(2.0));
        assert_eq!(n.top_p, Some(0.0));
        assert_eq!(n.frequency_penalty, Some(2.0));
        assert_eq!(n.presence_penalty, Some(-2.0));
        assert_eq!(n.max_tokens, None);
        assert_eq!(n.context_tokens, Some(8000));
        assert_eq!(n.system_prompt, "hi");
    }

    #[test]
    fn overlay_assist_params_applies_only_set_knobs_and_clamps() {
        let base = AssistParams {
            temperature: None,
            max_tokens: None,
            context_tokens: Some(8000),
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            system_prompt: "persona".to_owned(),
        };

        // A knob the user set overrides; unset knobs leave the resolved value untouched.
        let mut set = ParamValues::new();
        set.set("temperature", ParamValue::Float(0.3));
        let out = overlay_assist_params(base.clone(), &set);
        assert_eq!(out.temperature, Some(0.3), "set temperature overrides");
        assert_eq!(out.top_p, None, "unset top_p stays unset");
        assert_eq!(out.presence_penalty, None, "unset penalty stays unset");
        assert_eq!(out.context_tokens, Some(8000), "untouched fields preserved");
        assert_eq!(out.system_prompt, "persona");

        // Empty overrides leave every field as resolved — so an unset temperature is still omitted.
        let untouched = overlay_assist_params(base.clone(), &ParamValues::new());
        assert_eq!(untouched.temperature, None);

        // Overrides bypass the save-time normaliser, so the overlay clamps them itself.
        let mut wild = ParamValues::new();
        wild.set("temperature", ParamValue::Float(5.0));
        wild.set("top_p", ParamValue::Float(2.0));
        wild.set("frequency_penalty", ParamValue::Float(9.0));
        wild.set("presence_penalty", ParamValue::Float(-9.0));
        wild.set("max_tokens", ParamValue::Int(512));
        let clamped = overlay_assist_params(base.clone(), &wild);
        assert_eq!(clamped.temperature, Some(2.0), "temperature clamped to 2");
        assert_eq!(clamped.top_p, Some(1.0), "top_p clamped to 1");
        assert_eq!(
            clamped.frequency_penalty,
            Some(2.0),
            "frequency clamped to 2"
        );
        assert_eq!(
            clamped.presence_penalty,
            Some(-2.0),
            "presence clamped to -2"
        );
        assert_eq!(clamped.max_tokens, Some(512));

        // A max_tokens of 0 is the neutral default ("no cap") → treated as unset.
        let mut zero = ParamValues::new();
        zero.set("max_tokens", ParamValue::Int(0));
        assert_eq!(overlay_assist_params(base, &zero).max_tokens, None);
    }

    fn provider_at(base_url: &str) -> CloudProvider {
        let mut p = crate::cloud_provider_by_id("openai").unwrap();
        p.base_url = base_url.to_owned();
        p
    }

    #[test]
    fn a_key_is_needed_except_on_this_machine() {
        let remote = provider_at("https://api.openai.com/v1");
        assert_eq!(assist_key(&remote, Some("sk-1".into())).unwrap(), "sk-1");
        assert!(assist_key(&remote, None).is_err());
        let info = call_info(&remote, "gpt-x", "Summarize.", "You: hi");
        assert!(!info.local);
        assert_eq!(
            (info.task.as_str(), info.model.as_deref()),
            ("assist", Some("gpt-x"))
        );
        assert!(call_info(&provider_at("http://127.0.0.1:11434/v1"), "m", "", "").local);
        assert!(
            assist_key(&remote, Some("  ".into())).is_err(),
            "a blank key is no key"
        );

        let ollama = provider_at("http://127.0.0.1:11434/v1");
        assert_eq!(assist_key(&ollama, None).unwrap(), "");
        assert_eq!(
            assist_key(&provider_at("http://localhost:1234/v1"), None).unwrap(),
            ""
        );
        assert_eq!(
            assist_key(&ollama, Some("k".into())).unwrap(),
            "k",
            "a saved key is still sent"
        );

        // A host that only starts with "localhost" is not this machine.
        assert!(assist_key(&provider_at("http://localhost.evil.example/v1"), None).is_err());
    }

    #[test]
    fn the_subscription_reply_is_the_text_field() {
        assert_eq!(
            assist_text(&serde_json::json!({"text": "  hi  "})).unwrap(),
            "hi"
        );
        assert!(assist_text(&serde_json::json!({"text": " "})).is_err());
        assert!(assist_text(&serde_json::json!({})).is_err());

        let req = assist_request("Summarize.", "You: hello");
        assert_eq!(req.task, TaskKind::Assist);
        assert_eq!(req.instructions, "Summarize.");
        assert_eq!(req.context, "You: hello");
        assert!(req.images.is_empty());
    }

    #[test]
    fn a_long_transcript_map_reduces_on_the_subscription() {
        use std::sync::Mutex;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let backend = wisp_reasoning::ScriptedBackend::with_responder("scripted", move |req| {
            log.lock().unwrap().push(req.context.clone());
            Ok(serde_json::json!({"text": format!("part of {} chars", req.context.len())}))
        });
        let target = AssistTarget::Subscription(Arc::new(backend));
        let assist = AssistParams {
            context_tokens: Some(10), // an 18-char budget
            ..AssistParams::default()
        };

        let out = run_assist(&target, "Summarize.", "aaaaaaaaaa\nbbbbbbbbbb", &assist).unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "two parts, then one combine: {seen:?}");
        assert!(seen[2].contains("=== Part 1/2 ===") && seen[2].contains("=== Part 2/2 ==="));
        assert!(out.starts_with("part of"));
    }
}

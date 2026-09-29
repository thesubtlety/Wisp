//! Checks a local reasoning CLI end to end: health, then (with `--run`) one tiny structured call.
//!
//!     cargo run -p wisp-reasoning --example probe -- codex          # health only
//!     cargo run -p wisp-reasoning --example probe -- claude --run   # plus one real call
//!     cargo run -p wisp-reasoning --example probe -- auto --run     # Codex, falling back to Claude
//!     cargo run -p wisp-reasoning --example probe -- local --run    # Ollama at localhost:11434
//!     cargo run -p wisp-reasoning --example probe -- claude --run --image shot.png
//!
//! `local` reads `WISP_PROBE_LOCAL_URL` (default `http://127.0.0.1:11434/v1`) and
//! `WISP_PROBE_LOCAL_MODEL` (default `llama3.2:3b`).
//!
//! The call uses your logged-in subscription and sends only the made-up context below.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use wisp_reasoning::{
    CancelToken, ClaudeCodeBackend, ClaudeConfig, CodexCliBackend, CodexConfig, FallbackBackend,
    LocalConfig, OpenAiCompatBackend, ReasoningBackend, ReasoningRequest, TaskKind,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let backend: Box<dyn ReasoningBackend> = match args.first().map(String::as_str) {
        Some("claude") => Box::new(ClaudeCodeBackend::new(ClaudeConfig::default())),
        Some("auto") => Box::new(FallbackBackend::codex_then_claude()),
        Some("local") => Box::new(OpenAiCompatBackend::new(LocalConfig {
            base_url: env_or("WISP_PROBE_LOCAL_URL", "http://127.0.0.1:11434/v1"),
            model: env_or("WISP_PROBE_LOCAL_MODEL", "llama3.2:3b"),
            api_key: None,
        })),
        Some("codex") | None => Box::new(CodexCliBackend::new(CodexConfig::default())),
        Some(other) => {
            eprintln!("unknown backend {other:?}; use codex, claude, auto or local");
            std::process::exit(2);
        }
    };

    let health = backend.health();
    println!("{} health: {health:?}", backend.name());
    if !health.is_ready() || !args.iter().any(|a| a == "--run") {
        return;
    }

    let image = args
        .iter()
        .position(|a| a == "--image")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let instructions = if image.is_some() {
        "Answer from the context and the attached image. Describe what the image shows in one \
         sentence, then cite the evidence IDs you used."
    } else {
        "Answer from the context only. Cite the evidence IDs you used."
    };
    let request = ReasoningRequest {
        task: TaskKind::Ask,
        instructions: instructions.into(),
        context: "[T1] Them: Production has to run in our own Azure tenant.\n\
                  [T2] You: Understood, Azure it is."
            .into(),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["answer", "evidence"],
            "properties": {
                "answer": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "string"}}
            }
        }),
        timeout: Duration::from_secs(180),
        images: image.into_iter().collect(),
    };
    match backend.invoke(&request, &CancelToken::new()) {
        Ok(resp) => println!(
            "ok from {} in {:?}:\n{:#}",
            resp.backend, resp.elapsed, resp.output
        ),
        Err(e) => println!("failed: {e}"),
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

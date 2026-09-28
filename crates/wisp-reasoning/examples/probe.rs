//! Checks a local reasoning CLI end to end: health, then (with `--run`) one tiny structured call.
//!
//!     cargo run -p wisp-reasoning --example probe -- codex          # health only
//!     cargo run -p wisp-reasoning --example probe -- claude --run   # plus one real call
//!     cargo run -p wisp-reasoning --example probe -- auto --run     # Codex, falling back to Claude
//!
//! The call uses your logged-in subscription and sends only the made-up context below.

use std::time::Duration;

use serde_json::json;
use wisp_reasoning::{
    CancelToken, ClaudeCodeBackend, ClaudeConfig, CodexCliBackend, CodexConfig, FallbackBackend,
    ReasoningBackend, ReasoningRequest, TaskKind,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let backend: Box<dyn ReasoningBackend> = match args.first().map(String::as_str) {
        Some("claude") => Box::new(ClaudeCodeBackend::new(ClaudeConfig::default())),
        Some("auto") => Box::new(FallbackBackend::codex_then_claude()),
        Some("codex") | None => Box::new(CodexCliBackend::new(CodexConfig::default())),
        Some(other) => {
            eprintln!("unknown backend {other:?}; use codex, claude or auto");
            std::process::exit(2);
        }
    };

    let health = backend.health();
    println!("{} health: {health:?}", backend.name());
    if !health.is_ready() || !args.iter().any(|a| a == "--run") {
        return;
    }

    let request = ReasoningRequest {
        task: TaskKind::Ask,
        instructions: "Answer from the context only. Cite the evidence IDs you used.".into(),
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
        images: Vec::new(),
    };
    match backend.invoke(&request, &CancelToken::new()) {
        Ok(resp) => println!(
            "ok from {} in {:?}:\n{:#}",
            resp.backend, resp.elapsed, resp.output
        ),
        Err(e) => println!("failed: {e}"),
    }
}

//! Checks the local `codex` CLI end to end: health, then (with `--run`) one tiny structured call.
//!
//!     cargo run -p wisp-reasoning --example codex_probe            # health only
//!     cargo run -p wisp-reasoning --example codex_probe -- --run   # plus one real call
//!
//! The call uses your logged-in Codex subscription and sends only the made-up context below.

use std::time::Duration;

use serde_json::json;
use wisp_reasoning::{
    CancelToken, CodexCliBackend, CodexConfig, ReasoningBackend, ReasoningRequest, TaskKind,
};

fn main() {
    let backend = CodexCliBackend::new(CodexConfig::default());
    let health = backend.health();
    println!("health: {health:?}");
    if !health.is_ready() || !std::env::args().any(|a| a == "--run") {
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
    };
    match backend.invoke(&request, &CancelToken::new()) {
        Ok(resp) => println!("ok in {:?}:\n{:#}", resp.elapsed, resp.output),
        Err(e) => println!("failed: {e}"),
    }
}

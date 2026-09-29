//! A record of every model call: what was sent, where, and what came back. [`AuditingBackend`]
//! wraps any [`ReasoningBackend`] and hands an [`AuditRecord`] to a sink after each call, success or
//! error; [`audited`] does the same for a call made outside a backend (a chat request over HTTP).
//! Where the records go is the sink's business.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::{
    CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError, ReasoningRequest,
    ReasoningResponse, TaskKind, TokenUsage,
};

/// Rough characters per token, for calls whose backend reports no usage.
const CHARS_PER_TOKEN: u64 = 4;

/// One finished model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// When the call started (Unix ms).
    pub at_ms: i64,
    /// What the call was for (`observe`, `ask`, `assist`, …).
    pub task: String,
    /// The backend or provider that answered, or was tried.
    pub backend: String,
    /// The model asked for; `None` for the backend's default.
    pub model: Option<String>,
    /// Whether the data stayed on this machine.
    pub local: bool,
    pub instructions: String,
    pub context: String,
    /// Images attached to the call, by path.
    pub images: Vec<PathBuf>,
    /// The reply as received; empty when the call failed.
    pub output: String,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The token counts are a character-based estimate, not the backend's own.
    pub tokens_estimated: bool,
}

/// Where records go. Called on the calling thread right after each call, so it should be quick.
pub type AuditSink = Arc<dyn Fn(AuditRecord) + Send + Sync>;

/// What is known about a call before it runs.
#[derive(Debug, Clone)]
pub struct CallInfo {
    pub task: String,
    pub backend: String,
    pub model: Option<String>,
    pub local: bool,
    pub instructions: String,
    pub context: String,
    pub images: Vec<PathBuf>,
}

/// A reasoning task's name as recorded: its snake_case serde name.
pub fn task_label(task: TaskKind) -> String {
    serde_json::to_value(task)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A token estimate for `text`: characters over four, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// The current time in Unix ms.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl AuditRecord {
    /// The record of a call that started at `at_ms` and took `elapsed`. `result` is the reply and
    /// its reported usage, or the error text. Without reported usage the tokens are estimated from
    /// the text sent and received.
    pub fn finished(
        info: CallInfo,
        at_ms: i64,
        elapsed: Duration,
        result: Result<(String, Option<TokenUsage>), String>,
    ) -> Self {
        let (output, usage, error) = match result {
            Ok((output, usage)) => (output, usage, None),
            Err(e) => (String::new(), None, Some(e)),
        };
        let (tokens_in, tokens_out, tokens_estimated) = match usage {
            Some(u) => (u.input, u.output, false),
            None => (
                estimate_tokens(&info.instructions) + estimate_tokens(&info.context),
                estimate_tokens(&output),
                true,
            ),
        };
        Self {
            at_ms,
            task: info.task,
            backend: info.backend,
            model: info.model,
            local: info.local,
            instructions: info.instructions,
            context: info.context,
            images: info.images,
            output,
            error,
            elapsed_ms: elapsed.as_millis() as u64,
            tokens_in,
            tokens_out,
            tokens_estimated,
        }
    }
}

/// Runs `call` and records it. `reply` turns a success into the reply text and reported usage.
pub fn audited<T, E: std::fmt::Display>(
    sink: &AuditSink,
    info: CallInfo,
    call: impl FnOnce() -> Result<T, E>,
    reply: impl FnOnce(&T) -> (String, Option<TokenUsage>),
) -> Result<T, E> {
    let at_ms = now_ms();
    let start = Instant::now();
    let result = call();
    let summary = match &result {
        Ok(value) => Ok(reply(value)),
        Err(e) => Err(e.to_string()),
    };
    sink(AuditRecord::finished(info, at_ms, start.elapsed(), summary));
    result
}

/// Records every call to the backend it wraps. Wrap each backend that sends data somewhere, not a
/// [`crate::FallbackBackend`] around them, so each attempt is recorded under its own name and
/// destination.
pub struct AuditingBackend {
    inner: Box<dyn ReasoningBackend>,
    sink: AuditSink,
}

impl AuditingBackend {
    pub fn new(inner: Box<dyn ReasoningBackend>, sink: AuditSink) -> Self {
        Self { inner, sink }
    }

    fn info(&self, req: &ReasoningRequest) -> CallInfo {
        CallInfo {
            task: task_label(req.task),
            backend: self.inner.name().to_owned(),
            model: self.inner.model().map(str::to_owned),
            local: self.inner.capabilities().local,
            instructions: req.instructions.clone(),
            context: req.context.clone(),
            images: req.images.clone(),
        }
    }
}

impl ReasoningBackend for AuditingBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn model(&self) -> Option<&str> {
        self.inner.model()
    }
    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        audited(
            &self.sink,
            self.info(req),
            || self.inner.invoke(req, cancel),
            |resp| {
                // A CLI's structured output can come with no result text; record the JSON then.
                let text = if resp.raw.trim().is_empty() {
                    resp.output.to_string()
                } else {
                    resp.raw.clone()
                };
                (text, resp.usage)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedBackend;
    use std::sync::Mutex;

    fn collector() -> (AuditSink, Arc<Mutex<Vec<AuditRecord>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let sink: AuditSink = Arc::new(move |r| log.lock().unwrap().push(r));
        (sink, seen)
    }

    fn req() -> ReasoningRequest {
        ReasoningRequest {
            task: TaskKind::EndgameAudit,
            instructions: "Be brief.".into(),
            context: "L1 You: ship it friday".into(),
            output_schema: serde_json::json!({"type": "object"}),
            timeout: Duration::from_secs(1),
            images: vec!["/tmp/shot.png".into()],
        }
    }

    #[test]
    fn success_and_error_are_both_recorded() {
        let inner = ScriptedBackend::named("scripted");
        inner.push_ok(serde_json::json!({"ok": true}));
        inner.push_err(ReasoningError::Timeout(Duration::from_secs(1)));
        let (sink, seen) = collector();
        let b = AuditingBackend::new(Box::new(inner), sink);
        let before = now_ms();

        assert!(b.invoke(&req(), &CancelToken::new()).is_ok());
        assert!(b.invoke(&req(), &CancelToken::new()).is_err());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let ok = &seen[0];
        assert!(ok.at_ms >= before);
        assert_eq!(ok.task, "endgame_audit");
        assert_eq!(ok.backend, "scripted");
        assert_eq!(ok.model, None);
        assert!(ok.local, "the scripted backend reports itself local");
        assert_eq!(ok.instructions, "Be brief.");
        assert_eq!(ok.context, "L1 You: ship it friday");
        assert_eq!(ok.images, vec![PathBuf::from("/tmp/shot.png")]);
        assert_eq!(ok.output, "{\"ok\":true}");
        assert_eq!(ok.error, None);
        assert!(ok.tokens_estimated, "the scripted backend reports no usage");
        assert_eq!(ok.tokens_in, 3 + 6);
        assert_eq!(ok.tokens_out, 3);

        let err = &seen[1];
        assert_eq!(err.output, "");
        assert!(err.error.as_deref().unwrap().contains("timed out"));
        assert_eq!(err.tokens_out, 0);
    }

    #[test]
    fn reported_usage_beats_the_estimate() {
        let info = CallInfo {
            task: "assist".into(),
            backend: "openai".into(),
            model: Some("gpt-x".into()),
            local: false,
            instructions: "x".repeat(400),
            context: String::new(),
            images: Vec::new(),
        };
        let reported = AuditRecord::finished(
            info.clone(),
            1,
            Duration::from_millis(1500),
            Ok((
                "hi".into(),
                Some(TokenUsage {
                    input: 7,
                    output: 2,
                }),
            )),
        );
        assert_eq!(
            (
                reported.tokens_in,
                reported.tokens_out,
                reported.tokens_estimated
            ),
            (7, 2, false)
        );
        assert_eq!(reported.elapsed_ms, 1500);
        let estimated = AuditRecord::finished(info, 1, Duration::ZERO, Ok(("hello".into(), None)));
        assert_eq!(
            (
                estimated.tokens_in,
                estimated.tokens_out,
                estimated.tokens_estimated
            ),
            (100, 2, true)
        );
    }

    #[test]
    fn destination_comes_from_the_wrapped_backend() {
        use crate::{ClaudeCodeBackend, CodexCliBackend, LocalConfig, OpenAiCompatBackend};
        let (sink, _) = collector();
        let local = |url: &str| {
            AuditingBackend::new(
                Box::new(OpenAiCompatBackend::new(LocalConfig {
                    base_url: url.into(),
                    model: "qwen3:8b".into(),
                    api_key: None,
                })),
                sink.clone(),
            )
        };
        let ollama = local("http://127.0.0.1:11434/v1");
        assert!(ollama.info(&req()).local);
        assert_eq!(ollama.info(&req()).model.as_deref(), Some("qwen3:8b"));
        assert!(!local("https://api.example.com/v1").info(&req()).local);
        // The CLIs send everything to their vendor.
        let codex = AuditingBackend::new(
            Box::new(CodexCliBackend::new(Default::default())),
            sink.clone(),
        );
        let claude =
            AuditingBackend::new(Box::new(ClaudeCodeBackend::new(Default::default())), sink);
        assert!(!codex.info(&req()).local);
        assert!(!claude.info(&req()).local);
        assert_eq!(claude.info(&req()).backend, "claude");
    }

    #[test]
    fn task_labels_are_snake_case() {
        assert_eq!(task_label(TaskKind::Observe), "observe");
        assert_eq!(
            task_label(TaskKind::ScreenshotContext),
            "screenshot_context"
        );
        assert_eq!(task_label(TaskKind::PostCall), "post_call");
    }
}

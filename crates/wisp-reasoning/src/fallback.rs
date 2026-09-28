use crate::backend::{
    CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError, ReasoningRequest,
    ReasoningResponse,
};
use crate::claude::{ClaudeCodeBackend, ClaudeConfig};
use crate::codex::{CodexCliBackend, CodexConfig};

/// "Automatic / fallback": try each backend in order. Cancellation stops
/// the chain; any other failure moves on to the next backend.
pub struct FallbackBackend {
    backends: Vec<Box<dyn ReasoningBackend>>,
}

impl FallbackBackend {
    pub fn new(backends: Vec<Box<dyn ReasoningBackend>>) -> Self {
        Self { backends }
    }

    /// The default chain: Codex first, then Claude Code, both in subscription mode with default
    /// settings.
    pub fn codex_then_claude() -> Self {
        Self::new(vec![
            Box::new(CodexCliBackend::new(CodexConfig::default())),
            Box::new(ClaudeCodeBackend::new(ClaudeConfig::default())),
        ])
    }

    /// The backends' names, in the order they're tried.
    pub fn order(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }
}

impl ReasoningBackend for FallbackBackend {
    fn name(&self) -> &str {
        "auto"
    }

    fn health(&self) -> Health {
        let mut reasons = Vec::new();
        for b in &self.backends {
            match b.health() {
                Health::Ready { version } => {
                    return Health::Ready {
                        version: format!("{}: {version}", b.name()),
                    }
                }
                Health::Unavailable { reason } => reasons.push(format!("{}: {reason}", b.name())),
            }
        }
        Health::Unavailable {
            reason: reasons.join("; "),
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.backends
            .iter()
            .fold(Capabilities::default(), |acc, b| {
                let c = b.capabilities();
                Capabilities {
                    structured_output: acc.structured_output || c.structured_output,
                    vision: acc.vision || c.vision,
                    local: acc.local || c.local,
                }
            })
    }

    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        let mut errors = Vec::new();
        for b in &self.backends {
            match b.invoke(req, cancel) {
                Ok(r) => return Ok(r),
                Err(ReasoningError::Cancelled) => return Err(ReasoningError::Cancelled),
                Err(e) => errors.push(format!("{}: {e}", b.name())),
            }
        }
        Err(ReasoningError::Unavailable(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ScriptedBackend, TaskKind};
    use std::time::Duration;

    fn req() -> ReasoningRequest {
        ReasoningRequest {
            task: TaskKind::Ask,
            instructions: String::new(),
            context: String::new(),
            output_schema: serde_json::json!({"type": "object"}),
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn falls_through_to_next_backend() {
        let bad = ScriptedBackend::named("bad");
        bad.push_err(ReasoningError::BadOutput("x".into()));
        let good = ScriptedBackend::named("good");
        good.push_ok(serde_json::json!({"a": 1}));
        let fb = FallbackBackend::new(vec![Box::new(bad), Box::new(good)]);
        let r = fb.invoke(&req(), &CancelToken::new()).unwrap();
        assert_eq!(r.backend, "good");
    }

    #[test]
    fn default_chain_tries_codex_first() {
        assert_eq!(
            FallbackBackend::codex_then_claude().order(),
            ["codex", "claude"]
        );
    }

    #[test]
    fn cancel_stops_chain() {
        let a = ScriptedBackend::named("a");
        a.push_err(ReasoningError::Cancelled);
        let b = ScriptedBackend::named("b");
        b.push_ok(serde_json::json!({}));
        let fb = FallbackBackend::new(vec![Box::new(a), Box::new(b)]);
        assert!(matches!(
            fb.invoke(&req(), &CancelToken::new()),
            Err(ReasoningError::Cancelled)
        ));
    }
}

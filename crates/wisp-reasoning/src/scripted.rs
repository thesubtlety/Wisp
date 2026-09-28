use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::backend::{
    CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError, ReasoningRequest,
    ReasoningResponse,
};

type Responder =
    Box<dyn Fn(&ReasoningRequest) -> Result<serde_json::Value, ReasoningError> + Send + Sync>;

/// Deterministic backend for tests and offline demos. Replies from a queue,
/// then from an optional responder function. Records every request.
pub struct ScriptedBackend {
    name: String,
    queue: Mutex<VecDeque<Result<serde_json::Value, ReasoningError>>>,
    responder: Option<Responder>,
    pub requests: Mutex<Vec<ReasoningRequest>>,
}

impl ScriptedBackend {
    pub fn named(name: &str) -> Self {
        Self {
            name: name.into(),
            queue: Mutex::default(),
            responder: None,
            requests: Mutex::default(),
        }
    }

    pub fn with_responder(
        name: &str,
        f: impl Fn(&ReasoningRequest) -> Result<serde_json::Value, ReasoningError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            responder: Some(Box::new(f)),
            ..Self::named(name)
        }
    }

    pub fn push_ok(&self, v: serde_json::Value) {
        self.queue.lock().unwrap().push_back(Ok(v));
    }

    pub fn push_err(&self, e: ReasoningError) {
        self.queue.lock().unwrap().push_back(Err(e));
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl ReasoningBackend for ScriptedBackend {
    fn name(&self) -> &str {
        &self.name
    }
    fn health(&self) -> Health {
        Health::Ready {
            version: "scripted".into(),
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: true,
            vision: false,
            local: true,
        }
    }
    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        if cancel.is_cancelled() {
            return Err(ReasoningError::Cancelled);
        }
        self.requests.lock().unwrap().push(req.clone());
        let next = self.queue.lock().unwrap().pop_front();
        let result = match (next, &self.responder) {
            (Some(r), _) => r,
            (None, Some(f)) => f(req),
            (None, None) => Err(ReasoningError::Unavailable("script exhausted".into())),
        }?;
        crate::schema::validate(&req.output_schema, &result)
            .map_err(ReasoningError::SchemaViolation)?;
        Ok(ReasoningResponse {
            raw: result.to_string(),
            output: result,
            backend: self.name.clone(),
            elapsed: Duration::ZERO,
        })
    }
}

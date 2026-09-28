//! Routing by task: cheap, frequent tasks to one backend (a local model), the rest to another
//! (the frontier CLIs). Each route is usually a [`crate::FallbackBackend`].

use std::sync::Arc;

use crate::backend::{
    CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError, ReasoningRequest,
    ReasoningResponse, TaskKind,
};

pub struct TaskRouter {
    routes: Vec<(TaskKind, Arc<dyn ReasoningBackend>)>,
    default: Arc<dyn ReasoningBackend>,
}

impl TaskRouter {
    /// Every task goes to `default` unless routed elsewhere.
    pub fn new(default: Arc<dyn ReasoningBackend>) -> Self {
        Self {
            routes: Vec::new(),
            default,
        }
    }

    /// Sends `task` to `backend`.
    pub fn route(mut self, task: TaskKind, backend: Arc<dyn ReasoningBackend>) -> Self {
        self.routes.retain(|(t, _)| *t != task);
        self.routes.push((task, backend));
        self
    }

    /// The backend `task` goes to.
    pub fn backend_for(&self, task: TaskKind) -> &dyn ReasoningBackend {
        self.routes
            .iter()
            .find(|(t, _)| *t == task)
            .map_or(self.default.as_ref(), |(_, b)| b.as_ref())
    }
}

impl ReasoningBackend for TaskRouter {
    fn name(&self) -> &str {
        "routed"
    }

    /// The default route's health: that's where most tasks go.
    fn health(&self) -> Health {
        self.default.health()
    }

    /// What some route can do; a request that needs more fails on its own route.
    fn capabilities(&self) -> Capabilities {
        self.routes.iter().map(|(_, b)| b.capabilities()).fold(
            self.default.capabilities(),
            |acc, c| Capabilities {
                structured_output: acc.structured_output || c.structured_output,
                vision: acc.vision || c.vision,
                local: acc.local && c.local,
            },
        )
    }

    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        self.backend_for(req.task).invoke(req, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedBackend;
    use std::time::Duration;

    fn req(task: TaskKind) -> ReasoningRequest {
        ReasoningRequest {
            task,
            instructions: String::new(),
            context: String::new(),
            output_schema: serde_json::json!({"type": "object"}),
            timeout: Duration::from_secs(1),
            images: Vec::new(),
        }
    }

    #[test]
    fn tasks_go_to_their_route_and_the_rest_to_the_default() {
        let local = Arc::new(ScriptedBackend::with_responder("local", |_| {
            Ok(serde_json::json!({}))
        }));
        let cloud = Arc::new(ScriptedBackend::with_responder("cloud", |_| {
            Ok(serde_json::json!({}))
        }));
        let router = TaskRouter::new(cloud.clone()).route(TaskKind::Observe, local.clone());
        let c = CancelToken::new();
        assert_eq!(
            router.invoke(&req(TaskKind::Observe), &c).unwrap().backend,
            "local"
        );
        assert_eq!(
            router.invoke(&req(TaskKind::Ask), &c).unwrap().backend,
            "cloud"
        );
        assert_eq!(
            router
                .invoke(&req(TaskKind::EndgameAudit), &c)
                .unwrap()
                .backend,
            "cloud"
        );
        assert_eq!((local.request_count(), cloud.request_count()), (1, 2));
    }
}

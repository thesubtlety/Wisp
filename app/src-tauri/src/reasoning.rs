//! Which reasoning backend meeting intelligence uses: Codex, Claude Code, a local model, or
//! Automatic. Automatic tries Codex, then Claude Code; with a local model set, the frequent live
//! state updates go to it first, and it is the last resort for everything else (offline).
//!
//! The local model is one of the user's custom OpenAI-compatible endpoints (Settings › AI models),
//! so its URL and key live where they already do.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use wisp_reasoning::{
    is_loopback, CancelToken, Capabilities, ClaudeCodeBackend, ClaudeConfig, CodexCliBackend,
    CodexConfig, FallbackBackend, Health, LocalConfig, OpenAiCompatBackend, ReasoningBackend,
    ReasoningError, ReasoningRequest, ReasoningResponse, TaskKind, TaskRouter,
};

use crate::AppState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Mode {
    #[default]
    Auto,
    Codex,
    Claude,
    Local,
}

fn yes() -> bool {
    true
}

/// The user's reasoning choice, persisted as `reasoning.json` in app data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReasoningSettings {
    #[serde(default)]
    pub(crate) mode: Mode,
    /// The custom endpoint that serves the local model.
    #[serde(default)]
    pub(crate) local_endpoint: Option<String>,
    /// The model to ask for; the endpoint's own model when unset.
    #[serde(default)]
    pub(crate) local_model: Option<String>,
    /// In Automatic, send live state updates to the local model first.
    #[serde(default = "yes")]
    pub(crate) local_for_live: bool,
}

impl Default for ReasoningSettings {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            local_endpoint: None,
            local_model: None,
            local_for_live: true,
        }
    }
}

/// Settings plus where they persist.
pub(crate) struct ReasoningState {
    settings: Mutex<ReasoningSettings>,
    path: std::path::PathBuf,
}

impl ReasoningState {
    pub(crate) fn load(path: std::path::PathBuf) -> Self {
        let settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            settings: Mutex::new(settings),
            path,
        }
    }

    fn get(&self) -> ReasoningSettings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

fn save(path: &Path, settings: &ReasoningSettings) -> Result<(), String> {
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("could not save reasoning settings: {e}"))
}

/// Tasks cheap enough for a local model in Automatic: the frequent live state updates.
const LIGHT_TASKS: &[TaskKind] = &[TaskKind::Observe];

/// A backend that always explains what's missing.
struct Missing(&'static str);

impl ReasoningBackend for Missing {
    fn name(&self) -> &str {
        "none"
    }
    fn health(&self) -> Health {
        Health::Unavailable {
            reason: self.0.to_owned(),
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn invoke(
        &self,
        _req: &ReasoningRequest,
        _cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        Err(ReasoningError::Unavailable(self.0.to_owned()))
    }
}

const NO_LOCAL: &str = "no local model is set up; pick one in Settings › Storage";

fn codex() -> Box<dyn ReasoningBackend> {
    Box::new(CodexCliBackend::new(CodexConfig::default()))
}

fn claude() -> Box<dyn ReasoningBackend> {
    Box::new(ClaudeCodeBackend::new(ClaudeConfig::default()))
}

/// The local backend the settings point at, if its endpoint still exists.
fn local_config(state: &AppState, settings: &ReasoningSettings) -> Option<LocalConfig> {
    let id = settings.local_endpoint.as_deref()?;
    let endpoint = crate::custom_endpoint(state, id)?;
    let model = settings
        .local_model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(endpoint.model);
    let api_key = state
        .cloud_keys
        .lock()
        .ok()
        .and_then(|k| k.get(id).cloned())
        .filter(|k| !k.trim().is_empty());
    Some(LocalConfig {
        base_url: endpoint.base_url,
        model,
        api_key,
    })
}

/// The backend for `settings`, given the local model's config (if any).
fn build(settings: &ReasoningSettings, local: Option<LocalConfig>) -> Arc<dyn ReasoningBackend> {
    let local = || -> Option<Box<dyn ReasoningBackend>> {
        local
            .clone()
            .map(|c| Box::new(OpenAiCompatBackend::new(c)) as Box<dyn ReasoningBackend>)
    };
    match settings.mode {
        Mode::Codex => Arc::new(FallbackBackend::new(vec![codex()])),
        Mode::Claude => Arc::new(FallbackBackend::new(vec![claude()])),
        Mode::Local => match local() {
            Some(b) => Arc::from(b),
            None => Arc::new(Missing(NO_LOCAL)),
        },
        Mode::Auto => {
            let mut frontier = vec![codex(), claude()];
            frontier.extend(local());
            let frontier: Arc<dyn ReasoningBackend> = Arc::new(FallbackBackend::new(frontier));
            match local() {
                Some(first) if settings.local_for_live => {
                    let light: Arc<dyn ReasoningBackend> =
                        Arc::new(FallbackBackend::new(vec![first, codex(), claude()]));
                    Arc::new(
                        LIGHT_TASKS
                            .iter()
                            .fold(TaskRouter::new(frontier), |r, t| r.route(*t, light.clone())),
                    )
                }
                _ => frontier,
            }
        }
    }
}

/// The local endpoint's context window (tokens) when the local model is the only backend, as
/// `Some(None)` when that endpoint sets none. `None` when the CLIs are in play.
pub(crate) fn local_only_context(state: &AppState) -> Option<Option<u32>> {
    let settings = state.reasoning.get();
    if !matches!(settings.mode, Mode::Local) {
        return None;
    }
    let endpoint = settings
        .local_endpoint
        .as_deref()
        .and_then(|id| crate::custom_endpoint(state, id));
    Some(endpoint.and_then(|e| e.assist.context_tokens))
}

/// The backend meeting intelligence should use now.
pub(crate) fn backend(state: &AppState) -> Arc<dyn ReasoningBackend> {
    let settings = state.reasoning.get();
    let local = local_config(state, &settings);
    build(&settings, local)
}

/// A custom endpoint the local model can use.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EndpointChoice {
    id: String,
    name: String,
    model: String,
    /// On this machine (a loopback address).
    local: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReasoningDto {
    settings: ReasoningSettings,
    endpoints: Vec<EndpointChoice>,
}

#[tauri::command]
pub(crate) fn get_reasoning_settings(state: State<'_, AppState>) -> ReasoningDto {
    let endpoints = crate::custom_endpoints(&state)
        .into_iter()
        .map(|e| EndpointChoice {
            local: is_loopback(&e.base_url),
            id: e.id,
            name: e.name,
            model: e.model,
        })
        .collect();
    ReasoningDto {
        settings: state.reasoning.get(),
        endpoints,
    }
}

#[tauri::command]
pub(crate) fn set_reasoning_settings(
    state: State<'_, AppState>,
    settings: ReasoningSettings,
) -> Result<(), String> {
    if let Some(id) = &settings.local_endpoint {
        if crate::custom_endpoint(&state, id).is_none() {
            return Err(format!("no endpoint {id}"));
        }
    }
    save(&state.reasoning.path, &settings)?;
    *state
        .reasoning
        .settings
        .lock()
        .map_err(|_| "state lock poisoned".to_owned())? = settings;
    Ok(())
}

/// Whether a backend (`codex`, `claude` or `local`) is ready, and what it says.
#[derive(Serialize)]
pub(crate) struct HealthDto {
    ready: bool,
    detail: String,
}

/// Checks one backend without a model call: the CLI's version and login, or the local server's
/// model list.
#[tauri::command]
pub(crate) async fn check_reasoning(app: AppHandle, which: String) -> Result<HealthDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let backend: Box<dyn ReasoningBackend> = match which.as_str() {
            "codex" => codex(),
            "claude" => claude(),
            "local" => match local_config(&state, &state.reasoning.get()) {
                Some(c) => Box::new(OpenAiCompatBackend::new(c)),
                None => Box::new(Missing(NO_LOCAL)),
            },
            other => return Err(format!("unknown backend: {other}")),
        };
        Ok(match backend.health() {
            Health::Ready { version } => HealthDto {
                ready: true,
                detail: version,
            },
            Health::Unavailable { reason } => HealthDto {
                ready: false,
                detail: reason,
            },
        })
    })
    .await
    .map_err(|e| format!("check task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Option<LocalConfig> {
        Some(LocalConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
            api_key: None,
        })
    }

    fn settings(mode: Mode, local_for_live: bool) -> ReasoningSettings {
        ReasoningSettings {
            mode,
            local_endpoint: Some("custom-ollama".into()),
            local_model: None,
            local_for_live,
        }
    }

    #[test]
    fn older_or_missing_files_load_as_automatic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reasoning.json");
        assert_eq!(
            ReasoningState::load(path.clone()).get(),
            ReasoningSettings::default()
        );
        std::fs::write(&path, r#"{"mode":"local","localEndpoint":"x"}"#).unwrap();
        let s = ReasoningState::load(path.clone()).get();
        assert_eq!(s.mode, Mode::Local);
        assert!(s.local_for_live, "missing fields take their defaults");
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(
            ReasoningState::load(path).get(),
            ReasoningSettings::default()
        );
    }

    #[test]
    fn each_mode_builds_its_chain() {
        let name = |s: &ReasoningSettings, l: Option<LocalConfig>| build(s, l).name().to_owned();
        assert_eq!(name(&settings(Mode::Auto, true), None), "auto");
        assert_eq!(name(&settings(Mode::Auto, true), local()), "routed");
        assert_eq!(name(&settings(Mode::Auto, false), local()), "auto");
        assert_eq!(name(&settings(Mode::Local, true), local()), "local");
        assert_eq!(name(&settings(Mode::Local, true), None), "none");
        assert_eq!(name(&settings(Mode::Codex, true), local()), "auto");

        let missing = build(&settings(Mode::Local, true), None);
        let req = ReasoningRequest {
            task: TaskKind::Observe,
            instructions: String::new(),
            context: String::new(),
            output_schema: serde_json::json!({}),
            timeout: std::time::Duration::from_secs(1),
            images: Vec::new(),
        };
        let err = missing.invoke(&req, &CancelToken::new()).unwrap_err();
        assert!(err.to_string().contains("Settings › Storage"));
    }
}

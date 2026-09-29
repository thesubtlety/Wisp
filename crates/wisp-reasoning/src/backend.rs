use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a reasoning call is for. Backends may route or budget on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Observe,
    Ask,
    EndgameAudit,
    PostCall,
    ProjectLearning,
    /// Describe a screenshot the user attached as context.
    ScreenshotContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningRequest {
    pub task: TaskKind,
    /// Role and rules for the model.
    pub instructions: String,
    /// Everything the model may use, already rendered with evidence IDs.
    pub context: String,
    /// JSON Schema the final output must satisfy.
    pub output_schema: serde_json::Value,
    #[serde(with = "duration_secs")]
    pub timeout: Duration,
    /// Images to show the model with the prompt (PNG, JPEG, GIF or WebP, each at most
    /// [`MAX_IMAGE_BYTES`]). Only backends with [`Capabilities::vision`] accept them.
    #[serde(default)]
    pub images: Vec<PathBuf>,
}

/// Largest image a backend will attach.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The media type for an image path, from its extension; `None` for anything else.
pub fn image_media_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Checks every attached image exists, has a supported type and fits the size limit.
pub fn check_images(req: &ReasoningRequest) -> Result<(), ReasoningError> {
    for path in &req.images {
        if image_media_type(path).is_none() {
            return Err(ReasoningError::BadInput(format!(
                "unsupported image type: {}",
                path.display()
            )));
        }
        let len = std::fs::metadata(path)
            .map_err(|e| ReasoningError::BadInput(format!("{}: {e}", path.display())))?
            .len();
        if len > MAX_IMAGE_BYTES {
            return Err(ReasoningError::BadInput(format!(
                "image too large ({len} bytes): {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ReasoningResponse {
    pub output: serde_json::Value,
    pub raw: String,
    pub backend: String,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub structured_output: bool,
    pub vision: bool,
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ready { version: String },
    Unavailable { reason: String },
}

impl Health {
    pub fn is_ready(&self) -> bool {
        matches!(self, Health::Ready { .. })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReasoningError {
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("process failed (exit {code:?}): {stderr}")]
    Process { code: Option<i32>, stderr: String },
    #[error("unparseable output: {0}")]
    BadOutput(String),
    #[error("bad input: {0}")]
    BadInput(String),
    #[error("output violates schema: {0}")]
    SchemaViolation(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Shared cancel flag. Cloning shares the flag, so the UI can hold one copy
/// and the reasoning thread another.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The reasoning contract. `cancel` from the brief is the [`CancelToken`]
/// passed to `invoke`, so one backend can serve concurrent calls.
pub trait ReasoningBackend: Send + Sync {
    fn name(&self) -> &str;
    fn health(&self) -> Health;
    fn capabilities(&self) -> Capabilities;
    fn invoke(
        &self,
        request: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError>;
}

/// The single prompt text sent to a subprocess backend on stdin.
pub fn render_prompt(req: &ReasoningRequest) -> String {
    format!(
        "{}\n\n# Context\n\n{}\n\n# Output\n\nReturn only one JSON object that matches this JSON Schema. \
         No prose, no code fences.\n\n{}\n",
        req.instructions.trim(),
        req.context.trim(),
        serde_json::to_string_pretty(&req.output_schema).unwrap_or_default()
    )
}

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Shared tail for subprocess backends: find the JSON object in `raw`,
/// check it against the request schema, wrap it.
pub(crate) fn finish(
    backend: &str,
    req: &ReasoningRequest,
    raw: String,
    candidate: Option<serde_json::Value>,
    elapsed: Duration,
) -> Result<ReasoningResponse, ReasoningError> {
    let output = candidate
        .or_else(|| crate::json::extract_json_object(&raw))
        .ok_or_else(|| ReasoningError::BadOutput(truncate(&raw, 400)))?;
    crate::schema::validate(&req.output_schema, &output)
        .map_err(ReasoningError::SchemaViolation)?;
    Ok(ReasoningResponse {
        output,
        raw,
        backend: backend.to_string(),
        elapsed,
    })
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_checked_for_type_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("a.JPG");
        std::fs::write(&ok, b"x").unwrap();
        let big = dir.path().join("b.png");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        let req = |p: &Path| ReasoningRequest {
            task: TaskKind::ScreenshotContext,
            instructions: String::new(),
            context: String::new(),
            output_schema: serde_json::json!({}),
            timeout: Duration::from_secs(1),
            images: vec![p.to_path_buf()],
        };
        assert!(check_images(&req(&ok)).is_ok());
        assert!(check_images(&req(&big)).is_err());
        assert!(check_images(&req(&dir.path().join("c.bmp"))).is_err());
        assert!(check_images(&req(&dir.path().join("missing.png"))).is_err());
    }
}

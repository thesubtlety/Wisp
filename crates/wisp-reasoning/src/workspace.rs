use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::{render_prompt, ReasoningRequest};

/// A throwaway directory holding only what one reasoning call may see.
/// Deleted on drop.
pub struct Workspace {
    dir: tempfile::TempDir,
}

const PREFIX: &str = "wisp-reasoning-";

impl Workspace {
    /// Delete workspaces a crashed or killed process left behind. They hold
    /// verbatim transcript, so they must not outlive retention. Returns how
    /// many were removed.
    pub fn sweep_stale(max_age: std::time::Duration) -> usize {
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(PREFIX))
            .filter(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > max_age)
            })
            .filter(|e| fs::remove_dir_all(e.path()).is_ok())
            .count()
    }

    pub fn create(req: &ReasoningRequest) -> std::io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(PREFIX);
        // Owner-only from creation: the directory holds verbatim transcript, and the default mode
        // under a typical umask would let other local users read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let dir = builder.tempdir()?;
        let ws = Self { dir };
        fs::write(
            ws.path().join("request.json"),
            serde_json::to_vec_pretty(req)?,
        )?;
        fs::write(ws.path().join("context.md"), &req.context)?;
        fs::write(
            ws.schema_path(),
            serde_json::to_vec_pretty(&req.output_schema)?,
        )?;
        fs::write(ws.path().join("prompt.md"), render_prompt(req))?;
        Ok(ws)
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
    pub fn schema_path(&self) -> PathBuf {
        self.path().join("schema.json")
    }
    pub fn last_message_path(&self) -> PathBuf {
        self.path().join("last-message.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskKind;
    use std::time::Duration;

    #[test]
    fn contains_only_request_files_and_cleans_up() {
        let req = ReasoningRequest {
            task: TaskKind::Ask,
            instructions: "be brief".into(),
            context: "[T1] hello".into(),
            output_schema: serde_json::json!({"type": "object"}),
            timeout: Duration::from_secs(1),
        };
        let ws = Workspace::create(&req).unwrap();
        let root = ws.path().to_path_buf();
        let mut names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["context.md", "prompt.md", "request.json", "schema.json"]
        );
        assert_eq!(
            fs::read_to_string(root.join("context.md")).unwrap(),
            "[T1] hello"
        );
        drop(ws);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn directory_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let req = ReasoningRequest {
            task: TaskKind::Observe,
            instructions: String::new(),
            context: "verbatim transcript".into(),
            output_schema: serde_json::json!({}),
            timeout: Duration::from_secs(1),
        };
        let ws = Workspace::create(&req).unwrap();
        let mode = fs::metadata(ws.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "workspace holds transcript text; got {mode:o}");
    }
}

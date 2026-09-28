use std::time::Duration;

use crate::backend::{
    finish, truncate, CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError,
    ReasoningRequest, ReasoningResponse,
};
use crate::runner::{run_command, CommandSpec, SUBSCRIPTION_STRIPPED_ENV};
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub program: String,
    pub model: Option<String>,
    /// Use the CLI's own login (ChatGPT plan). Strips API-key env vars.
    pub subscription_mode: bool,
    /// `--ignore-user-config` / `--ignore-rules`. Present in current Codex
    /// source; turn off if an older installed CLI rejects them.
    pub ignore_user_config: bool,
    pub extra_args: Vec<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            program: "codex".into(),
            model: None,
            subscription_mode: true,
            ignore_user_config: true,
            extra_args: Vec::new(),
        }
    }
}

/// Runs `codex exec` once per request: ephemeral, read-only sandbox,
/// isolated cwd, schema-constrained final message.
pub struct CodexCliBackend {
    pub config: CodexConfig,
}

impl CodexCliBackend {
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    pub fn command(&self, ws: &Workspace, prompt: String) -> CommandSpec {
        let c = &self.config;
        let mut spec = crate::locate::cli_spec(&c.program)
            .args(["exec", "--ephemeral", "--skip-git-repo-check"])
            .args(["--sandbox", "read-only", "--color", "never"]);
        if c.ignore_user_config {
            spec = spec.args(["--ignore-user-config", "--ignore-rules"]);
        }
        if c.subscription_mode {
            // Env stripping cannot reach a key stored in ~/.codex/auth.json.
            spec = spec.args(["-c", "forced_login_method=\"chatgpt\""]);
        }
        spec = spec
            .arg("--output-schema")
            .arg(ws.schema_path().display().to_string())
            .arg("--output-last-message")
            .arg(ws.last_message_path().display().to_string())
            .arg("-C")
            .arg(ws.path().display().to_string());
        if let Some(m) = &c.model {
            spec = spec.arg("-m").arg(m);
        }
        spec = spec.args(c.extra_args.iter().cloned()).arg("-");
        spec.cwd = Some(ws.path().to_path_buf());
        spec.stdin = Some(prompt);
        if c.subscription_mode {
            spec.env_remove = SUBSCRIPTION_STRIPPED_ENV
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        spec
    }
}

impl ReasoningBackend for CodexCliBackend {
    fn name(&self) -> &str {
        "codex"
    }

    /// Ready when the CLI runs and, in subscription mode, is logged in with ChatGPT rather than
    /// an API key. An older CLI without `login status` still counts as ready; its version string
    /// says the login wasn't checked, and `forced_login_method` still guards each run.
    fn health(&self) -> Health {
        let version = match version_health(&self.config.program) {
            Health::Ready { version } => version,
            unavailable => return unavailable,
        };
        if !self.config.subscription_mode {
            return Health::Ready { version };
        }
        let mut spec = crate::locate::cli_spec(&self.config.program).args(["login", "status"]);
        spec.env_remove = SUBSCRIPTION_STRIPPED_ENV
            .iter()
            .map(|s| s.to_string())
            .collect();
        match run_command(&spec, Duration::from_secs(10), &CancelToken::new()) {
            Ok(out) => match classify_login(out.code, &format!("{}\n{}", out.stdout, out.stderr)) {
                Login::ChatGpt => Health::Ready { version },
                Login::Unknown => Health::Ready {
                    version: format!("{version} (login not checked)"),
                },
                Login::Problem(reason) => Health::Unavailable { reason },
            },
            Err(e) => Health::Ready {
                version: format!("{version} (login not checked: {e})"),
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: true,
            vision: true,
            local: false,
        }
    }

    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        let ws = Workspace::create(req)?;
        let spec = self.command(&ws, crate::render_prompt(req));
        let out = run_command(&spec, req.timeout, cancel)?;
        if out.code != Some(0) {
            return Err(ReasoningError::Process {
                code: out.code,
                stderr: truncate(&out.stderr, 800),
            });
        }
        let raw = std::fs::read_to_string(ws.last_message_path()).unwrap_or(out.stdout);
        finish(self.name(), req, raw, None, out.elapsed)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Login {
    ChatGpt,
    Problem(String),
    Unknown,
}

/// Reads `codex login status` output. It prints "Logged in using ChatGPT", "Logged in using an
/// API key - …", or "Not logged in" (exit 1).
fn classify_login(code: Option<i32>, output: &str) -> Login {
    let text = output.to_lowercase();
    if text.contains("api key") {
        Login::Problem(
            "codex is logged in with an API key; subscription mode needs `codex login` with ChatGPT"
                .into(),
        )
    } else if text.contains("not logged in") {
        Login::Problem("codex is not logged in; run `codex login`".into())
    } else if code == Some(0) && text.contains("chatgpt") {
        Login::ChatGpt
    } else {
        Login::Unknown
    }
}

pub(crate) fn version_health(program: &str) -> Health {
    let spec = crate::locate::cli_spec(program).arg("--version");
    match run_command(&spec, Duration::from_secs(10), &CancelToken::new()) {
        Ok(o) if o.code == Some(0) => Health::Ready {
            version: o.stdout.trim().to_string(),
        },
        Ok(o) => Health::Unavailable {
            reason: truncate(o.stderr.trim(), 200),
        },
        Err(e) => Health::Unavailable {
            reason: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskKind;

    fn req() -> ReasoningRequest {
        ReasoningRequest {
            task: TaskKind::Observe,
            instructions: "x".into(),
            context: "y".into(),
            output_schema: serde_json::json!({"type": "object", "required": ["ok"]}),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn command_is_ephemeral_readonly_isolated_and_strips_keys() {
        let b = CodexCliBackend::new(CodexConfig {
            model: Some("gpt-x".into()),
            ..Default::default()
        });
        let ws = Workspace::create(&req()).unwrap();
        let spec = b.command(&ws, "p".into());
        let a = spec.args.join(" ");
        assert!(a.starts_with("exec --ephemeral --skip-git-repo-check --sandbox read-only"));
        assert!(a.contains("--ignore-user-config --ignore-rules"));
        assert!(a.contains(&format!("--output-schema {}", ws.schema_path().display())));
        assert!(a.contains(&format!("-C {}", ws.path().display())));
        assert!(a.contains("-m gpt-x"));
        assert!(a.contains("-c forced_login_method=\"chatgpt\""));
        assert_eq!(spec.args.last().unwrap(), "-");
        assert_eq!(spec.cwd.as_deref(), Some(ws.path()));
        assert!(spec.env_remove.contains(&"OPENAI_API_KEY".to_string()));
        assert!(spec.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
    }

    #[test]
    fn api_mode_keeps_env() {
        let b = CodexCliBackend::new(CodexConfig {
            subscription_mode: false,
            ..Default::default()
        });
        let ws = Workspace::create(&req()).unwrap();
        assert!(b.command(&ws, "p".into()).env_remove.is_empty());
    }

    #[test]
    fn login_status_is_classified() {
        assert_eq!(
            classify_login(Some(0), "Logged in using ChatGPT\n"),
            Login::ChatGpt
        );
        assert!(matches!(
            classify_login(Some(0), "Logged in using an API key - sk-proj-***abcd"),
            Login::Problem(r) if r.contains("API key")
        ));
        assert!(matches!(
            classify_login(Some(1), "Not logged in"),
            Login::Problem(r) if r.contains("codex login")
        ));
        // An older CLI that doesn't know the subcommand.
        assert_eq!(
            classify_login(Some(2), "error: unrecognized subcommand 'login'"),
            Login::Unknown
        );
    }

    #[cfg(unix)]
    fn fake_cli(body: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = path.display().to_string();
        (dir, program)
    }

    #[cfg(unix)]
    #[test]
    fn health_reports_version_and_rejects_api_key_login() {
        let script = |login: &str| {
            format!(
                "case \"$1\" in --version) echo 'codex-cli 1.2.3';; login) echo '{login}';; esac\n"
            )
        };
        let (_d, program) = fake_cli(&script("Logged in using ChatGPT"));
        let b = CodexCliBackend::new(CodexConfig {
            program,
            ..Default::default()
        });
        assert_eq!(
            b.health(),
            Health::Ready {
                version: "codex-cli 1.2.3".into()
            }
        );

        let (_d, program) = fake_cli(&script("Logged in using an API key - sk-***"));
        let b = CodexCliBackend::new(CodexConfig {
            program: program.clone(),
            ..Default::default()
        });
        assert!(!b.health().is_ready());
        // API mode doesn't care how the CLI is logged in.
        let b = CodexCliBackend::new(CodexConfig {
            program,
            subscription_mode: false,
            ..Default::default()
        });
        assert!(b.health().is_ready());
    }

    #[test]
    fn missing_cli_is_unavailable() {
        let b = CodexCliBackend::new(CodexConfig {
            program: "definitely-not-codex-xyz".into(),
            ..Default::default()
        });
        assert!(matches!(b.health(), Health::Unavailable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_rejects_output_that_breaks_the_schema() {
        let (_d, program) = fake_cli(
            "cat >/dev/null\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --output-last-message ]; then echo '{\"nope\": 1}' > \"$2\"; fi\n  shift\ndone\n",
        );
        let b = CodexCliBackend::new(CodexConfig {
            program,
            ..Default::default()
        });
        let err = b.invoke(&req(), &CancelToken::new()).unwrap_err();
        assert!(matches!(err, ReasoningError::SchemaViolation(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_surfaces_a_failed_run() {
        let (_d, program) = fake_cli("cat >/dev/null\necho 'auth expired' >&2\nexit 1\n");
        let b = CodexCliBackend::new(CodexConfig {
            program,
            ..Default::default()
        });
        match b.invoke(&req(), &CancelToken::new()).unwrap_err() {
            ReasoningError::Process { code, stderr } => {
                assert_eq!(code, Some(1));
                assert!(stderr.contains("auth expired"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn invoke_reads_last_message_file_via_fake_cli() {
        // A fake `codex` that writes the final message where told.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("codex");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat >/dev/null\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --output-last-message ]; then echo '{\"ok\": true}' > \"$2\"; fi\n  shift\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = CodexCliBackend::new(CodexConfig {
            program: fake.display().to_string(),
            ..Default::default()
        });
        let resp = b.invoke(&req(), &CancelToken::new()).unwrap();
        assert_eq!(resp.output, serde_json::json!({"ok": true}));
        assert_eq!(resp.backend, "codex");
    }
}

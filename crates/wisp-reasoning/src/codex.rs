use std::path::PathBuf;
use std::time::Duration;

use crate::backend::{
    check_images, finish, truncate, CancelToken, Capabilities, Health, ReasoningBackend,
    ReasoningError, ReasoningRequest, ReasoningResponse, TokenUsage,
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

    pub fn command(&self, ws: &Workspace, prompt: String, images: &[PathBuf]) -> CommandSpec {
        let c = &self.config;
        let mut spec = crate::locate::cli_spec(&c.program).args([
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
        ]);
        // `--image=<file>` binds one value each, so the variadic flag can't swallow the `-` that
        // reads the prompt from stdin.
        for image in images {
            spec = spec.arg(format!("--image={}", image.display()));
        }
        // `--json` prints events as JSONL, the only place the CLI reports token usage.
        spec = spec.args(["--sandbox", "read-only", "--color", "never", "--json"]);
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

    fn model(&self) -> Option<&str> {
        self.config.model.as_deref()
    }

    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        check_images(req)?;
        let ws = Workspace::create(req)?;
        let spec = self.command(&ws, crate::render_prompt(req), &req.images);
        let out = run_command(&spec, req.timeout, cancel)?;
        let events = Events::parse(&out.stdout);
        if out.code != Some(0) {
            let detail = if out.stderr.trim().is_empty() {
                events.error.unwrap_or(out.stdout)
            } else {
                out.stderr
            };
            return Err(ReasoningError::Process {
                code: out.code,
                stderr: truncate(&detail, 800),
            });
        }
        let raw = std::fs::read_to_string(ws.last_message_path())
            .ok()
            .or(events.last_message)
            .unwrap_or(out.stdout);
        finish(self.name(), req, raw, None, out.elapsed, events.usage)
    }
}

/// What `codex exec --json` printed: one JSON event per line.
#[derive(Debug, Default)]
struct Events {
    /// Summed over every `turn.completed`; `None` when no turn reported usage.
    usage: Option<TokenUsage>,
    /// The text of the last `agent_message` item.
    last_message: Option<String>,
    /// The message of the last `error` or `turn.failed` event.
    error: Option<String>,
}

impl Events {
    /// Reads the event stream. Lines that are not JSON events are skipped, so a CLI that prints
    /// something else just reports no usage.
    fn parse(stdout: &str) -> Self {
        let mut events = Self::default();
        for line in stdout.lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match event.get("type").and_then(|t| t.as_str()) {
                Some("turn.completed") => {
                    if let Some(turn) = event.get("usage").and_then(turn_usage) {
                        let sum = events.usage.get_or_insert_with(TokenUsage::default);
                        sum.input += turn.input;
                        sum.output += turn.output;
                        sum.cache_read += turn.cache_read;
                        sum.cache_write += turn.cache_write;
                    }
                }
                Some("item.completed") => {
                    let item = &event["item"];
                    if item["type"] == "agent_message" {
                        if let Some(text) = item["text"].as_str() {
                            events.last_message = Some(text.to_owned());
                        }
                    }
                }
                Some("error") => {
                    events.error = event["message"].as_str().map(str::to_owned);
                }
                Some("turn.failed") => {
                    events.error = event["error"]["message"].as_str().map(str::to_owned);
                }
                _ => {}
            }
        }
        events
    }
}

/// One turn's `usage`. OpenAI counts cached tokens inside `input_tokens`; they are split out here
/// so `input` means uncached input, as it does for every backend.
fn turn_usage(usage: &serde_json::Value) -> Option<TokenUsage> {
    let n = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let input = usage.get("input_tokens")?.as_u64()?;
    let cache_read = n("cached_input_tokens").min(input);
    let cache_write = n("cache_write_input_tokens").min(input - cache_read);
    Some(TokenUsage {
        input: input - cache_read - cache_write,
        output: usage.get("output_tokens")?.as_u64()?,
        cache_read,
        cache_write,
        cost_usd: None,
        model: None,
    })
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
            images: Vec::new(),
        }
    }

    #[test]
    fn command_is_ephemeral_readonly_isolated_and_strips_keys() {
        let b = CodexCliBackend::new(CodexConfig {
            model: Some("gpt-x".into()),
            ..Default::default()
        });
        let ws = Workspace::create(&req()).unwrap();
        let spec = b.command(&ws, "p".into(), &[]);
        let a = spec.args.join(" ");
        assert!(a.starts_with("exec --ephemeral --skip-git-repo-check --sandbox read-only"));
        assert!(a.contains("--ignore-user-config --ignore-rules"));
        assert!(a.contains("--color never --json"));
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
        assert!(b.command(&ws, "p".into(), &[]).env_remove.is_empty());
    }

    #[test]
    fn images_bind_one_value_each_and_stdin_stays_last() {
        let b = CodexCliBackend::new(CodexConfig::default());
        let ws = Workspace::create(&req()).unwrap();
        let spec = b.command(
            &ws,
            "p".into(),
            &[PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.jpg")],
        );
        let a = &spec.args;
        assert_eq!(
            &a[..5],
            [
                "exec",
                "--ephemeral",
                "--skip-git-repo-check",
                "--image=/tmp/a.png",
                "--image=/tmp/b.jpg"
            ]
        );
        assert_eq!(a[5], "--sandbox");
        assert_eq!(a.last().unwrap(), "-");
    }

    /// A real `codex exec --json` run of "Reply OK" (thread id zeroed).
    const EVENTS_FIXTURE: &str = include_str!("../fixtures/codex_exec.jsonl");

    #[test]
    fn real_events_report_usage_with_cached_input_split_out() {
        let e = Events::parse(EVENTS_FIXTURE);
        let u = e.usage.unwrap();
        assert_eq!(
            (u.input, u.cache_read, u.cache_write),
            (14693 - 12288, 12288, 0)
        );
        assert_eq!(u.output, 5);
        assert_eq!(u.total_input(), 14693);
        assert_eq!((u.cost_usd, u.model), (None, None), "codex reports neither");
        assert_eq!(e.last_message.as_deref(), Some("OK"));
        assert_eq!(e.error, None);
    }

    #[test]
    fn events_sum_turns_and_tolerate_noise() {
        let out = "not json\n\
            {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":4,\"output_tokens\":2}}\n\
            {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":20,\"output_tokens\":3}}\n\
            {\"type\":\"turn.failed\",\"error\":{\"message\":\"usage limit\"}}\n";
        let e = Events::parse(out);
        let u = e.usage.unwrap();
        assert_eq!((u.input, u.cache_read, u.output), (26, 4, 5));
        assert_eq!(e.error.as_deref(), Some("usage limit"));
        assert!(Events::parse("plain text").usage.is_none());
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
            "#!/bin/sh\ncat >/dev/null\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --output-last-message ]; then echo '{\"ok\": true}' > \"$2\"; fi\n  shift\ndone\necho '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":9,\"output_tokens\":3}}'\n",
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
        assert_eq!(resp.usage.unwrap().output, 3);
    }
}

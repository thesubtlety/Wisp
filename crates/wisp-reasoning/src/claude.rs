use crate::backend::{
    check_images, finish, image_media_type, truncate, CancelToken, Capabilities, Health,
    ReasoningBackend, ReasoningError, ReasoningRequest, ReasoningResponse, TokenUsage,
};
use crate::codex::version_health;
use crate::runner::{run_command, CommandSpec, SUBSCRIPTION_STRIPPED_ENV};
use crate::workspace::Workspace;

/// Built-in tools an observer pass must never get. `--tools ""` already
/// removes them; this list is the second lock.
const DISALLOWED: &[&str] = &[
    "Bash",
    "Edit",
    "Write",
    "Read",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Task",
];

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub program: String,
    pub model: Option<String>,
    /// Use the CLI's own login. Strips `ANTHROPIC_API_KEY` and friends,
    /// which Claude Code would otherwise prefer over the subscription.
    pub subscription_mode: bool,
    pub max_turns: u32,
    /// Pass `--json-schema` so the CLI returns `structured_output`.
    pub use_json_schema: bool,
    /// `--setting-sources ""`: ignore user/project/local settings, so a
    /// user `env` block, `apiKeyHelper` or hooks cannot move the call onto
    /// API billing or see the transcript.
    pub isolate_settings: bool,
    pub extra_args: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            program: "claude".into(),
            model: None,
            subscription_mode: true,
            // Structured output may take one extra turn inside the CLI.
            max_turns: 2,
            use_json_schema: true,
            isolate_settings: true,
            extra_args: Vec::new(),
        }
    }
}

pub struct ClaudeCodeBackend {
    pub config: ClaudeConfig,
}

impl ClaudeCodeBackend {
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    pub fn command(&self, ws: &Workspace, req: &ReasoningRequest) -> CommandSpec {
        let c = &self.config;
        // Images need the streaming input format, which needs streaming output; the last line
        // of that output is the same result envelope `json` prints.
        let format = if req.images.is_empty() {
            ["-p", "--output-format", "json"].as_slice()
        } else {
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
            ]
            .as_slice()
        };
        let mut spec = crate::locate::cli_spec(&c.program)
            .args(format.iter().copied())
            .arg("--max-turns")
            .arg(c.max_turns.to_string())
            .args(["--tools", ""])
            .arg("--disallowedTools")
            .arg(DISALLOWED.join(","))
            .args(["--permission-mode", "default"])
            .args([
                "--strict-mcp-config",
                "--no-session-persistence",
                "--disable-slash-commands",
            ]);
        if c.isolate_settings {
            spec = spec.args(["--setting-sources", ""]);
        }
        if c.use_json_schema {
            spec = spec
                .arg("--json-schema")
                .arg(crate::schema::for_model(&req.output_schema).to_string());
        }
        if let Some(m) = &c.model {
            spec = spec.arg("--model").arg(m);
        }
        spec = spec.args(c.extra_args.iter().cloned());
        spec.cwd = Some(ws.path().to_path_buf());
        spec.stdin = Some(crate::render_prompt(req));
        if c.subscription_mode {
            spec.env_remove = SUBSCRIPTION_STRIPPED_ENV
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        spec
    }
}

impl ReasoningBackend for ClaudeCodeBackend {
    fn name(&self) -> &str {
        "claude"
    }

    fn health(&self) -> Health {
        version_health(&self.config.program)
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
        let mut spec = self.command(&ws, req);
        if !req.images.is_empty() {
            spec.stdin = Some(stream_input(req)?);
        }
        let out = run_command(&spec, req.timeout, cancel)?;
        let (text, structured, usage) = parse_envelope(result_line(&out.stdout))?;
        if out.code != Some(0) && structured.is_none() && text.is_empty() {
            return Err(ReasoningError::Process {
                code: out.code,
                stderr: truncate(&out.stderr, 800),
            });
        }
        finish(self.name(), req, text, structured, out.elapsed, usage)
    }
}

/// One stream-json user message: the images, then the prompt.
fn stream_input(req: &ReasoningRequest) -> Result<String, ReasoningError> {
    use base64::Engine as _;
    let mut content = Vec::new();
    for path in &req.images {
        let media_type = image_media_type(path).ok_or_else(|| {
            ReasoningError::BadInput(format!("unsupported image type: {}", path.display()))
        })?;
        let data = base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?);
        content.push(serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }));
    }
    content.push(serde_json::json!({"type": "text", "text": crate::render_prompt(req)}));
    let message = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": content},
    });
    Ok(format!("{message}\n"))
}

/// The result envelope: the whole output for `json`, the last `result` event for `stream-json`.
fn result_line(stdout: &str) -> &str {
    stdout
        .lines()
        .rev()
        .find(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "result")
                })
                .unwrap_or(false)
        })
        .unwrap_or(stdout)
}

/// What a result envelope carries: the result text, `structured_output` when present, and the
/// token usage when reported.
type Envelope = (String, Option<serde_json::Value>, Option<TokenUsage>);

/// Parse `claude -p --output-format json`.
fn parse_envelope(stdout: &str) -> Result<Envelope, ReasoningError> {
    let env: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        // Not an envelope; let the caller hunt for JSON in the raw text.
        Err(_) => return Ok((stdout.to_string(), None, None)),
    };
    if env.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        let msg = env
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        let subtype = env.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
        return Err(ReasoningError::Process {
            code: None,
            stderr: format!("{subtype}: {msg}"),
        });
    }
    let text = env
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let structured = env
        .get("structured_output")
        .filter(|v| v.is_object())
        .cloned();
    Ok((text, structured, envelope_usage(&env)))
}

/// The envelope's `usage`, `total_cost_usd` and the model that answered. Claude reports uncached
/// input, cache writes and cache reads separately; they stay separate here.
fn envelope_usage(env: &serde_json::Value) -> Option<TokenUsage> {
    let usage = env.get("usage")?;
    let n = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Some(TokenUsage {
        input: n("input_tokens"),
        output: usage.get("output_tokens")?.as_u64()?,
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cost_usd: env.get("total_cost_usd").and_then(|v| v.as_f64()),
        model: main_model(env),
    })
}

/// The model in `modelUsage` that cost the most (a call can touch more than one), or the only one.
fn main_model(env: &serde_json::Value) -> Option<String> {
    let models = env.get("modelUsage")?.as_object()?;
    let cost = |v: &serde_json::Value| v.get("costUSD").and_then(|c| c.as_f64()).unwrap_or(0.0);
    models
        .iter()
        .max_by(|a, b| cost(a.1).total_cmp(&cost(b.1)))
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskKind;
    use std::time::Duration;

    fn req() -> ReasoningRequest {
        ReasoningRequest {
            task: TaskKind::Ask,
            instructions: "x".into(),
            context: "y".into(),
            output_schema: serde_json::json!({"type": "object", "required": ["ok"]}),
            timeout: Duration::from_secs(5),
            images: Vec::new(),
        }
    }

    #[test]
    fn images_go_inline_as_stream_json_and_the_result_event_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let mut r = req();
        r.images = vec![png];
        let b = ClaudeCodeBackend::new(ClaudeConfig::default());
        let ws = Workspace::create(&r).unwrap();
        let a = b.command(&ws, &r).args;
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert_eq!(a[pos("--output-format") + 1], "stream-json");
        assert_eq!(a[pos("--input-format") + 1], "stream-json");
        assert!(a.contains(&"--verbose".to_string()));
        assert_eq!(a[pos("--tools") + 1], "", "images never unlock tools");

        let line = stream_input(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let content = &v["message"]["content"];
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "iVBORw==");
        assert_eq!(content[1]["type"], "text");

        let out = "{\"type\":\"system\"}\n{\"type\":\"assistant\"}\n{\"type\":\"result\",\"result\":\"\",\"structured_output\":{\"ok\":true}}\n";
        let (_, structured, _) = parse_envelope(result_line(out)).unwrap();
        assert_eq!(structured.unwrap()["ok"], true);
        assert_eq!(result_line("{\"result\":\"x\"}"), "{\"result\":\"x\"}");
    }

    #[test]
    fn command_has_no_tools_bounded_turns_and_strips_api_key() {
        let b = ClaudeCodeBackend::new(ClaudeConfig::default());
        let ws = Workspace::create(&req()).unwrap();
        let spec = b.command(&ws, &req());
        let a = &spec.args;
        let pos = |f: &str| {
            a.iter()
                .position(|x| x == f)
                .unwrap_or_else(|| panic!("{f} missing"))
        };
        assert_eq!(a[0], "-p");
        assert_eq!(a[pos("--output-format") + 1], "json");
        assert_eq!(a[pos("--max-turns") + 1], "2");
        assert_eq!(a[pos("--tools") + 1], "");
        assert!(a[pos("--disallowedTools") + 1].contains("Bash"));
        assert_eq!(a[pos("--json-schema") + 1], req().output_schema.to_string());
        assert_eq!(spec.cwd.as_deref(), Some(ws.path()));
        assert_eq!(a[pos("--setting-sources") + 1], "");
        assert!(spec.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(spec.stdin.as_ref().unwrap().contains("# Context"));
    }

    #[test]
    fn envelope_prefers_structured_output() {
        let (t, s, u) = parse_envelope(
            r#"{"type":"result","is_error":false,"result":"hi","structured_output":{"ok":true}}"#,
        )
        .unwrap();
        assert_eq!(t, "hi");
        assert_eq!(s, Some(serde_json::json!({"ok": true})));
        assert_eq!(u, None, "no usage reported");
    }

    #[test]
    fn envelope_usage_keeps_cache_tokens_apart() {
        let (_, _, u) = parse_envelope(
            r#"{"type":"result","result":"{}","usage":{"input_tokens":10,"cache_creation_input_tokens":200,"cache_read_input_tokens":3000,"output_tokens":42}}"#,
        )
        .unwrap();
        let u = u.unwrap();
        assert_eq!(
            (u.input, u.cache_write, u.cache_read, u.output),
            (10, 200, 3000, 42)
        );
        assert_eq!(u.total_input(), 3210);
        assert_eq!(u.total(), 3252);
        assert_eq!((u.cost_usd, u.model), (None, None), "neither reported");
    }

    /// A real `claude -p "Reply OK" --output-format json` envelope (ids zeroed).
    const JSON_FIXTURE: &str = include_str!("../fixtures/claude_result.json");
    /// The same result as the last line of `--output-format stream-json --verbose`.
    const STREAM_FIXTURE: &str = include_str!("../fixtures/claude_result_stream.jsonl");

    #[test]
    fn real_envelopes_report_cost_cache_and_model() {
        for out in [JSON_FIXTURE, STREAM_FIXTURE] {
            let (text, _, u) = parse_envelope(result_line(out)).unwrap();
            assert_eq!(text, "OK");
            let u = u.unwrap();
            assert_eq!(u.input, 2);
            assert_eq!(u.cache_write, 3116);
            assert_eq!(u.cache_read, 3144);
            assert_eq!(u.output, 4);
            assert_eq!(u.cost_usd, Some(0.0256448));
            assert_eq!(u.model.as_deref(), Some("claude-opus-5-5"));
        }
    }

    #[test]
    fn the_costliest_model_is_the_one_reported() {
        let env = serde_json::json!({"modelUsage": {
            "claude-haiku-4-5": {"costUSD": 0.001},
            "claude-opus-5-5": {"costUSD": 0.04},
        }});
        assert_eq!(main_model(&env).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(main_model(&serde_json::json!({})), None);
    }

    #[test]
    fn envelope_error_surfaces() {
        let err =
            parse_envelope(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#)
                .unwrap_err();
        assert!(err.to_string().contains("error_max_turns"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_with_fake_cli_falls_back_to_result_text() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        let envelope = dir.path().join("envelope.json");
        let body = serde_json::json!({"type": "result", "is_error": false, "result": "```json\n{\"ok\": 1}\n```"});
        std::fs::write(&envelope, body.to_string()).unwrap();
        std::fs::write(
            &fake,
            format!("#!/bin/sh\ncat >/dev/null\ncat '{}'\n", envelope.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = ClaudeCodeBackend::new(ClaudeConfig {
            program: fake.display().to_string(),
            ..Default::default()
        });
        let resp = b.invoke(&req(), &CancelToken::new()).unwrap();
        assert_eq!(resp.output, serde_json::json!({"ok": 1}));
    }
}

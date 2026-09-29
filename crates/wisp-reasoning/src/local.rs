//! A model behind an OpenAI-compatible chat endpoint: Ollama, llama.cpp's server, LM Studio, an
//! MLX server. Meant for a model on this machine, for the cheap, frequent tasks; it implements the
//! same [`ReasoningBackend`] contract as the CLIs, so nothing above this layer changes.
//!
//! The schema goes in `response_format` (`json_schema`) where the server supports it. If the server
//! rejects that with a 400, the call is retried once without it, relying on the prompt, and the
//! output is still validated against the schema.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::backend::{
    finish, truncate, CancelToken, Capabilities, Health, ReasoningBackend, ReasoningError,
    ReasoningRequest, ReasoningResponse,
};

/// How long the health check waits for `/models`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct LocalConfig {
    /// The API root, e.g. `http://127.0.0.1:11434/v1`.
    pub base_url: String,
    pub model: String,
    /// Sent as a bearer token when set; local servers usually need none.
    pub api_key: Option<String>,
}

pub struct OpenAiCompatBackend {
    pub config: LocalConfig,
}

impl OpenAiCompatBackend {
    pub fn new(config: LocalConfig) -> Self {
        Self { config }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.config.base_url.trim_end_matches('/'))
    }

    fn body(&self, req: &ReasoningRequest, structured: bool) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": "You reply with one JSON object and nothing else."},
                {"role": "user", "content": crate::render_prompt(req)},
            ],
            "temperature": 0,
            "stream": false,
        });
        if structured {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {"name": "output", "strict": true, "schema": req.output_schema},
            });
        }
        body
    }

    /// A model on this machine is reached directly, never through a proxy from the environment;
    /// a remote endpoint may need one.
    fn agent(&self, timeout: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .try_proxy_from_env(!is_loopback(&self.config.base_url))
            .build()
    }

    fn post(&self, body: &Value, timeout: Duration) -> Result<String, (Option<u16>, String)> {
        let mut request = self
            .agent(timeout)
            .post(&self.url("chat/completions"))
            .set("Content-Type", "application/json");
        if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        match request.send_string(&body.to_string()) {
            Ok(resp) => resp.into_string().map_err(|e| (None, e.to_string())),
            Err(ureq::Error::Status(code, resp)) => {
                Err((Some(code), resp.into_string().unwrap_or_default()))
            }
            Err(e) => Err((None, e.to_string())),
        }
    }

    /// One exchange: with the schema in `response_format`, then once more without it if the server
    /// refused that.
    fn exchange(
        &self,
        req: &ReasoningRequest,
        timeout: Duration,
    ) -> Result<String, ReasoningError> {
        let first = self.post(&self.body(req, true), timeout);
        let result = match first {
            Err((Some(400), _)) => self.post(&self.body(req, false), timeout),
            other => other,
        };
        result.map_err(|(code, text)| match code {
            Some(code) => ReasoningError::Process {
                code: Some(i32::from(code)),
                stderr: truncate(&text, 800),
            },
            None => ReasoningError::Unavailable(format!("{}: {text}", self.config.base_url)),
        })
    }
}

/// The assistant message's text from a chat-completions response.
fn message_text(body: &str) -> Result<String, ReasoningError> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| ReasoningError::BadOutput(truncate(body, 400)))?;
    v.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ReasoningError::BadOutput(truncate(body, 400)))
}

/// Whether the endpoint is on this machine: a loopback host.
pub fn is_loopback(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl ReasoningBackend for OpenAiCompatBackend {
    fn name(&self) -> &str {
        "local"
    }

    fn health(&self) -> Health {
        let mut request = self.agent(HEALTH_TIMEOUT).get(&self.url("models"));
        if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        match request.call() {
            Ok(_) => Health::Ready {
                version: self.config.model.clone(),
            },
            Err(ureq::Error::Status(code, _)) => Health::Unavailable {
                reason: format!("{} answered {code}", self.config.base_url),
            },
            Err(e) => Health::Unavailable {
                reason: format!("{}: {e}", self.config.base_url),
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_output: true,
            vision: false,
            local: is_loopback(&self.config.base_url),
        }
    }

    fn invoke(
        &self,
        req: &ReasoningRequest,
        cancel: &CancelToken,
    ) -> Result<ReasoningResponse, ReasoningError> {
        if !req.images.is_empty() {
            return Err(ReasoningError::BadInput(
                "the local model can't see images".into(),
            ));
        }
        let start = Instant::now();
        // The request runs on its own thread so a cancel or the deadline returns at once; the
        // agent's timeout ends the thread soon after.
        let (tx, rx) = mpsc::channel();
        let worker = Self::new(self.config.clone());
        let request = req.clone();
        std::thread::spawn(move || {
            let _ = tx.send(worker.exchange(&request, request.timeout));
        });
        let body = loop {
            if cancel.is_cancelled() {
                return Err(ReasoningError::Cancelled);
            }
            if start.elapsed() >= req.timeout {
                return Err(ReasoningError::Timeout(req.timeout));
            }
            match rx.recv_timeout(POLL) {
                Ok(result) => break result?,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ReasoningError::Unavailable(
                        "local request thread ended".into(),
                    ))
                }
            }
        };
        finish(
            self.name(),
            req,
            message_text(&body)?,
            None,
            start.elapsed(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskKind;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A one-thread HTTP server: answers each request with the next canned (status, body) and
    /// records the request bodies.
    fn server(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).unwrap();
                log.lock().unwrap().push(format!(
                    "{}{}",
                    first.trim(),
                    String::from_utf8(buf).unwrap()
                ));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    fn req() -> ReasoningRequest {
        ReasoningRequest {
            task: TaskKind::Observe,
            instructions: "Do it.".into(),
            context: "ctx".into(),
            output_schema: json!({
                "type": "object", "additionalProperties": false,
                "required": ["ok"], "properties": {"ok": {"type": "boolean"}},
            }),
            timeout: Duration::from_secs(5),
            images: Vec::new(),
        }
    }

    fn reply(content: &str) -> String {
        json!({"choices": [{"message": {"role": "assistant", "content": content}}]}).to_string()
    }

    fn backend(base: String) -> OpenAiCompatBackend {
        OpenAiCompatBackend::new(LocalConfig {
            base_url: base,
            model: "qwen3:8b".into(),
            api_key: None,
        })
    }

    #[test]
    fn the_schema_goes_in_response_format_and_the_reply_is_validated() {
        let (base, seen) = server(vec![(200, reply("{\"ok\": true}"))]);
        let r = backend(base).invoke(&req(), &CancelToken::new()).unwrap();
        assert_eq!(r.output, json!({"ok": true}));
        assert_eq!(r.backend, "local");
        let sent = seen.lock().unwrap()[0].clone();
        assert!(sent.starts_with("POST /v1/chat/completions"));
        let body: Value = serde_json::from_str(&sent[sent.find('{').unwrap()..]).unwrap();
        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            req().output_schema
        );
        assert!(body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Do it."));
    }

    #[test]
    fn a_server_without_json_schema_gets_one_retry_without_it() {
        let (base, seen) = server(vec![
            (400, "{\"error\": \"response_format not supported\"}".into()),
            (200, reply("Sure: {\"ok\": false}")),
        ]);
        let r = backend(base).invoke(&req(), &CancelToken::new()).unwrap();
        assert_eq!(r.output, json!({"ok": false}));
        let seen = seen.lock().unwrap();
        assert!(!seen[1].contains("response_format"));
    }

    #[test]
    fn schema_violations_and_http_errors_are_errors() {
        let (base, _) = server(vec![(200, reply("{\"ok\": \"yes\"}"))]);
        assert!(matches!(
            backend(base).invoke(&req(), &CancelToken::new()),
            Err(ReasoningError::SchemaViolation(_))
        ));
        let (base, _) = server(vec![(500, "boom".into())]);
        assert!(matches!(
            backend(base).invoke(&req(), &CancelToken::new()),
            Err(ReasoningError::Process {
                code: Some(500),
                ..
            })
        ));
        // Nothing listening.
        let dead = backend("http://127.0.0.1:9/v1".into());
        assert!(matches!(
            dead.invoke(&req(), &CancelToken::new()),
            Err(ReasoningError::Unavailable(_))
        ));
        assert!(!dead.health().is_ready());
    }

    #[test]
    fn a_cancel_returns_at_once_and_images_are_refused() {
        // A listener that never answers.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let cancel = CancelToken::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c.cancel();
        });
        let started = Instant::now();
        assert!(matches!(
            backend(base.clone()).invoke(&req(), &cancel),
            Err(ReasoningError::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(listener);

        let mut r = req();
        r.images = vec!["/tmp/x.png".into()];
        assert!(matches!(
            backend(base).invoke(&r, &CancelToken::new()),
            Err(ReasoningError::BadInput(_))
        ));
    }

    #[test]
    fn health_checks_models_and_local_means_loopback() {
        let (base, seen) = server(vec![(200, "{\"data\": []}".into())]);
        assert!(backend(base).health().is_ready());
        assert!(seen.lock().unwrap()[0].starts_with("GET /v1/models"));

        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:1234/v1",
            "http://user:pw@127.0.0.2/v1",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in [
            "https://api.openai.com/v1",
            "http://192.168.1.5:11434/v1",
            "http://localhost.evil.com/v1",
            "http://127.0.0.1.nip.io/v1",
        ] {
            assert!(!is_loopback(url), "{url}");
        }
    }
}

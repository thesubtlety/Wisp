use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{CancelToken, ReasoningError};

/// Variables removed from a subscription-mode child. The keys would move the run onto API billing
/// (Claude Code prefers `ANTHROPIC_API_KEY` over the logged-in subscription). The rest would send
/// the prompt, which holds meeting text, somewhere other than the provider you logged in to.
pub const SUBSCRIPTION_STRIPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_API_KEY",
];

#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env_remove: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub stdin: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

/// How long to keep reading output after the child exits. A helper the CLI forked can hold the
/// pipes open; past this, the process group is killed rather than hanging the call.
const DRAIN_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(5)
};

/// Run a command to completion, killing it on timeout or cancel. Stdout and
/// stderr are drained on their own threads so a chatty child cannot block.
///
/// On Unix the child leads its own process group, and a timeout or cancel kills the whole group:
/// anything the CLI started goes with it.
pub fn run_command(
    spec: &CommandSpec,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<RunOutput, ReasoningError> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &spec.env_set {
        cmd.env(k, v);
    }

    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ReasoningError::Unavailable(format!("{} not found on PATH", spec.program))
        } else {
            ReasoningError::Io(e)
        }
    })?;

    if let Some(input) = &spec.stdin {
        let mut stdin = child.stdin.take().expect("stdin piped");
        let input = input.clone();
        // Write on a thread: a large prompt must not deadlock against a
        // child that is also filling its stdout pipe.
        thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let out_handle = drain(child.stdout.take().expect("stdout piped"));
    let err_handle = drain(child.stderr.take().expect("stderr piped"));

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.is_cancelled() {
            kill_tree(&mut child);
            return Err(ReasoningError::Cancelled);
        }
        if start.elapsed() >= timeout {
            kill_tree(&mut child);
            return Err(ReasoningError::Timeout(timeout));
        }
        thread::sleep(Duration::from_millis(20));
    };

    let deadline = Instant::now() + DRAIN_GRACE;
    let (stdout, stderr) = match (
        collect(&out_handle, deadline),
        collect(&err_handle, deadline),
    ) {
        (Some(o), Some(e)) => (o, e),
        _ => {
            // Something the CLI left behind still holds a pipe. While it does, the group still
            // exists, so its id can't have been reused and killing it only reaches that leftover.
            #[cfg(unix)]
            kill_group(child.id());
            return Err(ReasoningError::Process {
                code: status.code(),
                stderr: "output pipe held open by a leftover child process".into(),
            });
        }
    };

    Ok(RunOutput {
        code: status.code(),
        stdout,
        stderr,
        elapsed: start.elapsed(),
    })
}

/// Kills the child and, on Unix, everything in its process group, then reaps it. The group is
/// killed before the child is reaped, so its id can't have been reused yet.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_group(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    // SAFETY: killpg only sends a signal. The child was spawned as the leader of its own group
    // (process_group(0)), so the group holds only what it started.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

fn drain<R: Read + Send + 'static>(mut r: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

fn collect(rx: &mpsc::Receiver<String>, deadline: Instant) -> Option<String> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> CommandSpec {
        CommandSpec::new("sh").arg("-c").arg(script)
    }

    #[test]
    fn captures_stdout_stderr_and_code() {
        let out = run_command(
            &sh("echo hi; echo oops >&2; exit 3"),
            Duration::from_secs(5),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "hi");
        assert_eq!(out.stderr.trim(), "oops");
        assert_eq!(out.code, Some(3));
    }

    #[test]
    fn pipes_stdin() {
        let mut spec = sh("cat");
        spec.stdin = Some("prompt text".into());
        let out = run_command(&spec, Duration::from_secs(5), &CancelToken::new()).unwrap();
        assert_eq!(out.stdout, "prompt text");
    }

    #[test]
    fn times_out() {
        let err = run_command(
            &sh("sleep 5"),
            Duration::from_millis(200),
            &CancelToken::new(),
        )
        .unwrap_err();
        assert!(matches!(err, ReasoningError::Timeout(_)));
    }

    #[test]
    fn cancels() {
        let token = CancelToken::new();
        let t2 = token.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            t2.cancel();
        });
        let start = Instant::now();
        let err = run_command(&sh("sleep 5"), Duration::from_secs(10), &token).unwrap_err();
        assert!(matches!(err, ReasoningError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    fn alive(pid: i32) -> bool {
        // Signal 0 probes for existence without delivering anything.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0
    }

    fn wait_gone(pid: i32) -> bool {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if !alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn timeout_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let err = run_command(
            &sh(&script),
            Duration::from_millis(300),
            &CancelToken::new(),
        )
        .unwrap_err();
        assert!(matches!(err, ReasoningError::Timeout(_)));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_gone(pid),
            "background sleep {pid} outlived the timeout"
        );
    }

    #[test]
    fn leftover_holding_the_pipe_cannot_hang_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        // The shell exits at once; its background sleep keeps stdout open.
        let script = format!("sleep 30 & echo $! > {}; echo done", pidfile.display());
        let start = Instant::now();
        let err =
            run_command(&sh(&script), Duration::from_secs(60), &CancelToken::new()).unwrap_err();
        assert!(matches!(err, ReasoningError::Process { .. }), "{err:?}");
        assert!(start.elapsed() < DRAIN_GRACE + Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(wait_gone(pid), "leftover sleep {pid} was not killed");
    }

    #[test]
    fn subscription_list_is_pinned() {
        // Pinned by literal: dropping one silently reopens a billing or data-routing path.
        assert_eq!(
            SUBSCRIPTION_STRIPPED_ENV,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "OPENAI_API_KEY",
                "OPENAI_BASE_URL",
                "CODEX_API_KEY",
            ]
        );
    }

    #[test]
    fn strips_env() {
        std::env::set_var("WISP_REASONING_TEST_SECRET", "leak");
        let mut spec = sh("echo \"[$WISP_REASONING_TEST_SECRET]\"");
        spec.env_remove.push("WISP_REASONING_TEST_SECRET".into());
        let out = run_command(&spec, Duration::from_secs(5), &CancelToken::new()).unwrap();
        assert_eq!(out.stdout.trim(), "[]");
    }

    #[test]
    fn missing_program_is_unavailable() {
        let err = run_command(
            &CommandSpec::new("definitely-not-a-real-binary-xyz"),
            Duration::from_secs(1),
            &CancelToken::new(),
        )
        .unwrap_err();
        assert!(matches!(err, ReasoningError::Unavailable(_)));
    }
}

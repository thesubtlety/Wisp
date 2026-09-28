//! Runs "Analyze Now" over a transcript file and prints the resulting meeting state.
//!
//!     cargo run -p wisp-intel --example analyze_probe -- <transcript.txt> [codex|claude|auto]
//!         [--folder <project docs>] [--focus "<what you want>"] [--dry-run]
//!
//! The transcript has one line per utterance, `Speaker: text` ("You: ...", "Them: ...").
//! `--folder` indexes a folder in memory (read only) and retrieves context from it for each pass.
//! `--dry-run` prints the request that would be sent and stops; nothing leaves the machine.
//! Otherwise each pass uses your logged-in CLI subscription, sending only the rendered context.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wisp_ingest::{ingest_folder, Limits};
use wisp_intel::{
    analyze_now, prepare_observe, retrieval_text, AnalyzeInput, IntelError, MeetingState,
    TranscriptLine,
};
use wisp_library::{Library, RetrievalQuery};
use wisp_reasoning::{
    render_prompt, CancelToken, ClaudeCodeBackend, ClaudeConfig, CodexCliBackend, CodexConfig,
    FallbackBackend, ReasoningBackend,
};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: analyze_probe <transcript.txt> [codex|claude|auto] [--folder <dir>] [--focus <text>] [--dry-run]");
        std::process::exit(2);
    };
    let transcript: Vec<TranscriptLine> = std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            let (speaker, text) = l.split_once(':').unwrap_or(("Them", l));
            TranscriptLine {
                idx: i as i64,
                start_ms: i as i64 * 5000,
                speaker: speaker.trim().to_owned(),
                text: text.trim().to_owned(),
            }
        })
        .collect();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

    let mut library = Library::open_in_memory()?;
    library.create_project("probe", "Probe", now)?;
    if let Some(folder) = flag(&args, "--folder") {
        let r = ingest_folder(
            &mut library,
            "probe",
            &PathBuf::from(folder),
            now,
            &Limits::default(),
        )?;
        println!("indexed {} file(s), skipped {}", r.added, r.skipped.len());
    }
    let focus = flag(&args, "--focus");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let backend: Box<dyn ReasoningBackend> = match args.get(1).map(String::as_str) {
        Some("claude") => Box::new(ClaudeCodeBackend::new(ClaudeConfig::default())),
        Some("auto") => Box::new(FallbackBackend::codex_then_claude()),
        _ => Box::new(CodexCliBackend::new(CodexConfig::default())),
    };
    if !dry_run {
        let health = backend.health();
        println!("{} health: {health:?}", backend.name());
        if !health.is_ready() {
            return Ok(());
        }
    }

    let mut state = MeetingState::new("probe-meeting");
    loop {
        let query = retrieval_text(&state, &transcript);
        let retrieved = library.retrieve(&RetrievalQuery {
            text: &query,
            project_id: Some("probe"),
            include_meetings: false,
            exclude_meeting_id: None,
            limit: 8,
            max_total_chars: 4000,
        })?;
        let input = AnalyzeInput {
            transcript: &transcript,
            retrieved: &retrieved,
            focus: focus.as_deref(),
            endgame: false,
            memory: &[],
            timeout: Duration::from_secs(240),
        };
        if dry_run {
            let pass = prepare_observe(&state, &input)?;
            println!("{}", render_prompt(&pass.request));
            return Ok(());
        }
        match analyze_now(
            backend.as_ref(),
            &CancelToken::new(),
            &mut state,
            &input,
            now,
        ) {
            Ok(out) => {
                println!(
                    "pass via {} in {:?}: {} new line(s), {} applied ({} merged), {} rejected",
                    out.backend,
                    out.elapsed,
                    out.new_lines,
                    out.report.applied.len(),
                    out.report.merged,
                    out.report.rejected.len()
                );
                for r in &out.report.rejected {
                    println!("  rejected op {}: {}", r.index, r.reason);
                }
            }
            Err(IntelError::NothingNew) => break,
            Err(e) => {
                println!("pass failed: {e}");
                break;
            }
        }
    }

    println!("\nstate:");
    for item in state.items.values() {
        println!(
            "{:8} {:14} {:9} {:.2} {:10} {}  [{}]",
            item.id,
            item.kind.as_str(),
            item.status.as_str(),
            item.confidence,
            item.lifecycle.as_str(),
            item.text,
            item.source_refs.join(", ")
        );
    }
    Ok(())
}

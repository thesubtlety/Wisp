//! "Analyze Now": one observer pass over the transcript lines added since the last pass.
//!
//! [`prepare_observe`] builds the request (instructions, context with evidence IDs, the op schema)
//! and the evidence packet that goes with it. [`analyze_now`] sends it to a reasoning backend,
//! reduces the returned ops into the state, and advances [`MeetingState::analyzed_through`]. The
//! caller supplies retrieved project context; [`retrieval_text`] gives it the text to search with.

use std::fmt::Write;
use std::time::Duration;

use wisp_library::Snippet;
use wisp_reasoning::{CancelToken, ReasoningBackend, ReasoningError, ReasoningRequest, TaskKind};

use crate::evidence::{render_snippets, render_transcript, EvidencePacket, TranscriptLine};
use crate::intervene::{validate_candidate, Candidate, MAX_CANDIDATES_PER_PASS};
use crate::model::MeetingState;
use crate::ops::{output_schema, OpBatch};
use crate::reducer::{reduce, ApplyReport, RejectReason};

/// Lines already analyzed that are shown again before the new ones, for continuity.
pub const CONTEXT_LINES: usize = 30;
/// Most transcript characters in one pass. New lines past this wait for the next pass.
pub const MAX_TRANSCRIPT_CHARS: usize = 24_000;
/// Most characters of new text handed to retrieval.
pub const RETRIEVAL_CHARS: usize = 1_500;

const INSTRUCTIONS: &str = "\
You keep the structured state of a live meeting for one participant, labelled \"You\". \
The other side is \"Them\" or a speaker name. You receive the current state, retrieved project \
context, and transcript lines. Propose changes to the state as ops.

Rules:
- Use only the evidence in the context. Cite evidence IDs exactly as shown, like T12, D17:C4 or \
M1:T221, in source_refs. Never invent an ID. Every op cites at least one.
- Work from the new lines. Earlier lines and project context are there to interpret them.
- epistemic_status: \"stated\" when someone said it or a document says it; \"inferred\" when it \
follows from the evidence but nobody said it; \"suggested\" for your own recommendation, only for \
open_question, risk, task_candidate or objective.
- confidence is how sure you are that the item is correct as written, from 0 to 1.
- Prefer updating an existing item (ids are listed in the state) over adding a near-duplicate.
- When something replaces an earlier item (a date moves, a decision is reversed), add the new item \
with a temp_id, then set the old item's lifecycle to \"superseded\" with superseded_by set to that \
temp_id. Use \"withdrawn\" when something is taken back with no replacement, \"resolved\" when a \
question is answered or a risk is dealt with, \"uncertain\" when it is now in doubt.
- Record a commitment's owner and due date only if they were said.
- For a conflict, list the conflicting item ids in related_items.
- One fact per item, short, in plain words. Skip small talk and pleasantries.
- If nothing new is worth recording, return no ops.

Candidates: things worth interrupting You for now, while the people are still present, because \
raising them later costs a follow-up, rework or another meeting. For example: something the plan \
relies on that nobody confirmed; two statements (or a statement and a document) that conflict; a \
commitment with no owner or date; a decision discussed but never made; scope that changed without \
agreement; information needed to proceed that nobody asked for. Never propose generic observations \
(\"they seem interested\", \"ask for more detail\"). Most passes should propose none; at most a few. \
Give each a one-line title, a short detail, a suggested question if there is one, its evidence IDs, \
existing item ids in related_items, and honest scores from 0 to 1: importance (later work saved), \
urgency (why now), confidence (that the issue is real), future_work_risk (chance of rework if left).

If nothing is worth recording or raising, return {\"ops\": [], \"candidates\": []}.

Fields: an add sets kind, text, epistemic_status, confidence, source_refs, and temp_id if a later \
op refers to it. An update sets id and only the fields that change, plus source_refs for the new \
evidence. A set_lifecycle sets id, lifecycle, source_refs, and superseded_by when superseded. Set \
unused fields to null and unused lists to [].";

/// What an observer pass works from.
#[derive(Debug, Clone)]
pub struct AnalyzeInput<'a> {
    /// Every final line of the meeting so far, in order.
    pub transcript: &'a [TranscriptLine],
    /// Project context retrieved for this pass (see [`retrieval_text`]).
    pub retrieved: &'a [Snippet],
    /// What the user wants from this meeting, if they said.
    pub focus: Option<&'a str>,
    /// Whether the meeting is wrapping up: candidates then focus on unresolved gaps.
    pub endgame: bool,
    pub timeout: Duration,
}

/// A request ready to send, with the packet the reducer checks citations against.
#[derive(Debug, Clone)]
pub struct PreparedPass {
    pub request: ReasoningRequest,
    pub packet: EvidencePacket,
    /// The last new line included; `analyzed_through` moves here when the pass succeeds.
    pub through: Option<i64>,
    pub new_lines: usize,
}

/// What a pass did.
#[derive(Debug, Clone)]
pub struct AnalyzeOutcome {
    pub report: ApplyReport,
    /// Proposed interventions whose evidence checked out, for the local filter.
    pub candidates: Vec<Candidate>,
    /// Proposed interventions that failed the check: title and reason.
    pub rejected_candidates: Vec<(String, RejectReason)>,
    pub new_lines: usize,
    /// New lines left for the next pass because this one was full.
    pub remaining_lines: usize,
    pub backend: String,
    pub elapsed: Duration,
}

/// Why a pass failed. The state is unchanged when it does.
#[derive(Debug, thiserror::Error)]
pub enum IntelError {
    #[error("nothing new to analyze")]
    NothingNew,
    #[error(transparent)]
    Reasoning(#[from] ReasoningError),
    #[error("reasoning output is not an op batch: {0}")]
    BadOutput(String),
}

/// Lines after `analyzed_through`.
fn new_lines<'a>(state: &MeetingState, transcript: &'a [TranscriptLine]) -> &'a [TranscriptLine] {
    let start = match state.analyzed_through {
        Some(through) => transcript.partition_point(|l| l.idx <= through),
        None => 0,
    };
    &transcript[start..]
}

/// The text of the new lines (the latest [`RETRIEVAL_CHARS`] of it), to retrieve project context
/// with. Empty when there is nothing new.
pub fn retrieval_text(state: &MeetingState, transcript: &[TranscriptLine]) -> String {
    let text = new_lines(state, transcript)
        .iter()
        .map(|l| l.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    let n = text.chars().count();
    text.chars()
        .skip(n.saturating_sub(RETRIEVAL_CHARS))
        .collect()
}

/// Builds the observer request for the lines added since the last pass: as many new lines as fit
/// in [`MAX_TRANSCRIPT_CHARS`] (oldest first, so nothing is skipped), preceded by up to
/// [`CONTEXT_LINES`] earlier lines if there is room.
pub fn prepare_observe(
    state: &MeetingState,
    input: &AnalyzeInput,
) -> Result<PreparedPass, IntelError> {
    let fresh = new_lines(state, input.transcript);
    if fresh.is_empty() {
        return Err(IntelError::NothingNew);
    }
    let cost = |l: &TranscriptLine| l.text.chars().count() + 24;
    let mut budget = MAX_TRANSCRIPT_CHARS;
    let mut take = 0;
    for line in fresh {
        let c = cost(line);
        if take > 0 && c > budget {
            break;
        }
        budget = budget.saturating_sub(c);
        take += 1;
    }
    let new = &fresh[..take];
    let before = &input.transcript[..input.transcript.len() - fresh.len()];
    let mut context_start = before.len();
    for line in before.iter().rev().take(CONTEXT_LINES) {
        let c = cost(line);
        if c > budget {
            break;
        }
        budget -= c;
        context_start -= 1;
    }
    let earlier = &before[context_start..];

    let mut packet = EvidencePacket::default();
    let transcript = render_transcript(&mut packet, &state.meeting_id, earlier, new);
    let project = render_snippets(&mut packet, input.retrieved);

    let mut context = String::new();
    if let Some(focus) = input.focus.map(str::trim).filter(|f| !f.is_empty()) {
        let _ = writeln!(context, "## What You want from this meeting\n\n{focus}\n");
    }
    if input.endgame {
        context.push_str(
            "## The meeting is wrapping up\n\nFocus candidates on what must be resolved before \
             everyone leaves: missing topics, unconfirmed assumptions, conflicts, commitments \
             without owner or date, follow-up ownership. Skip everything else.\n\n",
        );
    }
    context.push_str(&render_state(state, &packet));
    context.push_str(&project);
    context.push_str(&transcript);

    Ok(PreparedPass {
        request: ReasoningRequest {
            task: TaskKind::Observe,
            instructions: INSTRUCTIONS.to_owned(),
            context,
            output_schema: output_schema(),
            timeout: input.timeout,
        },
        packet,
        through: new.last().map(|l| l.idx),
        new_lines: new.len(),
    })
}

/// Runs one observer pass and reduces its ops into `state`. On failure `state` is untouched.
pub fn analyze_now(
    backend: &dyn ReasoningBackend,
    cancel: &CancelToken,
    state: &mut MeetingState,
    input: &AnalyzeInput,
    now_ms: i64,
) -> Result<AnalyzeOutcome, IntelError> {
    let pass = prepare_observe(state, input)?;
    let response = backend.invoke(&pass.request, cancel)?;
    let batch: OpBatch = serde_json::from_value(response.output)
        .map_err(|e| IntelError::BadOutput(e.to_string()))?;
    let report = reduce(state, &batch, &pass.packet, now_ms);
    let mut candidates = Vec::new();
    let mut rejected_candidates = Vec::new();
    for raw in batch.candidates.iter().take(MAX_CANDIDATES_PER_PASS) {
        match validate_candidate(raw, &pass.packet, state) {
            Ok(c) => candidates.push(c),
            Err(reason) => rejected_candidates.push((raw.title.clone(), reason)),
        }
    }
    state.analyzed_through = pass.through.or(state.analyzed_through);
    let remaining_lines = new_lines(state, input.transcript).len();
    Ok(AnalyzeOutcome {
        report,
        candidates,
        rejected_candidates,
        new_lines: pass.new_lines,
        remaining_lines,
        backend: response.backend,
        elapsed: response.elapsed,
    })
}

/// The live items, one per line, with the IDs of whatever evidence is in this packet.
fn render_state(state: &MeetingState, packet: &EvidencePacket) -> String {
    let items = state.live_items();
    if items.is_empty() {
        return "## Current state\n\n(empty)\n\n".to_owned();
    }
    let mut out = String::from("## Current state\n\n");
    for item in items {
        let _ = write!(
            out,
            "{} [{} · {} · {:.2} · {}] {}",
            item.id,
            item.kind.as_str(),
            item.status.as_str(),
            item.confidence,
            item.lifecycle.as_str(),
            item.text
        );
        if let Some(owner) = &item.owner {
            let _ = write!(out, " (owner: {owner})");
        }
        if let Some(due) = &item.due {
            let _ = write!(out, " (due: {due})");
        }
        if !item.related_items.is_empty() {
            let _ = write!(out, " (related: {})", item.related_items.join(", "));
        }
        let cited: Vec<&str> = item
            .source_refs
            .iter()
            .filter_map(|r| packet.alias_for(r))
            .collect();
        if !cited.is_empty() {
            let _ = write!(out, " (evidence: {})", cited.join(", "));
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use serde_json::{json, Value};
    use wisp_library::SnippetOrigin;
    use wisp_reasoning::ScriptedBackend;

    fn line(idx: i64, speaker: &str, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 5000,
            speaker: speaker.into(),
            text: text.into(),
        }
    }

    fn doc() -> Snippet {
        Snippet {
            ref_id: "S5:C0".into(),
            origin: SnippetOrigin::Source {
                source_id: 5,
                chunk_idx: 0,
                label: "security.md".into(),
                line_start: Some(12),
            },
            text: "All customer data must stay in the EU.".into(),
            score: 1.0,
        }
    }

    fn input<'a>(transcript: &'a [TranscriptLine], retrieved: &'a [Snippet]) -> AnalyzeInput<'a> {
        AnalyzeInput {
            transcript,
            retrieved,
            focus: Some("Scope the hosting model"),
            endgame: false,
            timeout: Duration::from_secs(60),
        }
    }

    fn model_add(kind: &str, text: &str, status: &str, refs: &[&str]) -> Value {
        json!({
            "op": "add", "id": null, "temp_id": null, "kind": kind, "text": text,
            "epistemic_status": status, "confidence": 0.9, "lifecycle": null,
            "superseded_by": null, "owner": null, "due": null,
            "source_refs": refs, "related_items": []
        })
    }

    #[test]
    fn a_pass_sends_cited_context_and_reduces_the_reply() {
        let transcript = vec![
            line(0, "You", "Where does production run?"),
            line(1, "Them", "In our own Azure tenant, US East."),
            line(2, "Them", "Retention is ninety days."),
        ];
        let retrieved = vec![doc()];
        let backend = ScriptedBackend::named("scripted");
        backend.push_ok(json!({"ops": [
            model_add("requirement", "Production runs in the customer's Azure tenant (US East)", "stated", &["T1"]),
            model_add("conflict", "US East hosting conflicts with EU-only data", "inferred", &["T1", "D5:C0"]),
            model_add("fact", "They run Kubernetes", "stated", &["T7"]),
        ], "candidates": [
            {"kind": "conflict", "title": "US East hosting breaks the EU-only data rule",
             "detail": "security.md requires EU regions.", "suggested_question": "Can production run in an EU region?",
             "source_refs": ["T1", "D5:C0"], "related_items": [], "importance": 0.9, "urgency": 0.9,
             "confidence": 0.85, "future_work_risk": 0.9},
            {"kind": "follow_up", "title": "Made-up evidence", "detail": "", "suggested_question": null,
             "source_refs": ["T8"], "related_items": [], "importance": 0.9, "urgency": 0.9,
             "confidence": 0.9, "future_work_risk": 0.9}
        ]}));
        let mut state = MeetingState::new("live");

        let out = analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &retrieved),
            42,
        )
        .unwrap();
        assert_eq!((out.new_lines, out.remaining_lines), (3, 0));
        assert_eq!(out.report.applied.len(), 2);
        assert_eq!(
            out.report.rejected.len(),
            1,
            "the op citing T7 (never provided) is rejected"
        );
        assert_eq!(state.analyzed_through, Some(2));
        assert_eq!(state.item("REQ-1").unwrap().source_refs, ["Mlive:T1"]);
        assert_eq!(
            state.item("CONF-1").unwrap().source_refs,
            ["Mlive:T1", "S5:C0"]
        );
        assert_eq!(state.item("CONF-1").unwrap().kind, ItemKind::Conflict);
        assert_eq!(out.candidates.len(), 1);
        assert_eq!(out.candidates[0].source_refs, ["Mlive:T1", "S5:C0"]);
        assert_eq!(
            out.rejected_candidates,
            [(
                "Made-up evidence".to_owned(),
                RejectReason::UnknownEvidence("T8".into())
            )]
        );

        let req = backend.requests.lock().unwrap()[0].clone();
        assert_eq!(req.task, TaskKind::Observe);
        assert_eq!(req.output_schema, output_schema());
        assert_eq!(req.timeout, Duration::from_secs(60));
        let ctx = &req.context;
        assert!(ctx.contains("## What You want from this meeting\n\nScope the hosting model"));
        assert!(ctx.contains("## Current state\n\n(empty)"));
        assert!(
            ctx.contains("[D5:C0] security.md, line 12\nAll customer data must stay in the EU.")
        );
        assert!(
            ctx.contains("## New in this meeting\n\n[T0] 00:00 You: Where does production run?")
        );
        assert!(!ctx.contains("Earlier in this meeting"));

        // Nothing new: no call.
        let again = analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &[]),
            43,
        );
        assert!(matches!(again, Err(IntelError::NothingNew)));
        assert_eq!(backend.request_count(), 1);
    }

    #[test]
    fn the_next_pass_shows_earlier_lines_and_the_state_with_evidence_ids() {
        let mut transcript = vec![
            line(0, "Them", "We send traffic numbers Friday."),
            line(1, "You", "Great."),
        ];
        let backend = ScriptedBackend::named("scripted");
        let mut commit = model_add(
            "commitment",
            "Customer sends traffic numbers",
            "stated",
            &["T0"],
        );
        commit["due"] = json!("Friday");
        backend.push_ok(json!({"ops": [commit], "candidates": []}));
        let mut state = MeetingState::new("live");
        analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &[]),
            1,
        )
        .unwrap();

        transcript.push(line(2, "Them", "Actually Friday won't work, no date yet."));
        let mut replacement = model_add(
            "commitment",
            "Customer sends traffic numbers; no date",
            "stated",
            &["T2"],
        );
        replacement["temp_id"] = json!("n");
        backend.push_ok(json!({"ops": [
            replacement,
            {"op": "set_lifecycle", "id": "COM-1", "temp_id": null, "kind": null, "text": null,
             "epistemic_status": null, "confidence": null, "lifecycle": "superseded",
             "superseded_by": "n", "owner": null, "due": null,
             "source_refs": ["T2"], "related_items": []}
        ], "candidates": []}));
        let out = analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &[]),
            2,
        )
        .unwrap();
        assert!(out.report.rejected.is_empty(), "{:?}", out.report.rejected);
        assert_eq!(out.new_lines, 1);

        let ctx = backend.requests.lock().unwrap()[1].context.clone();
        assert!(ctx.contains(
            "COM-1 [commitment · stated · 0.90 · active] Customer sends traffic numbers (due: Friday) (evidence: T0)"
        ), "{ctx}");
        assert!(ctx.contains("## Earlier in this meeting (already analyzed)\n\n[T0]"));
        assert!(ctx.contains("## New in this meeting\n\n[T2]"));

        let live: Vec<&str> = state.live_items().iter().map(|i| i.id.as_str()).collect();
        assert_eq!(live, ["COM-2"]);
        assert_eq!(state.analyzed_through, Some(2));
    }

    #[test]
    fn a_failed_pass_leaves_the_state_alone() {
        let transcript = vec![line(0, "Them", "Hello")];
        let mut state = MeetingState::new("live");

        let backend = ScriptedBackend::named("scripted");
        backend.push_err(ReasoningError::Timeout(Duration::from_secs(1)));
        let err = analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &[]),
            1,
        );
        assert!(matches!(
            err,
            Err(IntelError::Reasoning(ReasoningError::Timeout(_)))
        ));
        assert_eq!(state, MeetingState::new("live"));

        // Output that breaks the schema never reaches the reducer.
        backend.push_ok(json!({"ops": [{"op": "add"}]}));
        let err = analyze_now(
            &backend,
            &CancelToken::new(),
            &mut state,
            &input(&transcript, &[]),
            1,
        );
        assert!(matches!(
            err,
            Err(IntelError::Reasoning(ReasoningError::SchemaViolation(_)))
        ));
        assert_eq!(state.analyzed_through, None);

        let cancel = CancelToken::new();
        cancel.cancel();
        let err = analyze_now(&backend, &cancel, &mut state, &input(&transcript, &[]), 1);
        assert!(matches!(
            err,
            Err(IntelError::Reasoning(ReasoningError::Cancelled))
        ));
    }

    #[test]
    fn a_long_backlog_is_analyzed_over_several_passes_without_skipping() {
        let long = "word ".repeat(1000); // ~5000 chars a line
        let transcript: Vec<TranscriptLine> = (0..12).map(|i| line(i, "Them", &long)).collect();
        let backend = ScriptedBackend::with_responder("scripted", |_| {
            Ok(json!({"ops": [], "candidates": []}))
        });
        let mut state = MeetingState::new("live");
        let mut seen = 0;
        let mut passes = 0;
        loop {
            match analyze_now(
                &backend,
                &CancelToken::new(),
                &mut state,
                &input(&transcript, &[]),
                1,
            ) {
                Ok(out) => {
                    seen += out.new_lines;
                    passes += 1;
                    assert!(out.new_lines >= 1);
                }
                Err(IntelError::NothingNew) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(seen, 12);
        assert!(passes >= 3, "{passes}");
        assert_eq!(state.analyzed_through, Some(11));
        for req in backend.requests.lock().unwrap().iter() {
            let transcript_part = req.context.split("## Earlier").next().unwrap_or("");
            assert!(
                req.context.len() < MAX_TRANSCRIPT_CHARS + 4000,
                "{}",
                transcript_part.len()
            );
        }
    }

    #[test]
    fn a_single_huge_line_still_goes_through() {
        let huge = "x".repeat(MAX_TRANSCRIPT_CHARS * 2);
        let transcript = vec![line(0, "Them", &huge)];
        let pass = prepare_observe(&MeetingState::new("m"), &input(&transcript, &[])).unwrap();
        assert_eq!((pass.new_lines, pass.through), (1, Some(0)));
    }

    #[test]
    fn retrieval_text_is_the_tail_of_the_new_lines() {
        let transcript = vec![line(0, "Them", "old words"), line(1, "Them", " new words ")];
        let mut state = MeetingState::new("m");
        assert_eq!(retrieval_text(&state, &transcript), "old words new words");
        state.analyzed_through = Some(0);
        assert_eq!(retrieval_text(&state, &transcript), "new words");
        state.analyzed_through = Some(1);
        assert_eq!(retrieval_text(&state, &transcript), "");
        let long = vec![line(0, "Them", &"ab".repeat(RETRIEVAL_CHARS))];
        assert_eq!(
            retrieval_text(&MeetingState::new("m"), &long)
                .chars()
                .count(),
            RETRIEVAL_CHARS
        );
    }
}

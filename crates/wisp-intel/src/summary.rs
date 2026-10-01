//! The meeting summary: one reasoning call over the meeting's structured state, rendered as
//! Markdown to store on the meeting.
//!
//! The state is compact and already checked, so the model reads it rather than the transcript.
//! A meeting with no state (intelligence was off) falls back to the end of its transcript.

use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_reasoning::{ReasoningRequest, TaskKind};

use crate::evidence::TranscriptLine;
use crate::model::{ItemKind, MeetingState, StateItem};

/// Most characters of state items in the context.
pub const STATE_CHARS: usize = 12_000;
/// Most characters of transcript in the fallback context: the end of the meeting.
pub const TRANSCRIPT_CHARS: usize = 20_000;

/// The order kinds are listed in, most summary-worthy first. Kinds not listed are left out.
const KIND_ORDER: [ItemKind; 13] = [
    ItemKind::Decision,
    ItemKind::Commitment,
    ItemKind::TaskCandidate,
    ItemKind::OpenQuestion,
    ItemKind::Risk,
    ItemKind::Conflict,
    ItemKind::Requirement,
    ItemKind::Constraint,
    ItemKind::Objective,
    ItemKind::Fact,
    ItemKind::Assumption,
    ItemKind::Topic,
    ItemKind::Artifact,
];

const INSTRUCTIONS_STATE: &str = "Summarize this meeting for someone who missed it. The context \
lists what the meeting recorded: decisions, commitments, open questions, risks and facts, one per \
line as [kind] text (owner; due). Use only that; do not invent. tldr: two or three plain \
sentences. decisions, open_questions: short sentences. commitments: who does what, and when if \
said (empty string if not). next_steps: the concrete actions that follow, at most six. Leave a \
list empty when there is nothing for it. Write in the language the items are written in.";

const INSTRUCTIONS_TRANSCRIPT: &str = "Summarize this meeting for someone who missed it, from the \
transcript below (it may be only the end of the meeting). Use only what was said; do not invent. \
tldr: two or three plain sentences. decisions, open_questions: short sentences. commitments: who \
does what, and when if said (empty string if not). next_steps: the concrete actions that follow, \
at most six. Leave a list empty when there is nothing for it. Write in the language of the \
transcript.";

/// What the summary says about the meeting itself.
#[derive(Debug, Clone, Default)]
pub struct SummaryMeta {
    pub title: String,
    /// When it happened, already formatted.
    pub when: String,
    pub participants: Vec<String>,
}

/// The context for a summary call, and whether it had to use the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryContext {
    pub text: String,
    /// No state items: the context is the end of the transcript.
    pub from_transcript: bool,
}

/// A structured summary, as the model returns it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeetingSummary {
    #[serde(default)]
    pub tldr: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub commitments: Vec<SummaryCommitment>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// The meeting type's own sections (see [`summary_request_with_sections`]), in order.
    #[serde(default)]
    pub sections: Vec<SummarySection>,
}

/// One section the meeting type asked for, like "Strengths" for an interview.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SummarySection {
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub points: Vec<String>,
}

/// Who does what, by when.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SummaryCommitment {
    #[serde(default)]
    pub who: String,
    #[serde(default)]
    pub what: String,
    #[serde(default)]
    pub due: String,
}

/// The distinct speakers of `transcript`, in first-seen order.
pub fn participants(transcript: &[TranscriptLine]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in transcript {
        if !l.speaker.is_empty() && !out.contains(&l.speaker) {
            out.push(l.speaker.clone());
        }
    }
    out
}

fn item_line(item: &StateItem) -> String {
    let mut out = format!("- [{}] {}", item.kind.as_str(), one_line(&item.text));
    let extra: Vec<String> = [
        item.owner.as_deref().map(|o| format!("owner: {o}")),
        item.due.as_deref().map(|d| format!("due: {d}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !extra.is_empty() {
        let _ = write!(out, " ({})", extra.join("; "));
    }
    out.push('\n');
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn header(meta: &SummaryMeta, state: &MeetingState) -> String {
    let mut people = meta.participants.clone();
    for p in state
        .live_items()
        .into_iter()
        .filter(|i| i.kind == ItemKind::Participant)
    {
        let name = one_line(&p.text);
        if !people.contains(&name) {
            people.push(name);
        }
    }
    let mut out = format!("Meeting: {}\n", one_line(&meta.title));
    if !meta.when.trim().is_empty() {
        let _ = writeln!(out, "When: {}", meta.when.trim());
    }
    if !people.is_empty() {
        let _ = writeln!(out, "Participants: {}", people.join(", "));
    }
    out
}

/// The context for a summary: the header (title, date, participants), then the live state items
/// most summary-worthy first, capped at [`STATE_CHARS`]. With no state items, the last
/// [`TRANSCRIPT_CHARS`] of the transcript instead.
pub fn summary_context(
    meta: &SummaryMeta,
    state: &MeetingState,
    transcript: &[TranscriptLine],
) -> SummaryContext {
    let mut text = header(meta, state);
    let live = state.live_items();
    let mut items: Vec<&StateItem> = live
        .iter()
        .copied()
        .filter(|i| KIND_ORDER.contains(&i.kind))
        .collect();
    items.sort_by_key(|i| KIND_ORDER.iter().position(|k| *k == i.kind));
    if !items.is_empty() {
        text.push_str("\nMeeting state:\n");
        let mut used = 0;
        let mut left_out = 0;
        for item in &items {
            let line = item_line(item);
            let n = line.chars().count();
            if used + n > STATE_CHARS {
                left_out += 1;
                continue;
            }
            used += n;
            text.push_str(&line);
        }
        if left_out > 0 {
            let _ = writeln!(text, "({left_out} more items left out for length)");
        }
        return SummaryContext {
            text,
            from_transcript: false,
        };
    }
    let mut lines = Vec::new();
    let mut used = 0;
    for l in transcript.iter().rev() {
        let s = l.start_ms.max(0) / 1000;
        let line = format!(
            "[{:02}:{:02}] {}: {}\n",
            s / 60,
            s % 60,
            l.speaker,
            one_line(&l.text)
        );
        let n = line.chars().count();
        if used + n > TRANSCRIPT_CHARS {
            break;
        }
        used += n;
        lines.push(line);
    }
    let omitted = transcript.len() - lines.len();
    text.push_str("\nTranscript");
    if omitted > 0 {
        let _ = write!(
            text,
            " (the last {} of {} lines)",
            lines.len(),
            transcript.len()
        );
    }
    text.push_str(":\n");
    for line in lines.into_iter().rev() {
        text.push_str(&line);
    }
    SummaryContext {
        text,
        from_transcript: true,
    }
}

/// The output schema: strict, every field required, so every backend can enforce it.
pub fn summary_schema() -> Value {
    let list = json!({"type": "array", "items": {"type": "string"}});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tldr", "decisions", "commitments", "open_questions", "next_steps"],
        "properties": {
            "tldr": {"type": "string"},
            "decisions": list,
            "commitments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["who", "what", "due"],
                    "properties": {
                        "who": {"type": "string"},
                        "what": {"type": "string"},
                        "due": {"type": "string"}
                    }
                }
            },
            "open_questions": list,
            "next_steps": list
        }
    })
}

/// The request for one summary call.
pub fn summary_request(context: &SummaryContext, timeout: Duration) -> ReasoningRequest {
    summary_request_with_sections(context, timeout, &[])
}

/// The request for one summary call that also fills the meeting type's `sections` (headings, in
/// order). With none, the same as [`summary_request`].
pub fn summary_request_with_sections(
    context: &SummaryContext,
    timeout: Duration,
    sections: &[String],
) -> ReasoningRequest {
    let base = if context.from_transcript {
        INSTRUCTIONS_TRANSCRIPT
    } else {
        INSTRUCTIONS_STATE
    };
    let headings: Vec<String> = sections
        .iter()
        .map(|h| one_line(h))
        .filter(|h| !h.is_empty())
        .collect();
    let mut schema = summary_schema();
    let instructions = if headings.is_empty() {
        base.to_owned()
    } else {
        schema["properties"]["sections"] = json!({
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["heading", "points"],
                "properties": {
                    "heading": {"type": "string", "enum": headings},
                    "points": {"type": "array", "items": {"type": "string"}}
                }
            }
        });
        if let Some(required) = schema["required"].as_array_mut() {
            required.push("sections".into());
        }
        format!(
            "{base} sections: one entry per heading, in this order: {}. points: short sentences \
             for that heading, from the same material; leave points empty when nothing supports \
             it.",
            headings.join("; ")
        )
    };
    ReasoningRequest {
        task: TaskKind::Summary,
        instructions,
        context: context.text.clone(),
        output_schema: schema,
        timeout,
        images: Vec::new(),
    }
}

/// Reads the model's output.
pub fn parse_summary(output: Value) -> Result<MeetingSummary, String> {
    let summary: MeetingSummary = serde_json::from_value(output).map_err(|e| e.to_string())?;
    if summary.to_markdown().trim().is_empty() {
        return Err("the model returned an empty summary".to_owned());
    }
    Ok(summary)
}

fn clean(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|s| one_line(s))
        .filter(|s| !s.is_empty())
        .collect()
}

impl MeetingSummary {
    /// The summary as Markdown: the TL;DR, then a `###` section per non-empty list, so it nests
    /// under a `## Summary` heading in the record and the transcript.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let tldr = one_line(&self.tldr);
        if !tldr.is_empty() {
            let _ = writeln!(out, "**TL;DR:** {tldr}");
        }
        let extra: Vec<(String, Vec<String>)> = self
            .sections
            .iter()
            .map(|s| (one_line(&s.heading), clean(&s.points)))
            .filter(|(h, _)| !h.is_empty())
            .collect();
        let mut section = |heading: &str, lines: Vec<String>| {
            if lines.is_empty() {
                return;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = writeln!(out, "### {heading}\n");
            for l in lines {
                let _ = writeln!(out, "- {l}");
            }
        };
        for (heading, points) in extra {
            section(&heading, points);
        }
        section("Decisions", clean(&self.decisions));
        let commitments = self
            .commitments
            .iter()
            .filter(|c| !one_line(&c.what).is_empty())
            .map(|c| {
                let who = one_line(&c.who);
                let due = one_line(&c.due);
                let mut l = if who.is_empty() {
                    one_line(&c.what)
                } else {
                    format!("**{who}:** {}", one_line(&c.what))
                };
                if !due.is_empty() {
                    let _ = write!(l, " (due {due})");
                }
                l
            })
            .collect();
        section("Commitments", commitments);
        section("Open questions", clean(&self.open_questions));
        section("Next steps", clean(&self.next_steps));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpistemicStatus;
    use crate::ops::{AppliedOp, ResolvedOp};

    fn add(
        seq: u64,
        kind: ItemKind,
        text: &str,
        owner: Option<&str>,
        due: Option<&str>,
    ) -> AppliedOp {
        AppliedOp {
            seq,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: format!("{}-{seq}", kind.prefix()),
                kind,
                text: text.into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                source_refs: vec![format!("Mm:T{seq}")],
                related_items: vec![],
                owner: owner.map(str::to_owned),
                due: due.map(str::to_owned),
            },
        }
    }

    fn line(idx: i64, speaker: &str, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 1000,
            speaker: speaker.into(),
            text: text.into(),
        }
    }

    fn meta() -> SummaryMeta {
        SummaryMeta {
            title: "Acme scoping".into(),
            when: "Sept 26, 2026".into(),
            participants: vec!["You".into(), "Laurie".into()],
        }
    }

    #[test]
    fn the_context_is_the_compact_state_first_and_never_the_transcript() {
        let state = MeetingState::replay(
            "m",
            &[
                add(0, ItemKind::Fact, "Runs  in\nAzure", None, None),
                add(
                    1,
                    ItemKind::Commitment,
                    "Send traffic numbers",
                    Some("Laurie"),
                    Some("Friday"),
                ),
                add(2, ItemKind::Decision, "Use OIDC", None, None),
                add(3, ItemKind::Participant, "Sam", None, None),
            ],
        )
        .unwrap();
        let transcript = [line(0, "Laurie", "a secret line from the transcript")];
        let ctx = summary_context(&meta(), &state, &transcript);
        assert!(!ctx.from_transcript);
        assert_eq!(
            ctx.text,
            "Meeting: Acme scoping\nWhen: Sept 26, 2026\nParticipants: You, Laurie, Sam\n\
             \nMeeting state:\n\
             - [decision] Use OIDC\n\
             - [commitment] Send traffic numbers (owner: Laurie; due: Friday)\n\
             - [fact] Runs in Azure\n"
        );
    }

    #[test]
    fn a_meeting_types_sections_are_asked_for_and_rendered_after_the_tldr() {
        let ctx = SummaryContext {
            text: "Meeting: Interview".into(),
            from_transcript: false,
        };
        let plain = summary_request(&ctx, Duration::from_secs(1));
        assert!(plain.output_schema["properties"].get("sections").is_none());
        let typed = summary_request_with_sections(
            &ctx,
            Duration::from_secs(1),
            &["Strengths".into(), " ".into(), "Concerns".into()],
        );
        assert!(typed
            .instructions
            .contains("in this order: Strengths; Concerns."));
        assert_eq!(
            typed.output_schema["properties"]["sections"]["items"]["properties"]["heading"]["enum"],
            json!(["Strengths", "Concerns"])
        );
        assert!(typed.output_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("sections")));

        let summary = parse_summary(json!({
            "tldr": "Strong systems candidate.", "decisions": [], "commitments": [],
            "open_questions": [], "next_steps": ["Send the take-home"],
            "sections": [
                {"heading": "Strengths", "points": ["Led a migration"]},
                {"heading": "Concerns", "points": []}
            ]
        }))
        .unwrap();
        assert_eq!(
            summary.to_markdown(),
            "**TL;DR:** Strong systems candidate.\n\n### Strengths\n\n- Led a migration\n\n\
             ### Next steps\n\n- Send the take-home\n"
        );
    }

    #[test]
    fn the_state_is_capped_and_says_what_it_left_out() {
        let long = "x".repeat(1000);
        let ops: Vec<AppliedOp> = (0..20)
            .map(|n| add(n, ItemKind::Fact, &long, None, None))
            .collect();
        let state = MeetingState::replay("m", &ops).unwrap();
        let ctx = summary_context(&meta(), &state, &[]);
        let state_part = ctx.text.split("Meeting state:\n").nth(1).unwrap();
        assert!(state_part.chars().count() <= STATE_CHARS + 60);
        assert!(ctx.text.ends_with("(9 more items left out for length)\n"));
    }

    #[test]
    fn with_no_state_the_context_is_the_end_of_the_transcript() {
        let state = MeetingState::new("m");
        let transcript: Vec<TranscriptLine> = (0..400)
            .map(|n| line(n, "Them", &format!("line {n} {}", "y".repeat(80))))
            .collect();
        let ctx = summary_context(&meta(), &state, &transcript);
        assert!(ctx.from_transcript);
        assert!(ctx.text.chars().count() <= TRANSCRIPT_CHARS + 200);
        assert!(ctx.text.contains("Transcript (the last "));
        assert!(ctx
            .text
            .trim_end()
            .ends_with(&format!("line 399 {}", "y".repeat(80))));
        assert!(!ctx.text.contains("line 0 "), "the start is dropped first");
        let req = summary_request(&ctx, Duration::from_secs(1));
        assert_eq!(req.task, TaskKind::Summary);
        assert!(req.instructions.contains("from the transcript"));
    }

    #[test]
    fn a_summary_renders_as_markdown_with_empty_sections_left_out() {
        let output = json!({
            "tldr": "Hosting settled on Azure.\nSSO on OIDC.",
            "decisions": ["Use OIDC", " "],
            "commitments": [
                {"who": "Laurie", "what": "Send traffic numbers", "due": "Friday"},
                {"who": "", "what": "Draft the plan", "due": ""},
                {"who": "Sam", "what": "", "due": ""}
            ],
            "open_questions": [],
            "next_steps": ["Book the follow-up"]
        });
        let md = parse_summary(output).unwrap().to_markdown();
        assert_eq!(
            md,
            "**TL;DR:** Hosting settled on Azure. SSO on OIDC.\n\
             \n### Decisions\n\n- Use OIDC\n\
             \n### Commitments\n\n- **Laurie:** Send traffic numbers (due Friday)\n- Draft the plan\n\
             \n### Next steps\n\n- Book the follow-up\n"
        );
    }

    #[test]
    fn an_empty_or_malformed_summary_is_an_error() {
        assert!(parse_summary(json!({"tldr": " ", "decisions": []})).is_err());
        assert!(parse_summary(json!({"tldr": 3})).is_err());
    }

    #[test]
    fn the_schema_requires_every_field() {
        let s = summary_schema();
        assert_eq!(s["required"].as_array().unwrap().len(), 5);
        assert_eq!(
            s["properties"]["commitments"]["items"]["required"],
            json!(["who", "what", "due"])
        );
    }
}

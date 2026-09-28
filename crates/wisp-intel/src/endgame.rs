//! Endgame: helping close the meeting well.
//!
//! Endgame starts three ways. The user presses Wrapping Up (always available; recording goes on),
//! or one of two advisory signals suggests it: the scheduled end is near, or the latest lines sound
//! like a wrap-up ([`wrap_probability`]). Advisory signals only suggest; the user decides.
//!
//! Entering endgame runs a gap audit ([`prepare_audit`], [`audit`]): not a summary, but what still
//! needs resolving before everyone leaves. Each [`Gap`] cites its evidence, checked like every other
//! claim, except a "missing" gap, which is about something nobody said and so may cite nothing.

use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_library::Snippet;
use wisp_reasoning::{CancelToken, ReasoningBackend, ReasoningRequest, TaskKind};

use crate::analyze::IntelError;
use crate::evidence::{render_lines, render_snippets, EvidencePacket, TranscriptLine};
use crate::model::{MeetingState, SourceRef};
use crate::reducer::RejectReason;

/// How many of the latest lines the wrap-up signal reads.
pub const WRAP_WINDOW: usize = 12;
/// Probability at which the wrap-up signal suggests endgame.
pub const WRAP_SUGGEST_AT: f64 = 0.7;
/// How long before the scheduled end endgame is suggested.
pub const SCHEDULED_LEAD_MS: i64 = 5 * 60 * 1000;
/// Most transcript characters sent to the audit (the latest lines).
pub const AUDIT_TRANSCRIPT_CHARS: usize = 30_000;
/// Most gaps taken from one audit.
pub const MAX_GAPS: usize = 20;

/// Phrases that sound like a meeting closing, with how strongly.
const WRAP_PHRASES: &[(&str, f64)] = &[
    ("before we wrap", 0.6),
    ("wrap up", 0.5),
    ("wrap this up", 0.6),
    ("anything else", 0.45),
    ("one last thing", 0.45),
    ("last question", 0.35),
    ("out of time", 0.5),
    ("running out of time", 0.5),
    ("top of the hour", 0.4),
    ("next steps", 0.35),
    ("to recap", 0.4),
    ("to summarize", 0.4),
    ("i'll send", 0.3),
    ("i will send", 0.3),
    ("we'll send", 0.3),
    ("follow up with", 0.3),
    ("thanks everyone", 0.6),
    ("thank you everyone", 0.6),
    ("thanks all", 0.55),
    ("thanks for your time", 0.6),
    ("talk soon", 0.5),
    ("have a good one", 0.5),
];

/// How much the latest lines sound like a wrap-up, 0..1. Phrase weights combine as independent
/// evidence (1 − Π(1 − w)), so one strong phrase or a few weaker ones cross [`WRAP_SUGGEST_AT`].
pub fn wrap_probability(lines: &[TranscriptLine]) -> f64 {
    let start = lines.len().saturating_sub(WRAP_WINDOW);
    let mut miss = 1.0;
    for line in &lines[start..] {
        let text = line.text.to_lowercase().replace('’', "'");
        for (phrase, weight) in WRAP_PHRASES {
            if text.contains(phrase) {
                miss *= 1.0 - weight;
            }
        }
    }
    1.0 - miss
}

/// Why endgame is suggested or active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndgameTrigger {
    Manual,
    Scheduled,
    Semantic,
}

/// What kind of gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCategory {
    /// Something never discussed that should have been.
    Missing,
    /// Partly answered, assumed, or talked about but never decided.
    Clarify,
    CommitmentWithoutOwner,
    CommitmentWithoutDate,
    /// A promise later softened or walked back.
    WeakenedPromise,
    /// Something the other side owes.
    OwedByThem,
    /// Something You owe.
    OwedByYou,
    /// Two things that can't both hold.
    Conflict,
}

impl GapCategory {
    const ALL: [GapCategory; 8] = [
        GapCategory::Missing,
        GapCategory::Clarify,
        GapCategory::CommitmentWithoutOwner,
        GapCategory::CommitmentWithoutDate,
        GapCategory::WeakenedPromise,
        GapCategory::OwedByThem,
        GapCategory::OwedByYou,
        GapCategory::Conflict,
    ];

    fn as_str(self) -> &'static str {
        match self {
            GapCategory::Missing => "missing",
            GapCategory::Clarify => "clarify",
            GapCategory::CommitmentWithoutOwner => "commitment_without_owner",
            GapCategory::CommitmentWithoutDate => "commitment_without_date",
            GapCategory::WeakenedPromise => "weakened_promise",
            GapCategory::OwedByThem => "owed_by_them",
            GapCategory::OwedByYou => "owed_by_you",
            GapCategory::Conflict => "conflict",
        }
    }

    /// The heading used in copied text.
    fn heading(self) -> &'static str {
        match self {
            GapCategory::Missing => "Missing",
            GapCategory::Clarify => "Clarify",
            GapCategory::CommitmentWithoutOwner => "Commitment without owner",
            GapCategory::CommitmentWithoutDate => "Commitment without date",
            GapCategory::WeakenedPromise => "Promise weakened",
            GapCategory::OwedByThem => "Still owed by them",
            GapCategory::OwedByYou => "Owed by you",
            GapCategory::Conflict => "Potential conflict",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawGap {
    category: GapCategory,
    text: String,
    source_refs: Vec<String>,
    related_items: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawAudit {
    gaps: Vec<RawGap>,
}

/// One checked gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub category: GapCategory,
    pub text: String,
    pub source_refs: Vec<SourceRef>,
    /// The evidence IDs as cited, for display.
    pub cited: Vec<String>,
    pub related_items: Vec<String>,
}

/// A checked audit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditReport {
    pub gaps: Vec<Gap>,
    /// Gaps dropped: their text and why.
    pub rejected: Vec<(String, String)>,
}

impl AuditReport {
    /// "Before you wrap", grouped by category, for copying.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("## Before you wrap\n");
        for category in GapCategory::ALL {
            let gaps: Vec<&Gap> = self
                .gaps
                .iter()
                .filter(|g| g.category == category)
                .collect();
            if gaps.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n**{}**", category.heading());
            for g in gaps {
                if g.cited.is_empty() {
                    let _ = writeln!(out, "- {}", g.text);
                } else {
                    let _ = writeln!(out, "- {} [{}]", g.text, g.cited.join(", "));
                }
            }
        }
        if self.gaps.is_empty() {
            out.push_str("\nNothing outstanding found.\n");
        }
        out.trim_end().to_owned()
    }
}

const INSTRUCTIONS: &str = "\
The participant labelled \"You\" is closing this meeting now. Audit what still needs resolving \
while everyone is present. This is a gap audit, not a summary: list only what is unresolved.

Look for: topics never discussed that the meeting needed (missing); questions only partly \
answered, assumptions nobody confirmed, decisions discussed but never made (clarify); commitments \
with no owner or no date; promises later weakened; things the other side still owes; things You \
owe; contradictions (conflict).

Rules:
- Each gap is one short, plain sentence.
- Cite the evidence IDs each gap rests on, exactly as shown. A \"missing\" gap is about something \
nobody said, so it may cite nothing; every other gap cites at least one ID.
- List related state item ids in related_items when a gap is about an item.
- Most important first. If nothing is outstanding, return {\"gaps\": []}.";

/// The output schema of an audit.
pub fn audit_schema() -> Value {
    let categories: Vec<Value> = GapCategory::ALL
        .iter()
        .map(|c| Value::from(c.as_str()))
        .collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["gaps"],
        "properties": {
            "gaps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["category", "text", "source_refs", "related_items"],
                    "properties": {
                        "category": {"type": "string", "enum": categories},
                        "text": {"type": "string"},
                        "source_refs": {"type": "array", "items": {"type": "string"}},
                        "related_items": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }
    })
}

/// What an audit works from.
#[derive(Debug, Clone)]
pub struct AuditInput<'a> {
    pub transcript: &'a [TranscriptLine],
    pub state: &'a MeetingState,
    pub retrieved: &'a [Snippet],
    pub focus: Option<&'a str>,
    pub timeout: Duration,
}

/// Builds the audit request: every state item (history included, to catch weakened promises), the
/// latest transcript within [`AUDIT_TRANSCRIPT_CHARS`], and retrieved context.
pub fn prepare_audit(input: &AuditInput) -> (ReasoningRequest, EvidencePacket) {
    let mut packet = EvidencePacket::default();
    let cost = |l: &TranscriptLine| l.text.chars().count() + 24;
    let mut budget = AUDIT_TRANSCRIPT_CHARS;
    let mut start = input.transcript.len();
    for line in input.transcript.iter().rev() {
        let c = cost(line);
        if start < input.transcript.len() && c > budget {
            break;
        }
        budget = budget.saturating_sub(c);
        start -= 1;
    }
    let transcript = render_lines(
        &mut packet,
        &input.state.meeting_id,
        if start == 0 {
            "The meeting so far"
        } else {
            "The meeting so far (latest part)"
        },
        &input.transcript[start..],
    );
    let project = render_snippets(&mut packet, input.retrieved);

    let mut context = String::new();
    if let Some(focus) = input.focus.map(str::trim).filter(|f| !f.is_empty()) {
        let _ = writeln!(context, "## What You wanted from this meeting\n\n{focus}\n");
    }
    context.push_str(&crate::ask::render_items_for(input.state, &packet));
    context.push_str(&project);
    context.push_str(&transcript);

    (
        ReasoningRequest {
            task: TaskKind::EndgameAudit,
            instructions: INSTRUCTIONS.to_owned(),
            context,
            output_schema: audit_schema(),
            timeout: input.timeout,
        },
        packet,
    )
}

/// Runs the gap audit and checks every gap.
pub fn audit(
    backend: &dyn ReasoningBackend,
    cancel: &CancelToken,
    input: &AuditInput,
) -> Result<AuditReport, IntelError> {
    let (request, packet) = prepare_audit(input);
    let response = backend.invoke(&request, cancel)?;
    let raw: RawAudit = serde_json::from_value(response.output)
        .map_err(|e| IntelError::BadOutput(e.to_string()))?;
    let mut report = AuditReport {
        gaps: Vec::new(),
        rejected: Vec::new(),
    };
    for g in raw.gaps.into_iter().take(MAX_GAPS) {
        match check_gap(&g, &packet, input.state) {
            Ok(gap) => report.gaps.push(gap),
            Err(reason) => report.rejected.push((g.text, reason.to_string())),
        }
    }
    Ok(report)
}

fn check_gap(
    raw: &RawGap,
    packet: &EvidencePacket,
    state: &MeetingState,
) -> Result<Gap, RejectReason> {
    let text = raw.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(RejectReason::EmptyText);
    }
    let n = text.chars().count();
    if n > crate::reducer::MAX_TEXT_CHARS {
        return Err(RejectReason::TextTooLong(n));
    }
    if raw.source_refs.is_empty() && raw.category != GapCategory::Missing {
        return Err(RejectReason::NoEvidence);
    }
    let mut source_refs = Vec::new();
    let mut cited = Vec::new();
    for alias in &raw.source_refs {
        let r = packet
            .resolve(alias)
            .ok_or_else(|| RejectReason::UnknownEvidence(alias.clone()))?;
        if !source_refs.contains(r) {
            source_refs.push(r.clone());
            cited.push(alias.trim().to_owned());
        }
    }
    let mut related_items = Vec::new();
    for id in &raw.related_items {
        let id = id.trim();
        if state.item(id).is_none() {
            return Err(RejectReason::UnknownItem(id.to_owned()));
        }
        if !related_items.iter().any(|r: &String| r == id) {
            related_items.push(id.to_owned());
        }
    }
    Ok(Gap {
        category: raw.category,
        text,
        source_refs,
        cited,
        related_items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpistemicStatus, ItemKind};
    use crate::ops::{AppliedOp, ResolvedOp};
    use wisp_reasoning::ScriptedBackend;

    fn line(idx: i64, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 1000,
            speaker: "Them".into(),
            text: text.into(),
        }
    }

    fn state() -> MeetingState {
        let log = vec![AppliedOp {
            seq: 0,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: "COM-1".into(),
                kind: ItemKind::Commitment,
                text: "Send the revised architecture".into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                source_refs: vec!["Mlive:T1".into()],
                related_items: vec![],
                owner: Some("You".into()),
                due: None,
            },
        }];
        MeetingState::replay("live", &log).unwrap()
    }

    fn input<'a>(t: &'a [TranscriptLine], s: &'a MeetingState) -> AuditInput<'a> {
        AuditInput {
            transcript: t,
            state: s,
            retrieved: &[],
            focus: Some("Scope the migration"),
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn ordinary_talk_is_not_a_wrap_up() {
        let lines = vec![
            line(0, "Let's talk about the migration plan."),
            line(1, "We need the traffic numbers first."),
        ];
        assert!(wrap_probability(&lines) < 0.01);
        assert_eq!(wrap_probability(&[]), 0.0);
    }

    #[test]
    fn closing_phrases_add_up_to_a_wrap_up() {
        let one_weak = vec![line(0, "Next steps are on me.")];
        assert!(wrap_probability(&one_weak) < WRAP_SUGGEST_AT);
        let closing = vec![
            line(0, "Anything else before we wrap?"),
            line(1, "No, I think that's it. I’ll send the notes."),
        ];
        let p = wrap_probability(&closing);
        assert!(p >= WRAP_SUGGEST_AT, "{p}");
        assert!(p < 1.0);
        // Only the latest lines count.
        let mut old = closing.clone();
        old.extend((2..30).map(|i| line(i, "Back to the database schema.")));
        assert!(wrap_probability(&old) < 0.01);
    }

    #[test]
    fn the_audit_request_is_a_gap_audit_with_state_history_and_transcript() {
        let t = vec![
            line(0, "Hello"),
            line(1, "I'll send the revised architecture."),
        ];
        let s = state();
        let (req, packet) = prepare_audit(&input(&t, &s));
        assert_eq!(req.task, TaskKind::EndgameAudit);
        assert!(req.instructions.contains("gap audit, not a summary"));
        assert!(req
            .context
            .contains("## What You wanted from this meeting\n\nScope the migration"));
        assert!(req.context.contains("COM-1 [commitment"));
        assert!(req
            .context
            .contains("## The meeting so far\n\n[T0] 00:00 Them: Hello"));
        assert_eq!(packet.resolve("T1").map(String::as_str), Some("Mlive:T1"));
    }

    #[test]
    fn gaps_are_checked_and_only_missing_ones_may_cite_nothing() {
        let t = vec![
            line(0, "Hello"),
            line(1, "I'll send the revised architecture."),
        ];
        let s = state();
        let backend = ScriptedBackend::named("scripted");
        backend.push_ok(json!({"gaps": [
            {"category": "commitment_without_date", "text": "You will send the revised architecture.",
             "source_refs": ["T1"], "related_items": ["COM-1"]},
            {"category": "missing", "text": "Production deployment ownership was never established.",
             "source_refs": [], "related_items": []},
            {"category": "clarify", "text": "Migration scope unclear.", "source_refs": [], "related_items": []},
            {"category": "owed_by_them", "text": "Security questionnaire.", "source_refs": ["T9"], "related_items": []},
            {"category": "conflict", "text": "x", "source_refs": ["T0"], "related_items": ["REQ-4"]}
        ]}));
        let report = audit(&backend, &CancelToken::new(), &input(&t, &s)).unwrap();
        assert_eq!(report.gaps.len(), 2);
        assert_eq!(report.gaps[0].source_refs, ["Mlive:T1"]);
        assert_eq!(report.gaps[0].related_items, ["COM-1"]);
        assert!(report.gaps[1].cited.is_empty());
        let reasons: Vec<&str> = report.rejected.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(
            reasons,
            [
                "no evidence cited",
                "evidence T9 was not provided",
                "no item REQ-4"
            ]
        );

        let md = report.to_markdown();
        assert!(md.starts_with("## Before you wrap\n\n**Missing**\n- Production deployment ownership was never established."));
        assert!(md.contains(
            "**Commitment without date**\n- You will send the revised architecture. [T1]"
        ));
        assert!(
            md.find("**Missing**").unwrap() < md.find("**Commitment without date**").unwrap(),
            "categories in a fixed order"
        );
    }

    #[test]
    fn an_empty_audit_says_so() {
        let report = AuditReport {
            gaps: vec![],
            rejected: vec![],
        };
        assert_eq!(
            report.to_markdown(),
            "## Before you wrap\n\nNothing outstanding found."
        );
    }

    #[test]
    fn long_meetings_send_the_latest_part() {
        let long = "word ".repeat(1500);
        let t: Vec<TranscriptLine> = (0..20).map(|i| line(i, &long)).collect();
        let s = MeetingState::new("live");
        let (req, packet) = prepare_audit(&input(&t, &s));
        assert!(req.context.contains("(latest part)"));
        assert!(packet.resolve("T19").is_some());
        assert!(packet.resolve("T0").is_none());
    }
}

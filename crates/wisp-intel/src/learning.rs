//! Project learning: at the end of a meeting, propose what the project should remember.
//!
//! [`propose_learning`] asks for durable knowledge (facts, requirements, decisions, people and
//! roles, open issues) from the meeting's state and transcript, checks each proposal's evidence, and
//! drops what the project already knows. Follow-ups the user marked "save to project" in the review
//! join as stated proposals. Nothing is stored here: inferred proposals start unaccepted and the
//! user accepts or edits each one.
//!
//! Provenance is built to outlive the transcript: each evidence ref keeps a label ("Meeting Sept
//! 26 18:31, Them") and a SHA-256 of the evidence text, never the text itself, so deleting the
//! transcript under retention leaves no hidden verbatim copy behind.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use wisp_library::{MemoryEntry, MemoryInput, ProvenanceRef};
use wisp_reasoning::{CancelToken, ReasoningBackend, ReasoningRequest, TaskKind};

use crate::analyze::IntelError;
use crate::evidence::{render_lines, render_memory, EvidencePacket, TranscriptLine};
use crate::model::MeetingState;
use crate::review::{FollowUp, FollowUpClass};

/// Most proposals from one meeting.
pub const MAX_PROPOSALS: usize = 15;
const TRANSCRIPT_CHARS: usize = 30_000;
const MAX_TEXT_CHARS: usize = 300;

/// Kinds of project knowledge.
pub const KNOWLEDGE_KINDS: [&str; 5] = ["fact", "requirement", "decision", "person", "open_issue"];

/// A proposed piece of project knowledge, for the user to accept, edit or drop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub kind: String,
    pub text: String,
    /// `stated` or `inferred`.
    pub status: String,
    pub confidence: f64,
    pub provenance: Vec<ProvenanceRef>,
    /// Stated and confident proposals start accepted; inferred ones never do.
    pub accepted: bool,
}

impl Proposal {
    /// The entry to store once accepted.
    pub fn to_memory(&self, meeting_id: &str) -> MemoryInput {
        MemoryInput {
            kind: self.kind.clone(),
            text: self.text.clone(),
            status: self.status.clone(),
            confidence: self.confidence,
            provenance: self.provenance.clone(),
            meeting_id: Some(meeting_id.to_owned()),
        }
    }
}

const INSTRUCTIONS: &str = "\
The meeting is over. Propose what the project should remember beyond this meeting: durable facts, \
requirements, decisions, people and their roles, and open issues. Not tasks, not small talk, not \
things only relevant today. One short plain sentence each. status is \"stated\" when someone said it \
(or a document says it) and \"inferred\" when it follows from the evidence but nobody said it. \
confidence from 0 to 1. Cite the evidence IDs each rests on, exactly as shown; at least one each. \
Skip anything the accepted project knowledge already covers. At most 15, most important first.";

/// The output schema of a learning pass.
pub fn learning_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["knowledge"],
        "properties": {
            "knowledge": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "text", "status", "confidence", "source_refs"],
                    "properties": {
                        "kind": {"type": "string", "enum": KNOWLEDGE_KINDS},
                        "text": {"type": "string"},
                        "status": {"type": "string", "enum": ["stated", "inferred"]},
                        "confidence": {"type": "number"},
                        "source_refs": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }
    })
}

/// What a learning pass works from.
#[derive(Debug, Clone)]
pub struct LearningInput<'a> {
    pub state: &'a MeetingState,
    /// The saved meeting's lines (empty once pruned).
    pub transcript: &'a [TranscriptLine],
    /// What the project already knows.
    pub memory: &'a [MemoryEntry],
    /// Follow-ups from the review; those marked project memory become stated proposals.
    pub followups: &'a [FollowUp],
    /// How the meeting is named in provenance labels ("Meeting Sept 26").
    pub meeting_label: &'a str,
    pub timeout: Duration,
}

/// Builds the request and the packet its evidence is checked against.
pub fn prepare_learning(input: &LearningInput) -> (ReasoningRequest, EvidencePacket) {
    let mut packet = EvidencePacket::default();
    let mut budget = TRANSCRIPT_CHARS;
    let mut start = input.transcript.len();
    for line in input.transcript.iter().rev() {
        let c = line.text.chars().count() + 24;
        if start < input.transcript.len() && c > budget {
            break;
        }
        budget = budget.saturating_sub(c);
        start -= 1;
    }
    let lines = render_lines(
        &mut packet,
        &input.state.meeting_id,
        "The meeting",
        &input.transcript[start..],
    );
    let mut context = render_memory(&mut packet, input.memory);
    context.push_str(&crate::ask::render_items_for(input.state, &packet));
    context.push_str(&lines);
    (
        ReasoningRequest {
            task: TaskKind::ProjectLearning,
            instructions: INSTRUCTIONS.to_owned(),
            context,
            output_schema: learning_schema(),
            timeout: input.timeout,
        },
        packet,
    )
}

/// Proposes project knowledge: the review's "save to project" follow-ups first, then the model's
/// checked proposals. Proposals already covered by the project's memory, or repeating each other,
/// are dropped.
pub fn propose_learning(
    backend: &dyn ReasoningBackend,
    cancel: &CancelToken,
    input: &LearningInput,
) -> Result<Vec<Proposal>, IntelError> {
    #[derive(Deserialize)]
    struct Raw {
        knowledge: Vec<RawKnowledge>,
    }
    #[derive(Deserialize)]
    struct RawKnowledge {
        kind: String,
        text: String,
        status: String,
        confidence: f64,
        source_refs: Vec<String>,
    }
    let (request, packet) = prepare_learning(input);
    let mut known: BTreeSet<String> = input.memory.iter().map(|m| key(&m.text)).collect();
    let mut out = Vec::new();

    for f in input
        .followups
        .iter()
        .filter(|f| f.class == FollowUpClass::ProjectMemory)
    {
        let provenance: Vec<ProvenanceRef> = f
            .source_refs
            .iter()
            .filter_map(|r| packet.alias_for(r))
            .filter_map(|alias| provenance_for(&packet, alias, input.meeting_label))
            .collect();
        if known.insert(key(&f.text)) {
            out.push(Proposal {
                kind: "fact".into(),
                text: f.text.clone(),
                status: "stated".into(),
                confidence: 1.0,
                provenance,
                accepted: true,
            });
        }
    }

    let response = backend.invoke(&request, cancel)?;
    let raw: Raw = serde_json::from_value(response.output)
        .map_err(|e| IntelError::BadOutput(e.to_string()))?;
    for k in raw.knowledge {
        if out.len() >= MAX_PROPOSALS {
            break;
        }
        let text = k.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let valid = !text.is_empty()
            && text.chars().count() <= MAX_TEXT_CHARS
            && KNOWLEDGE_KINDS.contains(&k.kind.as_str())
            && matches!(k.status.as_str(), "stated" | "inferred")
            && k.confidence.is_finite()
            && (0.0..=1.0).contains(&k.confidence)
            && !k.source_refs.is_empty();
        if !valid {
            continue;
        }
        // Evidence from the meeting only: accepted knowledge can't be its own source.
        let Some(provenance) = k
            .source_refs
            .iter()
            .map(|alias| {
                if alias.trim().starts_with('P') {
                    None
                } else {
                    provenance_for(&packet, alias, input.meeting_label)
                }
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if !known.insert(key(&text)) {
            continue;
        }
        out.push(Proposal {
            accepted: k.status == "stated" && k.confidence >= 0.8,
            kind: k.kind,
            text,
            status: k.status,
            confidence: k.confidence,
            provenance,
        });
    }
    Ok(out)
}

/// Provenance for one cited ID: its canonical ref, a label naming the meeting, and a hash of the
/// evidence text.
fn provenance_for(
    packet: &EvidencePacket,
    alias: &str,
    meeting_label: &str,
) -> Option<ProvenanceRef> {
    let source_ref = packet.resolve(alias)?.clone();
    let detail = packet.detail(alias)?;
    Some(ProvenanceRef {
        source_ref,
        label: detail.label.replacen("This meeting", meeting_label, 1),
        sha256: Sha256::digest(detail.text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    })
}

fn key(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_reasoning::ScriptedBackend;

    fn line(idx: i64, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 60_000,
            speaker: "Them".into(),
            text: text.into(),
        }
    }

    fn memory(id: i64, text: &str) -> MemoryEntry {
        MemoryEntry {
            id,
            project_id: "p".into(),
            kind: "fact".into(),
            text: text.into(),
            status: "stated".into(),
            confidence: 1.0,
            provenance: vec![],
            meeting_id: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn knowledge(kind: &str, text: &str, status: &str, confidence: f64, refs: &[&str]) -> Value {
        json!({"kind": kind, "text": text, "status": status, "confidence": confidence, "source_refs": refs})
    }

    #[test]
    fn proposals_are_checked_deduplicated_and_carry_hashed_provenance() {
        let state = MeetingState::new("m1");
        let transcript = vec![
            line(0, "Production has to run in our own Azure tenant."),
            line(1, "Sarah owns security approval."),
        ];
        let known = vec![memory(3, "The customer is Acme Corp")];
        let followups = vec![FollowUp {
            n: 6,
            text: "Azure deployment is required".into(),
            class: FollowUpClass::ProjectMemory,
            owner: None,
            due: None,
            source_refs: vec!["Mm1:T0".into()],
            item_id: None,
        }];
        let backend = ScriptedBackend::named("scripted");
        backend.push_ok(json!({"knowledge": [
            knowledge("requirement", "Production runs in the customer's Azure tenant", "stated", 0.95, &["T0"]),
            knowledge("person", "Sarah owns security approval", "stated", 0.9, &["T1"]),
            knowledge("decision", "Customer-hosted deployment is likely", "inferred", 0.84, &["T0", "T1"]),
            knowledge("fact", "The customer is Acme Corp.", "stated", 0.9, &["T0"]),
            knowledge("fact", "Made up", "stated", 0.9, &["T9"]),
            knowledge("fact", "Circular", "stated", 0.9, &["P3"]),
            knowledge("fact", "No evidence", "stated", 0.9, &[])
        ]}));
        let input = LearningInput {
            state: &state,
            transcript: &transcript,
            memory: &known,
            followups: &followups,
            meeting_label: "Meeting Sept 26",
            timeout: Duration::from_secs(9),
        };
        let out = propose_learning(&backend, &CancelToken::new(), &input).unwrap();
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Azure deployment is required",
                "Production runs in the customer's Azure tenant",
                "Sarah owns security approval",
                "Customer-hosted deployment is likely",
            ]
        );
        assert!(out[0].accepted && out[1].accepted && out[2].accepted);
        assert!(
            !out[3].accepted,
            "inferred knowledge needs explicit acceptance"
        );

        let p = &out[1].provenance[0];
        assert_eq!(p.source_ref, "Mm1:T0");
        assert_eq!(p.label, "Meeting Sept 26 00:00, Them");
        assert_eq!(p.sha256.len(), 64);
        assert!(
            !serde_json::to_string(&out)
                .unwrap()
                .contains("own Azure tenant"),
            "no verbatim evidence"
        );
        assert_eq!(
            out[0].provenance[0].source_ref, "Mm1:T0",
            "review follow-ups keep their evidence"
        );

        let req = &backend.requests.lock().unwrap()[0];
        assert_eq!(req.task, TaskKind::ProjectLearning);
        assert!(req
            .context
            .contains("[P3] fact (stated): The customer is Acme Corp"));
        let m = out[1].to_memory("m1");
        assert_eq!(
            (m.kind.as_str(), m.meeting_id.as_deref()),
            ("requirement", Some("m1"))
        );
    }

    #[test]
    fn a_failed_call_is_an_error() {
        let state = MeetingState::new("m1");
        let backend = ScriptedBackend::named("scripted");
        backend.push_err(wisp_reasoning::ReasoningError::Timeout(
            Duration::from_secs(1),
        ));
        let input = LearningInput {
            state: &state,
            transcript: &[],
            memory: &[],
            followups: &[],
            meeting_label: "Meeting",
            timeout: Duration::from_secs(9),
        };
        assert!(propose_learning(&backend, &CancelToken::new(), &input).is_err());
    }
}

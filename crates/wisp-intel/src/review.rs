//! Post-call review: after the meeting, the user decides what each follow-up is.
//!
//! [`generate_followups`] proposes a numbered list from the meeting's state and transcript, each
//! with a suggested class and checked evidence ([`fallback_followups`] derives one from the state
//! alone when no model is available). The user corrects it in plain words: "1 and 4 are mine. 2
//! should become an open question. 3 belongs to them. Drop 5. Save 6 to the project."
//! [`parse_reply`] understands that kind of reply locally; [`interpret_reply`] asks a model when it
//! can't, and the edits it returns are checked like anything else. [`review_ops`] turns the final
//! classes into state operations, so the meeting's structured state changes, not a summary.

use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_reasoning::{CancelToken, ReasoningBackend, ReasoningRequest, TaskKind};

use crate::analyze::IntelError;
use crate::evidence::{render_lines, EvidencePacket, TranscriptLine};
use crate::model::{EpistemicStatus, ItemKind, Lifecycle, MeetingState, SourceRef};
use crate::ops::ResolvedOp;

/// Most follow-ups in one review.
pub const MAX_FOLLOWUPS: usize = 20;
/// Most transcript characters sent when generating follow-ups (the latest lines).
const TRANSCRIPT_CHARS: usize = 30_000;

/// What a follow-up is, as the user decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpClass {
    /// You will do it.
    Mine,
    /// The other side will.
    Theirs,
    /// It's a question to resolve, not a task.
    OpenQuestion,
    /// Not actually a task: drop it.
    NotATask,
    /// Worth keeping as project knowledge.
    ProjectMemory,
}

impl FollowUpClass {
    const ALL: [FollowUpClass; 5] = [
        FollowUpClass::Mine,
        FollowUpClass::Theirs,
        FollowUpClass::OpenQuestion,
        FollowUpClass::NotATask,
        FollowUpClass::ProjectMemory,
    ];

    fn as_str(self) -> &'static str {
        match self {
            FollowUpClass::Mine => "mine",
            FollowUpClass::Theirs => "theirs",
            FollowUpClass::OpenQuestion => "open_question",
            FollowUpClass::NotATask => "not_a_task",
            FollowUpClass::ProjectMemory => "project_memory",
        }
    }
}

/// One follow-up in the review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUp {
    /// Its number in the list, from 1.
    pub n: usize,
    pub text: String,
    /// The class suggested, then as corrected by the user.
    pub class: FollowUpClass,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub source_refs: Vec<SourceRef>,
    /// The state item it is about, if any.
    pub item_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawFollowUp {
    text: String,
    suggested_class: FollowUpClass,
    owner: Option<String>,
    due: Option<String>,
    source_refs: Vec<String>,
    related_item: Option<String>,
}

fn class_enum() -> Vec<Value> {
    FollowUpClass::ALL
        .iter()
        .map(|c| Value::from(c.as_str()))
        .collect()
}

/// The output schema for generating follow-ups.
pub fn followups_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["followups"],
        "properties": {
            "followups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "suggested_class", "owner", "due", "source_refs", "related_item"],
                    "properties": {
                        "text": {"type": "string"},
                        "suggested_class": {"type": "string", "enum": class_enum()},
                        "owner": {"type": ["string", "null"]},
                        "due": {"type": ["string", "null"]},
                        "source_refs": {"type": "array", "items": {"type": "string"}},
                        "related_item": {"type": ["string", "null"]}
                    }
                }
            }
        }
    })
}

const GENERATE: &str = "\
The meeting is over. List the follow-ups that matter to the participant labelled \"You\": what \
the other side committed to do for You, what You committed to, decisions that affect the project, \
questions You still need answered, and facts worth keeping for the project. If the context has \
\"About You and this project\", judge what matters by it. Skip generic chatter, small talk, and \
anything You would not act on. One short imperative line each (\"Send Sarah the architecture \
diagram.\"). Suggest a class for each: mine, theirs, open_question, not_a_task, project_memory. \
Record owner and due only if said. Cite the evidence IDs each rests on, exactly as shown; every \
follow-up cites at least one. Put the state item id it is about in related_item, if any. Most \
important first, at most 20; fewer is fine.";

/// Builds the request that proposes follow-ups. `about` is what matters to the user here (see
/// [`crate::about_you`]).
pub fn prepare_followups(
    state: &MeetingState,
    transcript: &[TranscriptLine],
    about: Option<&str>,
    timeout: Duration,
) -> (ReasoningRequest, EvidencePacket) {
    let mut packet = EvidencePacket::default();
    let mut budget = TRANSCRIPT_CHARS;
    let mut start = transcript.len();
    for line in transcript.iter().rev() {
        let c = line.text.chars().count() + 24;
        if start < transcript.len() && c > budget {
            break;
        }
        budget = budget.saturating_sub(c);
        start -= 1;
    }
    let lines = render_lines(
        &mut packet,
        &state.meeting_id,
        "The meeting",
        &transcript[start..],
    );
    let mut context = crate::about::render_about(about);
    context.push_str(&crate::ask::render_items_for(state, &packet));
    context.push_str(&lines);
    (
        ReasoningRequest {
            task: TaskKind::PostCall,
            instructions: GENERATE.to_owned(),
            context,
            output_schema: followups_schema(),
            timeout,
            images: Vec::new(),
        },
        packet,
    )
}

/// Proposes follow-ups with a model, keeping only those whose evidence and item check out.
pub fn generate_followups(
    backend: &dyn ReasoningBackend,
    cancel: &CancelToken,
    state: &MeetingState,
    transcript: &[TranscriptLine],
    about: Option<&str>,
    timeout: Duration,
) -> Result<Vec<FollowUp>, IntelError> {
    #[derive(Deserialize)]
    struct Raw {
        followups: Vec<RawFollowUp>,
    }
    let (request, packet) = prepare_followups(state, transcript, about, timeout);
    let response = backend.invoke(&request, cancel)?;
    let raw: Raw = serde_json::from_value(response.output)
        .map_err(|e| IntelError::BadOutput(e.to_string()))?;
    let mut out = Vec::new();
    for f in raw.followups {
        let text = f.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || f.source_refs.is_empty() {
            continue;
        }
        let Some(refs) = f
            .source_refs
            .iter()
            .map(|a| packet.resolve(a).cloned())
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let item_id = f.related_item.map(|i| i.trim().to_owned());
        if item_id.as_deref().is_some_and(|i| state.item(i).is_none()) {
            continue;
        }
        out.push(FollowUp {
            n: out.len() + 1,
            text,
            class: f.suggested_class,
            owner: blank_none(f.owner),
            due: blank_none(f.due),
            source_refs: refs,
            item_id,
        });
        if out.len() == MAX_FOLLOWUPS {
            break;
        }
    }
    Ok(out)
}

fn blank_none(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Follow-ups straight from the state, for when no model is available: live commitments and task
/// candidates (mine or theirs by owner), then open questions.
pub fn fallback_followups(state: &MeetingState) -> Vec<FollowUp> {
    let mut out = Vec::new();
    for item in state.live_items() {
        let class = match item.kind {
            ItemKind::Commitment | ItemKind::TaskCandidate => {
                match item.owner.as_deref().map(str::to_lowercase).as_deref() {
                    Some("you") | Some("me") => FollowUpClass::Mine,
                    Some(_) => FollowUpClass::Theirs,
                    None => FollowUpClass::Mine,
                }
            }
            ItemKind::OpenQuestion => FollowUpClass::OpenQuestion,
            _ => continue,
        };
        if item.lifecycle == Lifecycle::Resolved {
            continue;
        }
        out.push(FollowUp {
            n: out.len() + 1,
            text: item.text.clone(),
            class,
            owner: item.owner.clone(),
            due: item.due.clone(),
            source_refs: item.source_refs.clone(),
            item_id: Some(item.id.clone()),
        });
        if out.len() == MAX_FOLLOWUPS {
            break;
        }
    }
    out
}

/// One change the user asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewEdit {
    pub n: usize,
    pub class: FollowUpClass,
}

/// Understands simple replies locally: clauses (split on `.`, `;`, new lines) that each name one
/// or more follow-up numbers and one class ("1 and 4 are mine", "3 belongs to them", "drop 5",
/// "save 6 to the project", "2 should become an open question"). Returns `None` if any clause
/// can't be read with certainty, so the caller can ask a model instead.
pub fn parse_reply(reply: &str, count: usize) -> Option<Vec<ReviewEdit>> {
    let mut edits = Vec::new();
    for clause in reply.split(['.', ';', '\n']) {
        let lower = clause.to_lowercase();
        let clause = lower.trim();
        if clause.is_empty() {
            continue;
        }
        let numbers = numbers_in(clause);
        if numbers.is_empty() || numbers.iter().any(|&n| n == 0 || n > count) {
            return None;
        }
        let class = class_in(clause)?;
        edits.extend(numbers.into_iter().map(|n| ReviewEdit { n, class }));
    }
    (!edits.is_empty()).then_some(edits)
}

fn numbers_in(clause: &str) -> Vec<usize> {
    clause
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty() && s.len() <= 2)
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// The single class a clause names, or `None` if it names none or several.
fn class_in(clause: &str) -> Option<FollowUpClass> {
    let words: Vec<&str> = clause
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    let has = |w: &str| words.contains(&w);
    let mut found = Vec::new();
    if clause.contains("open question") || has("question") || has("questions") {
        found.push(FollowUpClass::OpenQuestion);
    }
    if has("drop")
        || has("delete")
        || has("remove")
        || has("ignore")
        || clause.contains("not a task")
    {
        found.push(FollowUpClass::NotATask);
    }
    if has("project") || has("memory") {
        found.push(FollowUpClass::ProjectMemory);
    }
    if has("mine") || has("me") || has("i'll") || clause.contains("i will") {
        found.push(FollowUpClass::Mine);
    }
    if has("theirs") || has("them") || has("they") || has("they'll") {
        found.push(FollowUpClass::Theirs);
    }
    found.dedup();
    (found.len() == 1).then(|| found[0])
}

const INTERPRET: &str = "\
The user is correcting a numbered list of meeting follow-ups in plain words. Turn their reply \
into edits: for each follow-up they mention, its number and the class they mean (mine, theirs, \
open_question, not_a_task, project_memory). Only include follow-ups they clearly mention. If the \
reply isn't about the list, return no edits.";

/// Asks a model to turn a reply the local parser couldn't read into edits; numbers outside the list
/// are dropped.
pub fn interpret_reply(
    backend: &dyn ReasoningBackend,
    cancel: &CancelToken,
    followups: &[FollowUp],
    reply: &str,
    timeout: Duration,
) -> Result<Vec<ReviewEdit>, IntelError> {
    #[derive(Deserialize)]
    struct Raw {
        edits: Vec<RawEdit>,
    }
    #[derive(Deserialize)]
    struct RawEdit {
        n: i64,
        class: FollowUpClass,
    }
    let mut context = String::from("## Follow-ups\n\n");
    for f in followups {
        let _ = writeln!(context, "{}. {} ({})", f.n, f.text, f.class.as_str());
    }
    let _ = writeln!(context, "\n## Reply\n\n{}", reply.trim());
    let request = ReasoningRequest {
        task: TaskKind::PostCall,
        instructions: INTERPRET.to_owned(),
        context,
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["edits"],
            "properties": {"edits": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["n", "class"],
                "properties": {
                    "n": {"type": "integer"},
                    "class": {"type": "string", "enum": class_enum()}
                }
            }}}
        }),
        timeout,
        images: Vec::new(),
    };
    let response = backend.invoke(&request, cancel)?;
    let raw: Raw = serde_json::from_value(response.output)
        .map_err(|e| IntelError::BadOutput(e.to_string()))?;
    Ok(raw
        .edits
        .into_iter()
        .filter(|e| e.n >= 1 && (e.n as usize) <= followups.len())
        .map(|e| ReviewEdit {
            n: e.n as usize,
            class: e.class,
        })
        .collect())
}

/// Applies edits to the list (later edits win). Returns how many follow-ups changed class.
pub fn apply_edits(followups: &mut [FollowUp], edits: &[ReviewEdit]) -> usize {
    let mut changed = 0;
    for e in edits {
        if let Some(f) = followups.iter_mut().find(|f| f.n == e.n) {
            if f.class != e.class {
                f.class = e.class;
                changed += 1;
            }
        }
    }
    changed
}

/// The state operations the reviewed list implies, as user decisions (confidence 1, stated):
/// - mine / theirs: an existing commitment or task gets its owner ("You" or "Them" unless one was
///   named); otherwise a new commitment is added;
/// - open question: an open question is added (linked to the item, if any);
/// - not a task: the item it is about is withdrawn;
/// - project memory: nothing here; it is for project learning.
///
/// Ids continue from the state, so the ops apply in order after its log.
pub fn review_ops(state: &MeetingState, followups: &[FollowUp]) -> Vec<ResolvedOp> {
    let mut sim = state.clone();
    let mut ops = Vec::new();
    for f in followups {
        let item = f.item_id.as_deref().and_then(|id| sim.item(id)).cloned();
        let op = match f.class {
            FollowUpClass::Mine | FollowUpClass::Theirs => {
                let owner = match (f.class, &f.owner) {
                    (FollowUpClass::Theirs, Some(o)) if !o.eq_ignore_ascii_case("you") => o.clone(),
                    (FollowUpClass::Theirs, _) => "Them".to_owned(),
                    _ => "You".to_owned(),
                };
                match item.filter(|i| {
                    matches!(i.kind, ItemKind::Commitment | ItemKind::TaskCandidate)
                        && !i.lifecycle.is_terminal()
                }) {
                    Some(i) if i.owner.as_deref() == Some(owner.as_str()) => continue,
                    Some(i) => ResolvedOp::Update {
                        id: i.id,
                        text: None,
                        confidence: Some(1.0),
                        owner: Some(owner),
                        due: None,
                        add_refs: vec![],
                        add_related: vec![],
                    },
                    None => ResolvedOp::Add {
                        id: sim.next_id(ItemKind::Commitment),
                        kind: ItemKind::Commitment,
                        text: f.text.clone(),
                        status: EpistemicStatus::Stated,
                        confidence: 1.0,
                        source_refs: f.source_refs.clone(),
                        related_items: f.item_id.iter().cloned().collect(),
                        owner: Some(owner),
                        due: f.due.clone(),
                    },
                }
            }
            FollowUpClass::OpenQuestion => {
                if item
                    .as_ref()
                    .is_some_and(|i| i.kind == ItemKind::OpenQuestion)
                {
                    continue;
                }
                ResolvedOp::Add {
                    id: sim.next_id(ItemKind::OpenQuestion),
                    kind: ItemKind::OpenQuestion,
                    text: f.text.clone(),
                    status: EpistemicStatus::Stated,
                    confidence: 1.0,
                    source_refs: f.source_refs.clone(),
                    related_items: f.item_id.iter().cloned().collect(),
                    owner: None,
                    due: None,
                }
            }
            FollowUpClass::NotATask => match item.filter(|i| !i.lifecycle.is_terminal()) {
                Some(i) => ResolvedOp::SetLifecycle {
                    id: i.id,
                    lifecycle: Lifecycle::Withdrawn,
                    superseded_by: None,
                    add_refs: vec![],
                },
                None => continue,
            },
            FollowUpClass::ProjectMemory => continue,
        };
        let applied = crate::ops::AppliedOp {
            seq: sim.op_count,
            at_ms: 0,
            op: op.clone(),
        };
        if sim.apply(&applied).is_ok() {
            ops.push(op);
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::AppliedOp;
    use wisp_reasoning::ScriptedBackend;

    fn state() -> MeetingState {
        let add = |seq: u64, id: &str, kind: ItemKind, text: &str, owner: Option<&str>| AppliedOp {
            seq,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: id.into(),
                kind,
                text: text.into(),
                status: EpistemicStatus::Stated,
                confidence: 0.8,
                source_refs: vec![format!("Mm:T{seq}")],
                related_items: vec![],
                owner: owner.map(str::to_owned),
                due: None,
            },
        };
        MeetingState::replay(
            "m",
            &[
                add(
                    0,
                    "COM-1",
                    ItemKind::Commitment,
                    "Send Sarah the architecture diagram",
                    Some("You"),
                ),
                add(
                    1,
                    "COM-2",
                    ItemKind::Commitment,
                    "Send traffic numbers",
                    Some("Customer"),
                ),
                add(2, "Q-1", ItemKind::OpenQuestion, "SAML or OIDC?", None),
                add(3, "REQ-1", ItemKind::Requirement, "Runs in Azure", None),
                add(
                    4,
                    "TASK-1",
                    ItemKind::TaskCandidate,
                    "Update the estimate",
                    None,
                ),
            ],
        )
        .unwrap()
    }

    fn line(idx: i64, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 1000,
            speaker: "Them".into(),
            text: text.into(),
        }
    }

    fn fu(n: usize, text: &str, class: FollowUpClass, item: Option<&str>) -> FollowUp {
        FollowUp {
            n,
            text: text.into(),
            class,
            owner: None,
            due: None,
            source_refs: vec!["Mm:T1".into()],
            item_id: item.map(str::to_owned),
        }
    }

    #[test]
    fn the_briefs_reply_parses_locally() {
        let edits = parse_reply(
            "1 and 4 are mine. 2 should become an open question. 3 belongs to them. Drop 5. Save 6 to the project.",
            6,
        )
        .unwrap();
        let got: Vec<(usize, FollowUpClass)> = edits.iter().map(|e| (e.n, e.class)).collect();
        assert_eq!(
            got,
            [
                (1, FollowUpClass::Mine),
                (4, FollowUpClass::Mine),
                (2, FollowUpClass::OpenQuestion),
                (3, FollowUpClass::Theirs),
                (5, FollowUpClass::NotATask),
                (6, FollowUpClass::ProjectMemory),
            ]
        );
        assert_eq!(
            parse_reply("I'll take 2; 3 is not a task", 3)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn unclear_replies_are_left_to_the_model() {
        assert_eq!(parse_reply("looks good", 3), None, "no numbers");
        assert_eq!(parse_reply("7 is mine", 3), None, "out of range");
        assert_eq!(parse_reply("2 is for Sarah", 3), None, "no known class");
        assert_eq!(
            parse_reply("give 2 to them, not me", 3),
            None,
            "two classes"
        );
        assert_eq!(parse_reply("", 3), None);
    }

    #[test]
    fn edits_change_classes_and_count_real_changes() {
        let mut list = vec![
            fu(1, "a", FollowUpClass::Mine, None),
            fu(2, "b", FollowUpClass::Mine, None),
        ];
        let n = apply_edits(
            &mut list,
            &[
                ReviewEdit {
                    n: 1,
                    class: FollowUpClass::Mine,
                },
                ReviewEdit {
                    n: 2,
                    class: FollowUpClass::Theirs,
                },
                ReviewEdit {
                    n: 9,
                    class: FollowUpClass::Theirs,
                },
            ],
        );
        assert_eq!(n, 1);
        assert_eq!(list[1].class, FollowUpClass::Theirs);
    }

    #[test]
    fn reviewed_classes_become_state_changes() {
        let s = state();
        let list = vec![
            fu(
                1,
                "Send Sarah the architecture diagram",
                FollowUpClass::Mine,
                Some("COM-1"),
            ),
            fu(
                2,
                "Update the estimate",
                FollowUpClass::Theirs,
                Some("TASK-1"),
            ),
            fu(
                3,
                "Confirm SAML or OIDC",
                FollowUpClass::OpenQuestion,
                Some("Q-1"),
            ),
            fu(
                4,
                "Schedule a technical validation session",
                FollowUpClass::Mine,
                None,
            ),
            fu(
                5,
                "Send traffic numbers",
                FollowUpClass::NotATask,
                Some("COM-2"),
            ),
            fu(
                6,
                "Customer runs Azure",
                FollowUpClass::ProjectMemory,
                Some("REQ-1"),
            ),
            fu(
                7,
                "Is retention 90 days?",
                FollowUpClass::OpenQuestion,
                None,
            ),
        ];
        let ops = review_ops(&s, &list);
        // 1: already mine → nothing. 3: already a question → nothing. 6: project learning → nothing.
        assert_eq!(ops.len(), 4, "{ops:#?}");
        assert!(
            matches!(&ops[0], ResolvedOp::Update { id, owner: Some(o), .. } if id == "TASK-1" && o == "Them")
        );
        assert!(
            matches!(&ops[1], ResolvedOp::Add { id, kind: ItemKind::Commitment, owner: Some(o), confidence, .. }
            if id == "COM-3" && o == "You" && *confidence == 1.0)
        );
        assert!(
            matches!(&ops[2], ResolvedOp::SetLifecycle { id, lifecycle: Lifecycle::Withdrawn, .. } if id == "COM-2")
        );
        assert!(
            matches!(&ops[3], ResolvedOp::Add { id, kind: ItemKind::OpenQuestion, .. } if id == "Q-2")
        );

        // They apply in order after the state's own log.
        let mut after = s.clone();
        for op in ops {
            after
                .apply(&AppliedOp {
                    seq: after.op_count,
                    at_ms: 1,
                    op,
                })
                .unwrap();
        }
        assert_eq!(after.item("COM-2").unwrap().lifecycle, Lifecycle::Withdrawn);
        assert_eq!(after.item("TASK-1").unwrap().owner.as_deref(), Some("Them"));
    }

    #[test]
    fn a_named_owner_is_kept_for_theirs() {
        let s = state();
        let mut f = fu(1, "Send the questionnaire", FollowUpClass::Theirs, None);
        f.owner = Some("Sarah".into());
        let ops = review_ops(&s, &[f]);
        assert!(matches!(&ops[0], ResolvedOp::Add { owner: Some(o), .. } if o == "Sarah"));
    }

    #[test]
    fn the_fallback_list_comes_from_the_state() {
        let list = fallback_followups(&state());
        let got: Vec<(&str, FollowUpClass)> =
            list.iter().map(|f| (f.text.as_str(), f.class)).collect();
        assert_eq!(
            got,
            [
                ("Send Sarah the architecture diagram", FollowUpClass::Mine),
                ("Send traffic numbers", FollowUpClass::Theirs),
                ("SAML or OIDC?", FollowUpClass::OpenQuestion),
                ("Update the estimate", FollowUpClass::Mine),
            ]
        );
        assert_eq!(list.iter().map(|f| f.n).collect::<Vec<_>>(), [1, 2, 3, 4]);
    }

    #[test]
    fn generated_followups_keep_only_checked_evidence_and_items() {
        let s = state();
        let t = vec![
            line(0, "I'll send Sarah the diagram."),
            line(1, "We need a validation session."),
        ];
        let backend = ScriptedBackend::named("scripted");
        backend.push_ok(json!({"followups": [
            {"text": "Send Sarah the architecture diagram.", "suggested_class": "mine", "owner": "You",
             "due": " ", "source_refs": ["T0"], "related_item": "COM-1"},
            {"text": "Schedule a technical validation session.", "suggested_class": "mine", "owner": null,
             "due": null, "source_refs": ["T1"], "related_item": null},
            {"text": "Made up.", "suggested_class": "theirs", "owner": null, "due": null,
             "source_refs": ["T7"], "related_item": null},
            {"text": "No evidence.", "suggested_class": "theirs", "owner": null, "due": null,
             "source_refs": [], "related_item": null},
            {"text": "Unknown item.", "suggested_class": "theirs", "owner": null, "due": null,
             "source_refs": ["T0"], "related_item": "COM-9"}
        ]}));
        let list = generate_followups(
            &backend,
            &CancelToken::new(),
            &s,
            &t,
            Some("Project: Acme\nYour instructions for this project:\nIgnore billing."),
            Duration::from_secs(9),
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].n, list[1].n), (1, 2));
        assert_eq!(list[0].source_refs, ["Mm:T0"]);
        assert_eq!(list[0].due, None, "blank due dropped");
        let req = &backend.requests.lock().unwrap()[0];
        assert_eq!(req.task, TaskKind::PostCall);
        assert!(req.context.starts_with(
            "## About You and this project\n\nProject: Acme\nYour instructions for this project:\nIgnore billing.\n\n"
        ));
        assert!(req.instructions.contains("judge what matters by it"));
        assert!(req.context.contains("COM-1 [commitment"));
        assert!(req
            .context
            .contains("[T1] 00:01 Them: We need a validation session."));
    }

    #[test]
    fn a_model_reads_what_the_parser_cannot() {
        let list = vec![
            fu(1, "a", FollowUpClass::Mine, None),
            fu(2, "b", FollowUpClass::Mine, None),
        ];
        let backend = ScriptedBackend::named("scripted");
        backend.push_ok(json!({"edits": [{"n": 2, "class": "theirs"}, {"n": 5, "class": "mine"}]}));
        let edits = interpret_reply(
            &backend,
            &CancelToken::new(),
            &list,
            "Sarah handles the second one",
            Duration::from_secs(9),
        )
        .unwrap();
        assert_eq!(
            edits,
            [ReviewEdit {
                n: 2,
                class: FollowUpClass::Theirs
            }]
        );
        let ctx = &backend.requests.lock().unwrap()[0].context;
        assert!(
            ctx.contains("2. b (mine)") && ctx.contains("## Reply\n\nSarah handles the second one")
        );
    }
}

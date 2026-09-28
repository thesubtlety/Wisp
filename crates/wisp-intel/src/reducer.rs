//! The deterministic reducer: validates a model's proposed ops against the evidence it was given
//! and the current state, applies the valid ones in order, and reports every rejection with its
//! reason. Nothing a model returns reaches the state without passing through here.
//!
//! Rules:
//! - every change cites evidence, and every cited ID must be in the packet (unknown IDs reject the
//!   whole op, so provenance can't be fabricated);
//! - item ids are assigned here, never by the model; a batch may name new items with `temp_id`;
//! - decisions, requirements, commitments and the like are stated or inferred, never suggested;
//! - superseded and withdrawn items no longer change, and superseding needs a live replacement of
//!   the same kind;
//! - an `add` that repeats a live item of the same kind merges into it.

use std::collections::HashMap;
use std::fmt;

use crate::evidence::EvidencePacket;
use crate::model::{ItemKind, Lifecycle, MeetingState, SourceRef};
use crate::ops::{AppliedOp, ModelOp, OpBatch, OpKind, ResolvedOp};

/// Most ops taken from one batch; the rest are rejected.
pub const MAX_OPS_PER_BATCH: usize = 40;
/// Longest item text accepted, in characters.
pub const MAX_TEXT_CHARS: usize = 400;
/// Longest owner or due value kept, in characters.
const MAX_FIELD_CHARS: usize = 80;

/// Why a proposed op was not applied.
#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    TooManyOps,
    Missing(&'static str),
    EmptyText,
    TextTooLong(usize),
    BadConfidence(f64),
    NoEvidence,
    UnknownEvidence(String),
    UnknownItem(String),
    DuplicateTempId(String),
    SuggestedNotAllowed(ItemKind),
    TerminalItem(String),
    SupersedeNeedsReplacement,
    BadReplacement(String),
    /// An `add` that repeats a live item and brings nothing new.
    Duplicate(String),
    NoChange(String),
    /// The state itself is inconsistent (edited outside this crate).
    Inconsistent(String),
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::TooManyOps => write!(f, "over {MAX_OPS_PER_BATCH} ops in one batch"),
            RejectReason::Missing(field) => write!(f, "missing {field}"),
            RejectReason::EmptyText => write!(f, "empty text"),
            RejectReason::TextTooLong(n) => write!(f, "text too long ({n} chars)"),
            RejectReason::BadConfidence(c) => write!(f, "confidence {c} outside 0..1"),
            RejectReason::NoEvidence => write!(f, "no evidence cited"),
            RejectReason::UnknownEvidence(id) => write!(f, "evidence {id} was not provided"),
            RejectReason::UnknownItem(id) => write!(f, "no item {id}"),
            RejectReason::DuplicateTempId(id) => write!(f, "temp_id {id} already used"),
            RejectReason::SuggestedNotAllowed(k) => {
                write!(f, "a {} can't be merely suggested", k.as_str())
            }
            RejectReason::TerminalItem(id) => write!(f, "{id} is superseded or withdrawn"),
            RejectReason::SupersedeNeedsReplacement => write!(f, "superseded needs superseded_by"),
            RejectReason::BadReplacement(id) => write!(f, "{id} can't replace this item"),
            RejectReason::Duplicate(id) => write!(f, "repeats {id} with nothing new"),
            RejectReason::NoChange(id) => write!(f, "changes nothing on {id}"),
            RejectReason::Inconsistent(e) => write!(f, "state is inconsistent: {e}"),
        }
    }
}

/// A rejected op: its position in the batch and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub index: usize,
    pub reason: RejectReason,
}

/// What one batch did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyReport {
    /// The ops applied, in order; append these to the meeting's log.
    pub applied: Vec<AppliedOp>,
    /// How many of the applied ops are `add`s merged into an existing item.
    pub merged: usize,
    pub rejected: Vec<Rejection>,
}

/// Validates and applies `batch` to `state`. Ops apply one at a time, in order, so a later op can
/// build on an earlier one (by `temp_id`); an invalid op is skipped without affecting the rest.
pub fn reduce(
    state: &mut MeetingState,
    batch: &OpBatch,
    packet: &EvidencePacket,
    now_ms: i64,
) -> ApplyReport {
    let mut report = ApplyReport::default();
    let mut temps: HashMap<String, String> = HashMap::new();
    for (index, op) in batch.ops.iter().enumerate() {
        if index >= MAX_OPS_PER_BATCH {
            report.rejected.push(Rejection {
                index,
                reason: RejectReason::TooManyOps,
            });
            continue;
        }
        let mut ctx = Ctx {
            state,
            packet,
            temps: &mut temps,
        };
        let outcome = match op.op {
            OpKind::Add => ctx.add(op),
            OpKind::Update => ctx.update(op),
            OpKind::SetLifecycle => ctx.set_lifecycle(op),
        };
        match outcome {
            Ok((resolved, merged)) => {
                let applied = AppliedOp {
                    seq: state.op_count,
                    at_ms: now_ms,
                    op: resolved,
                };
                // Validated ops always apply to a state built by this crate; a state edited by hand
                // might not, and that is a rejection, not a crash.
                match state.apply(&applied) {
                    Ok(()) => {
                        report.merged += usize::from(merged);
                        report.applied.push(applied);
                    }
                    Err(e) => report.rejected.push(Rejection {
                        index,
                        reason: RejectReason::Inconsistent(e.to_string()),
                    }),
                }
            }
            Err(reason) => report.rejected.push(Rejection { index, reason }),
        }
    }
    report
}

struct Ctx<'a> {
    state: &'a MeetingState,
    packet: &'a EvidencePacket,
    temps: &'a mut HashMap<String, String>,
}

type Outcome = Result<(ResolvedOp, bool), RejectReason>;

impl Ctx<'_> {
    fn add(&mut self, op: &ModelOp) -> Outcome {
        let kind = op.kind.ok_or(RejectReason::Missing("kind"))?;
        let text = clean_text(op.text.as_deref())?;
        let status = op
            .epistemic_status
            .ok_or(RejectReason::Missing("epistemic_status"))?;
        let confidence = confidence(op.confidence.ok_or(RejectReason::Missing("confidence"))?)?;
        if status == crate::model::EpistemicStatus::Suggested && !kind.allows_suggested() {
            return Err(RejectReason::SuggestedNotAllowed(kind));
        }
        let refs = self.evidence(&op.source_refs)?;
        let related = self.items(&op.related_items)?;
        if let Some(temp) = &op.temp_id {
            if self.temps.contains_key(temp) || self.state.items.contains_key(temp) {
                return Err(RejectReason::DuplicateTempId(temp.clone()));
            }
        }

        let key = dedupe_key(&text);
        let existing =
            self.state.items.values().find(|i| {
                i.kind == kind && !i.lifecycle.is_terminal() && dedupe_key(&i.text) == key
            });
        if let Some(item) = existing {
            let id = item.id.clone();
            if let Some(temp) = &op.temp_id {
                self.temps.insert(temp.clone(), id.clone());
            }
            let add_refs: Vec<SourceRef> = refs
                .into_iter()
                .filter(|r| !item.source_refs.contains(r))
                .collect();
            let add_related: Vec<String> = related
                .into_iter()
                .filter(|r| !item.related_items.contains(r) && *r != id)
                .collect();
            let confidence = (confidence > item.confidence).then_some(confidence);
            if add_refs.is_empty() && add_related.is_empty() && confidence.is_none() {
                return Err(RejectReason::Duplicate(id));
            }
            return Ok((
                ResolvedOp::Update {
                    id,
                    text: None,
                    confidence,
                    owner: None,
                    due: None,
                    add_refs,
                    add_related,
                },
                true,
            ));
        }

        let id = self.state.next_id(kind);
        if let Some(temp) = &op.temp_id {
            self.temps.insert(temp.clone(), id.clone());
        }
        Ok((
            ResolvedOp::Add {
                id,
                kind,
                text,
                status,
                confidence,
                source_refs: refs,
                related_items: related,
                owner: clean_field(op.owner.as_deref()),
                due: clean_field(op.due.as_deref()),
            },
            false,
        ))
    }

    fn update(&mut self, op: &ModelOp) -> Outcome {
        let id = self.live_item(op.id.as_deref())?;
        let item = &self.state.items[&id];
        let refs = self.evidence(&op.source_refs)?;
        let related = self.items(&op.related_items)?;
        let text = match &op.text {
            Some(t) => Some(clean_text(Some(t))?).filter(|t| *t != item.text),
            None => None,
        };
        let confidence = match op.confidence {
            Some(c) => Some(confidence(c)?).filter(|c| *c != item.confidence),
            None => None,
        };
        let owner = clean_field(op.owner.as_deref()).filter(|o| item.owner.as_ref() != Some(o));
        let due = clean_field(op.due.as_deref()).filter(|d| item.due.as_ref() != Some(d));
        let add_refs: Vec<SourceRef> = refs
            .into_iter()
            .filter(|r| !item.source_refs.contains(r))
            .collect();
        let add_related: Vec<String> = related
            .into_iter()
            .filter(|r| !item.related_items.contains(r) && *r != id)
            .collect();
        let changes = text.is_some()
            || confidence.is_some()
            || owner.is_some()
            || due.is_some()
            || !add_refs.is_empty()
            || !add_related.is_empty();
        if !changes {
            return Err(RejectReason::NoChange(id));
        }
        Ok((
            ResolvedOp::Update {
                id,
                text,
                confidence,
                owner,
                due,
                add_refs,
                add_related,
            },
            false,
        ))
    }

    fn set_lifecycle(&mut self, op: &ModelOp) -> Outcome {
        let id = self.live_item(op.id.as_deref())?;
        let lifecycle = op.lifecycle.ok_or(RejectReason::Missing("lifecycle"))?;
        let refs = self.evidence(&op.source_refs)?;
        let item = &self.state.items[&id];
        let superseded_by = if lifecycle == Lifecycle::Superseded {
            let by = op
                .superseded_by
                .as_deref()
                .ok_or(RejectReason::SupersedeNeedsReplacement)?;
            let by_id = self.resolve_item(by)?;
            let replacement = &self.state.items[&by_id];
            if by_id == id || replacement.kind != item.kind || replacement.lifecycle.is_terminal() {
                return Err(RejectReason::BadReplacement(by_id));
            }
            Some(by_id)
        } else {
            None
        };
        if lifecycle == item.lifecycle {
            return Err(RejectReason::NoChange(id));
        }
        let add_refs = refs
            .into_iter()
            .filter(|r| !item.source_refs.contains(r))
            .collect();
        Ok((
            ResolvedOp::SetLifecycle {
                id,
                lifecycle,
                superseded_by,
                add_refs,
            },
            false,
        ))
    }

    /// Canonical refs for cited IDs: at least one, all known.
    fn evidence(&self, cited: &[String]) -> Result<Vec<SourceRef>, RejectReason> {
        if cited.is_empty() {
            return Err(RejectReason::NoEvidence);
        }
        let mut out: Vec<SourceRef> = Vec::new();
        for alias in cited {
            let r = self
                .packet
                .resolve(alias)
                .ok_or_else(|| RejectReason::UnknownEvidence(alias.clone()))?;
            if !out.contains(r) {
                out.push(r.clone());
            }
        }
        Ok(out)
    }

    /// Real ids for cited items or temp_ids, all known.
    fn items(&self, cited: &[String]) -> Result<Vec<String>, RejectReason> {
        let mut out: Vec<String> = Vec::new();
        for c in cited {
            let id = self.resolve_item(c)?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
        Ok(out)
    }

    fn resolve_item(&self, id: &str) -> Result<String, RejectReason> {
        let id = id.trim();
        if let Some(real) = self.temps.get(id) {
            return Ok(real.clone());
        }
        if self.state.items.contains_key(id) {
            return Ok(id.to_owned());
        }
        Err(RejectReason::UnknownItem(id.to_owned()))
    }

    /// The target of an update or lifecycle change: present, known, and not terminal.
    fn live_item(&self, id: Option<&str>) -> Result<String, RejectReason> {
        let id = self.resolve_item(id.ok_or(RejectReason::Missing("id"))?)?;
        if self.state.items[&id].lifecycle.is_terminal() {
            return Err(RejectReason::TerminalItem(id));
        }
        Ok(id)
    }
}

/// Item text with whitespace collapsed; present, non-empty, and within [`MAX_TEXT_CHARS`].
fn clean_text(text: Option<&str>) -> Result<String, RejectReason> {
    let text = text.ok_or(RejectReason::Missing("text"))?;
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = cleaned.chars().count();
    if n == 0 {
        return Err(RejectReason::EmptyText);
    }
    if n > MAX_TEXT_CHARS {
        return Err(RejectReason::TextTooLong(n));
    }
    Ok(cleaned)
}

/// An owner or due value, trimmed and shortened; blank means absent.
fn clean_field(value: Option<&str>) -> Option<String> {
    let v = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!v.is_empty()).then(|| v.chars().take(MAX_FIELD_CHARS).collect())
}

fn confidence(c: f64) -> Result<f64, RejectReason> {
    if c.is_finite() && (0.0..=1.0).contains(&c) {
        Ok(c)
    } else {
        Err(RejectReason::BadConfidence(c))
    }
}

/// Text reduced to lower-case words, so trivially different phrasings of one item match.
fn dedupe_key(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{render_snippets, render_transcript, TranscriptLine};
    use crate::model::EpistemicStatus;
    use wisp_library::{Snippet, SnippetOrigin};

    const NOW: i64 = 1_000;

    /// A packet holding T0..T9 of meeting "m" and chunk 0 of source 17.
    fn packet() -> EvidencePacket {
        let mut p = EvidencePacket::default();
        let lines: Vec<TranscriptLine> = (0..10)
            .map(|idx| TranscriptLine {
                idx,
                start_ms: idx * 1000,
                speaker: "Them".into(),
                text: format!("line {idx}"),
            })
            .collect();
        render_transcript(&mut p, "m", &[], &lines);
        render_snippets(
            &mut p,
            &[Snippet {
                ref_id: "S17:C0".into(),
                origin: SnippetOrigin::Source {
                    source_id: 17,
                    chunk_idx: 0,
                    label: "sec.md".into(),
                    line_start: Some(1),
                },
                text: "EU only".into(),
                score: 1.0,
            }],
        );
        p
    }

    fn op(kind: OpKind) -> ModelOp {
        ModelOp {
            op: kind,
            id: None,
            temp_id: None,
            kind: None,
            text: None,
            epistemic_status: None,
            confidence: None,
            lifecycle: None,
            superseded_by: None,
            owner: None,
            due: None,
            source_refs: vec![],
            related_items: vec![],
        }
    }

    fn add(kind: ItemKind, text: &str, refs: &[&str]) -> ModelOp {
        ModelOp {
            kind: Some(kind),
            text: Some(text.into()),
            epistemic_status: Some(EpistemicStatus::Stated),
            confidence: Some(0.9),
            source_refs: refs.iter().map(|r| r.to_string()).collect(),
            ..op(OpKind::Add)
        }
    }

    fn lifecycle(id: &str, to: Lifecycle, by: Option<&str>, refs: &[&str]) -> ModelOp {
        ModelOp {
            id: Some(id.into()),
            lifecycle: Some(to),
            superseded_by: by.map(str::to_owned),
            source_refs: refs.iter().map(|r| r.to_string()).collect(),
            ..op(OpKind::SetLifecycle)
        }
    }

    fn run(state: &mut MeetingState, ops: Vec<ModelOp>) -> ApplyReport {
        reduce(state, &OpBatch { ops }, &packet(), NOW)
    }

    fn reasons(report: &ApplyReport) -> Vec<(usize, RejectReason)> {
        report
            .rejected
            .iter()
            .map(|r| (r.index, r.reason.clone()))
            .collect()
    }

    #[test]
    fn adds_get_ids_per_kind_and_canonical_refs() {
        let mut s = MeetingState::new("m");
        let r = run(
            &mut s,
            vec![
                add(
                    ItemKind::Requirement,
                    "  Production   runs in Azure ",
                    &["T1", "D17:C0", "T1"],
                ),
                add(ItemKind::Requirement, "Data stays in the EU", &["D17:C0"]),
                add(ItemKind::Decision, "Use SAML", &["T2"]),
            ],
        );
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        let req = s.item("REQ-1").unwrap();
        assert_eq!(req.text, "Production runs in Azure");
        assert_eq!(req.source_refs, ["Mm:T1", "S17:C0"]);
        assert_eq!((req.created_at_ms, req.revision), (NOW, 1));
        assert!(s.item("REQ-2").is_some() && s.item("DEC-1").is_some());
        assert_eq!(
            r.applied.iter().map(|a| a.seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn fabricated_or_missing_evidence_rejects_the_op() {
        let mut s = MeetingState::new("m");
        let r = run(
            &mut s,
            vec![
                add(ItemKind::Fact, "a", &["T1", "T99"]),
                add(ItemKind::Fact, "b", &[]),
                add(ItemKind::Fact, "c", &["S17:C0"]),
                add(ItemKind::Fact, "d", &["M1:T4"]),
            ],
        );
        assert!(s.items.is_empty());
        assert_eq!(
            reasons(&r),
            [
                (0, RejectReason::UnknownEvidence("T99".into())),
                (1, RejectReason::NoEvidence),
                (2, RejectReason::UnknownEvidence("S17:C0".into())),
                (3, RejectReason::UnknownEvidence("M1:T4".into())),
            ]
        );
    }

    #[test]
    fn malformed_adds_are_rejected() {
        let mut s = MeetingState::new("m");
        let mut no_kind = add(ItemKind::Fact, "x", &["T1"]);
        no_kind.kind = None;
        let mut no_status = add(ItemKind::Fact, "x", &["T1"]);
        no_status.epistemic_status = None;
        let mut nan = add(ItemKind::Fact, "x", &["T1"]);
        nan.confidence = Some(f64::NAN);
        let mut high = add(ItemKind::Fact, "x", &["T1"]);
        high.confidence = Some(1.5);
        let mut no_text = add(ItemKind::Fact, "x", &["T1"]);
        no_text.text = None;
        let long = "w".repeat(MAX_TEXT_CHARS + 1);
        let r = run(
            &mut s,
            vec![
                no_kind,
                no_status,
                nan,
                high,
                no_text,
                add(ItemKind::Fact, " \n ", &["T1"]),
                add(ItemKind::Fact, &long, &["T1"]),
            ],
        );
        assert!(s.items.is_empty());
        let got = reasons(&r);
        assert_eq!(got[0].1, RejectReason::Missing("kind"));
        assert_eq!(got[1].1, RejectReason::Missing("epistemic_status"));
        assert!(matches!(got[2].1, RejectReason::BadConfidence(c) if c.is_nan()));
        assert_eq!(got[3].1, RejectReason::BadConfidence(1.5));
        assert_eq!(got[4].1, RejectReason::Missing("text"));
        assert_eq!(got[5].1, RejectReason::EmptyText);
        assert_eq!(got[6].1, RejectReason::TextTooLong(MAX_TEXT_CHARS + 1));
    }

    #[test]
    fn only_some_kinds_can_be_suggested() {
        let mut s = MeetingState::new("m");
        let mut decision = add(ItemKind::Decision, "Go with Azure", &["T1"]);
        decision.epistemic_status = Some(EpistemicStatus::Suggested);
        let mut question = add(ItemKind::OpenQuestion, "Ask about peak load", &["T1"]);
        question.epistemic_status = Some(EpistemicStatus::Suggested);
        let r = run(&mut s, vec![decision, question]);
        assert_eq!(
            reasons(&r),
            [(0, RejectReason::SuggestedNotAllowed(ItemKind::Decision))]
        );
        assert_eq!(s.item("Q-1").unwrap().status, EpistemicStatus::Suggested);
    }

    #[test]
    fn repeated_adds_merge_into_the_live_item() {
        let mut s = MeetingState::new("m");
        run(
            &mut s,
            vec![add(ItemKind::Requirement, "Runs in Azure", &["T1"])],
        );
        let mut stronger = add(ItemKind::Requirement, "runs in azure.", &["T1"]);
        stronger.confidence = Some(0.95);
        let r = run(
            &mut s,
            vec![
                add(ItemKind::Requirement, "Runs in  AZURE!", &["T3", "T1"]),
                add(ItemKind::Requirement, "Runs in Azure", &["T1"]),
                stronger,
                add(ItemKind::Constraint, "Runs in Azure", &["T1"]),
            ],
        );
        assert_eq!(r.merged, 2);
        assert_eq!(reasons(&r), [(1, RejectReason::Duplicate("REQ-1".into()))]);
        let req = s.item("REQ-1").unwrap();
        assert_eq!(req.source_refs, ["Mm:T1", "Mm:T3"]);
        assert_eq!(req.confidence, 0.95);
        assert_eq!(
            req.text, "Runs in Azure",
            "merging keeps the original wording"
        );
        assert!(s.item("CON-1").is_some(), "another kind is another item");
        assert!(s.item("REQ-2").is_none());
    }

    #[test]
    fn a_replaced_commitment_is_superseded_not_left_open() {
        let mut s = MeetingState::new("m");
        let mut friday = add(
            ItemKind::Commitment,
            "Customer sends traffic numbers",
            &["T1"],
        );
        friday.owner = Some("Customer".into());
        friday.due = Some(" Friday ".into());
        run(&mut s, vec![friday]);

        let mut open = add(
            ItemKind::Commitment,
            "Customer sends traffic numbers, date open",
            &["T5"],
        );
        open.temp_id = Some("new".into());
        open.owner = Some("Customer".into());
        let r = run(
            &mut s,
            vec![
                open,
                lifecycle("COM-1", Lifecycle::Superseded, Some("new"), &["T5"]),
            ],
        );
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        let old = s.item("COM-1").unwrap();
        let new = s.item("COM-2").unwrap();
        assert_eq!(old.due.as_deref(), Some("Friday"));
        assert_eq!(old.lifecycle, Lifecycle::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some("COM-2"));
        assert_eq!(old.source_refs, ["Mm:T1", "Mm:T5"]);
        assert_eq!(new.supersedes.as_deref(), Some("COM-1"));
        assert_eq!(new.due, None);
        let live: Vec<&str> = s.live_items().iter().map(|i| i.id.as_str()).collect();
        assert_eq!(live, ["COM-2"]);

        // A superseded item no longer changes.
        let mut touch = op(OpKind::Update);
        touch.id = Some("COM-1".into());
        touch.due = Some("Monday".into());
        touch.source_refs = vec!["T6".into()];
        let r = run(
            &mut s,
            vec![touch, lifecycle("COM-1", Lifecycle::Active, None, &["T6"])],
        );
        assert_eq!(
            reasons(&r),
            [
                (0, RejectReason::TerminalItem("COM-1".into())),
                (1, RejectReason::TerminalItem("COM-1".into())),
            ]
        );
    }

    #[test]
    fn superseding_needs_a_live_replacement_of_the_same_kind() {
        let mut s = MeetingState::new("m");
        run(
            &mut s,
            vec![
                add(ItemKind::Decision, "Use SAML", &["T1"]),
                add(ItemKind::Decision, "Use OIDC", &["T2"]),
                add(ItemKind::Fact, "They use Okta", &["T2"]),
            ],
        );
        let r = run(
            &mut s,
            vec![
                lifecycle("DEC-1", Lifecycle::Superseded, None, &["T2"]),
                lifecycle("DEC-1", Lifecycle::Superseded, Some("DEC-1"), &["T2"]),
                lifecycle("DEC-1", Lifecycle::Superseded, Some("FACT-1"), &["T2"]),
                lifecycle("DEC-1", Lifecycle::Superseded, Some("DEC-9"), &["T2"]),
                lifecycle("DEC-1", Lifecycle::Active, None, &["T2"]),
                lifecycle("DEC-1", Lifecycle::Resolved, None, &[]),
                lifecycle("DEC-7", Lifecycle::Resolved, None, &["T2"]),
                lifecycle("DEC-1", Lifecycle::Superseded, Some("DEC-2"), &["T2"]),
            ],
        );
        assert_eq!(
            reasons(&r),
            [
                (0, RejectReason::SupersedeNeedsReplacement),
                (1, RejectReason::BadReplacement("DEC-1".into())),
                (2, RejectReason::BadReplacement("FACT-1".into())),
                (3, RejectReason::UnknownItem("DEC-9".into())),
                (4, RejectReason::NoChange("DEC-1".into())),
                (5, RejectReason::NoEvidence),
                (6, RejectReason::UnknownItem("DEC-7".into())),
            ]
        );
        assert_eq!(
            s.item("DEC-1").unwrap().superseded_by.as_deref(),
            Some("DEC-2")
        );
    }

    #[test]
    fn updates_need_evidence_and_an_actual_change() {
        let mut s = MeetingState::new("m");
        run(
            &mut s,
            vec![add(ItemKind::Commitment, "Send the diagram", &["T1"])],
        );
        let upd = |refs: &[&str], owner: Option<&str>, conf: Option<f64>| ModelOp {
            id: Some("COM-1".into()),
            owner: owner.map(str::to_owned),
            confidence: conf,
            source_refs: refs.iter().map(|r| r.to_string()).collect(),
            ..op(OpKind::Update)
        };
        let r = run(
            &mut s,
            vec![
                upd(&[], Some("You"), None),
                upd(&["T1"], None, Some(0.9)),
                upd(&["T2"], Some("You"), None),
                upd(&["T2"], Some("You"), None),
                ModelOp {
                    id: None,
                    ..upd(&["T2"], Some("Sarah"), None)
                },
            ],
        );
        assert_eq!(
            reasons(&r),
            [
                (0, RejectReason::NoEvidence),
                (1, RejectReason::NoChange("COM-1".into())),
                (3, RejectReason::NoChange("COM-1".into())),
                (4, RejectReason::Missing("id")),
            ]
        );
        let c = s.item("COM-1").unwrap();
        assert_eq!(c.owner.as_deref(), Some("You"));
        assert_eq!(c.source_refs, ["Mm:T1", "Mm:T2"]);
        assert_eq!(c.revision, 2);
    }

    #[test]
    fn temp_ids_and_related_items_must_resolve() {
        let mut s = MeetingState::new("m");
        run(
            &mut s,
            vec![add(ItemKind::Requirement, "Runs in Azure", &["T1"])],
        );
        let mut bad = add(ItemKind::Requirement, "", &["T2"]);
        bad.temp_id = Some("x".into());
        let mut conflict = add(ItemKind::Conflict, "Azure vs on-prem", &["T3"]);
        conflict.related_items = vec!["REQ-1".into(), "x".into()];
        let mut dup_a = add(ItemKind::Fact, "a", &["T1"]);
        dup_a.temp_id = Some("t".into());
        let mut dup_b = add(ItemKind::Fact, "b", &["T1"]);
        dup_b.temp_id = Some("t".into());
        let mut clash = add(ItemKind::Fact, "c", &["T1"]);
        clash.temp_id = Some("REQ-1".into());
        let mut ok_conflict = add(ItemKind::Conflict, "Azure vs EU-only", &["T3", "D17:C0"]);
        ok_conflict.related_items = vec!["REQ-1".into(), "t".into(), "REQ-1".into()];
        let r = run(
            &mut s,
            vec![bad, conflict, dup_a, dup_b, clash, ok_conflict],
        );
        assert_eq!(
            reasons(&r),
            [
                (0, RejectReason::EmptyText),
                (1, RejectReason::UnknownItem("x".into())),
                (3, RejectReason::DuplicateTempId("t".into())),
                (4, RejectReason::DuplicateTempId("REQ-1".into())),
            ]
        );
        assert_eq!(s.item("CONF-1").unwrap().related_items, ["REQ-1", "FACT-1"]);
    }

    #[test]
    fn a_batch_is_capped() {
        let mut s = MeetingState::new("m");
        let ops: Vec<ModelOp> = (0..MAX_OPS_PER_BATCH + 2)
            .map(|i| add(ItemKind::Fact, &format!("fact {i}"), &["T1"]))
            .collect();
        let r = run(&mut s, ops);
        assert_eq!(r.applied.len(), MAX_OPS_PER_BATCH);
        assert_eq!(r.rejected.len(), 2);
        assert!(r
            .rejected
            .iter()
            .all(|x| x.reason == RejectReason::TooManyOps));
    }

    #[test]
    fn the_applied_log_replays_to_the_same_state() {
        let mut s = MeetingState::new("m");
        let mut log = Vec::new();
        let mut first = add(ItemKind::Commitment, "Send numbers Friday", &["T1"]);
        first.due = Some("Friday".into());
        log.extend(run(&mut s, vec![first]).applied);
        let mut second = add(ItemKind::Commitment, "Send numbers, date open", &["T4"]);
        second.temp_id = Some("n".into());
        log.extend(
            run(
                &mut s,
                vec![
                    second,
                    lifecycle("COM-1", Lifecycle::Superseded, Some("n"), &["T4"]),
                    add(ItemKind::Commitment, "send numbers date open", &["T5"]),
                ],
            )
            .applied,
        );
        assert_eq!(MeetingState::replay("m", &log).unwrap(), s);
        // After replay, new ids continue past the replayed ones.
        let mut replayed = MeetingState::replay("m", &log).unwrap();
        run(
            &mut replayed,
            vec![add(ItemKind::Commitment, "Book the review", &["T6"])],
        );
        assert!(replayed.item("COM-3").is_some());
    }

    #[test]
    fn an_inconsistent_state_rejects_instead_of_panicking() {
        let mut s = MeetingState::new("m");
        run(&mut s, vec![add(ItemKind::Fact, "a", &["T1"])]);
        // A hand-edited state: an item inserted without going through the log takes the next id.
        let mut forged = s.item("FACT-1").unwrap().clone();
        forged.id = "FACT-2".into();
        s.items.insert(forged.id.clone(), forged);
        let r = run(&mut s, vec![add(ItemKind::Fact, "b", &["T1"])]);
        assert!(r.applied.is_empty());
        assert!(matches!(
            r.rejected[0].reason,
            RejectReason::Inconsistent(_)
        ));
    }

    #[test]
    fn reject_reasons_read_plainly() {
        assert_eq!(
            RejectReason::UnknownEvidence("T99".into()).to_string(),
            "evidence T99 was not provided"
        );
        assert_eq!(
            RejectReason::SuggestedNotAllowed(ItemKind::Decision).to_string(),
            "a decision can't be merely suggested"
        );
    }
}

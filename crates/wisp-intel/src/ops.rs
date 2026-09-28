//! State operations. A model proposes [`ModelOp`]s that cite evidence by short ID; the reducer turns
//! each valid one into a [`ResolvedOp`] with canonical refs and assigned ids. Resolved ops are what
//! gets recorded: replaying them in order rebuilds the state exactly, without the model or the
//! evidence packet.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{EpistemicStatus, ItemKind, Lifecycle, MeetingState, SourceRef, StateItem};

/// What a model may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Add,
    Update,
    SetLifecycle,
}

/// One proposed change, as a model returns it. Every field is present (null when unused) so the
/// schema works with strict structured output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelOp {
    pub op: OpKind,
    /// The item to change (`update`, `set_lifecycle`): an existing id or an earlier `temp_id`.
    pub id: Option<String>,
    /// A name for a new item (`add`) that later ops in the same batch can refer to.
    pub temp_id: Option<String>,
    pub kind: Option<ItemKind>,
    pub text: Option<String>,
    pub epistemic_status: Option<EpistemicStatus>,
    pub confidence: Option<f64>,
    pub lifecycle: Option<Lifecycle>,
    /// For `set_lifecycle` to `superseded`: the replacing item (id or temp_id).
    pub superseded_by: Option<String>,
    pub owner: Option<String>,
    pub due: Option<String>,
    /// Evidence IDs from the context (`T12`, `D17:C4`, `M1:T221`).
    pub source_refs: Vec<String>,
    /// Item ids (or temp_ids) this item is about.
    pub related_items: Vec<String>,
}

/// A batch of proposed changes: the whole model output of an observer pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpBatch {
    pub ops: Vec<ModelOp>,
}

/// A validated change with canonical refs and real ids. This is what the log stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ResolvedOp {
    Add {
        id: String,
        kind: ItemKind,
        text: String,
        status: EpistemicStatus,
        confidence: f64,
        source_refs: Vec<SourceRef>,
        related_items: Vec<String>,
        owner: Option<String>,
        due: Option<String>,
    },
    /// Changes fields that are `Some` and appends new refs. Also what a duplicate `add` becomes.
    Update {
        id: String,
        text: Option<String>,
        confidence: Option<f64>,
        owner: Option<String>,
        due: Option<String>,
        add_refs: Vec<SourceRef>,
        add_related: Vec<String>,
    },
    SetLifecycle {
        id: String,
        lifecycle: Lifecycle,
        superseded_by: Option<String>,
        add_refs: Vec<SourceRef>,
    },
}

impl ResolvedOp {
    /// The item the op creates or changes.
    pub fn item_id(&self) -> &str {
        match self {
            ResolvedOp::Add { id, .. }
            | ResolvedOp::Update { id, .. }
            | ResolvedOp::SetLifecycle { id, .. } => id,
        }
    }
}

/// A resolved op with its place in the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedOp {
    pub seq: u64,
    pub at_ms: i64,
    pub op: ResolvedOp,
}

/// Why a log can't be replayed onto a state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("op {seq} is out of order (expected {expected})")]
    OutOfOrder { seq: u64, expected: u64 },
    #[error("op {seq} refers to unknown item {id}")]
    UnknownItem { seq: u64, id: String },
    #[error("op {seq} adds {id}, which already exists")]
    Duplicate { seq: u64, id: String },
}

impl MeetingState {
    /// Applies a logged op. The reducer only produces ops that apply; this checks anyway so a
    /// damaged log fails loudly instead of rebuilding a different state.
    pub fn apply(&mut self, applied: &AppliedOp) -> Result<(), ReplayError> {
        let seq = applied.seq;
        if seq != self.op_count {
            return Err(ReplayError::OutOfOrder {
                seq,
                expected: self.op_count,
            });
        }
        let at = applied.at_ms;
        match &applied.op {
            ResolvedOp::Add {
                id,
                kind,
                text,
                status,
                confidence,
                source_refs,
                related_items,
                owner,
                due,
            } => {
                if self.items.contains_key(id) {
                    return Err(ReplayError::Duplicate {
                        seq,
                        id: id.clone(),
                    });
                }
                self.note_id(*kind, id);
                self.items.insert(
                    id.clone(),
                    StateItem {
                        id: id.clone(),
                        kind: *kind,
                        text: text.clone(),
                        status: *status,
                        confidence: *confidence,
                        lifecycle: Lifecycle::Active,
                        source_refs: source_refs.clone(),
                        related_items: related_items.clone(),
                        owner: owner.clone(),
                        due: due.clone(),
                        supersedes: None,
                        superseded_by: None,
                        created_at_ms: at,
                        updated_at_ms: at,
                        revision: 1,
                    },
                );
            }
            ResolvedOp::Update {
                id,
                text,
                confidence,
                owner,
                due,
                add_refs,
                add_related,
            } => {
                let item = self.item_mut(seq, id)?;
                if let Some(t) = text {
                    item.text = t.clone();
                }
                if let Some(c) = confidence {
                    item.confidence = *c;
                }
                if owner.is_some() {
                    item.owner = owner.clone();
                }
                if due.is_some() {
                    item.due = due.clone();
                }
                push_new(&mut item.source_refs, add_refs);
                push_new(&mut item.related_items, add_related);
                touch(item, at);
            }
            ResolvedOp::SetLifecycle {
                id,
                lifecycle,
                superseded_by,
                add_refs,
            } => {
                if let Some(by) = superseded_by {
                    self.item_mut(seq, by)?.supersedes = Some(id.clone());
                }
                let item = self.item_mut(seq, id)?;
                item.lifecycle = *lifecycle;
                if superseded_by.is_some() {
                    item.superseded_by = superseded_by.clone();
                }
                push_new(&mut item.source_refs, add_refs);
                touch(item, at);
            }
        }
        self.op_count += 1;
        Ok(())
    }

    /// Rebuilds a meeting's state from its log.
    pub fn replay<'a>(
        meeting_id: &str,
        log: impl IntoIterator<Item = &'a AppliedOp>,
    ) -> Result<Self, ReplayError> {
        let mut state = MeetingState::new(meeting_id);
        for op in log {
            state.apply(op)?;
        }
        Ok(state)
    }

    fn item_mut(&mut self, seq: u64, id: &str) -> Result<&mut StateItem, ReplayError> {
        self.items
            .get_mut(id)
            .ok_or_else(|| ReplayError::UnknownItem {
                seq,
                id: id.to_owned(),
            })
    }
}

fn push_new(list: &mut Vec<String>, add: &[String]) {
    for x in add {
        if !list.contains(x) {
            list.push(x.clone());
        }
    }
}

fn touch(item: &mut StateItem, at: i64) {
    item.updated_at_ms = at;
    item.revision += 1;
}

/// The JSON Schema for an [`OpBatch`]. Every property is required and objects are closed, so it
/// works as strict structured output for Codex and as a checked contract for Claude. It sticks to
/// keywords strict mode accepts; ranges (confidence in 0..1) are the reducer's job.
pub fn output_schema() -> Value {
    let nullable_string = json!({"type": ["string", "null"]});
    let kinds: Vec<Value> = ItemKind::ALL
        .iter()
        .map(|k| Value::from(k.as_str()))
        .chain([Value::Null])
        .collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["ops"],
        "properties": {
            "ops": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "op", "id", "temp_id", "kind", "text", "epistemic_status", "confidence",
                        "lifecycle", "superseded_by", "owner", "due", "source_refs", "related_items"
                    ],
                    "properties": {
                        "op": {"type": "string", "enum": ["add", "update", "set_lifecycle"]},
                        "id": nullable_string,
                        "temp_id": nullable_string,
                        "kind": {"type": ["string", "null"], "enum": kinds},
                        "text": nullable_string,
                        "epistemic_status": {
                            "type": ["string", "null"],
                            "enum": ["stated", "inferred", "suggested", null]
                        },
                        "confidence": {"type": ["number", "null"]},
                        "lifecycle": {
                            "type": ["string", "null"],
                            "enum": ["active", "resolved", "superseded", "withdrawn", "uncertain", null]
                        },
                        "superseded_by": nullable_string,
                        "owner": nullable_string,
                        "due": nullable_string,
                        "source_refs": {"type": "array", "items": {"type": "string"}},
                        "related_items": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(id: &str, kind: ItemKind, text: &str) -> ResolvedOp {
        ResolvedOp::Add {
            id: id.into(),
            kind,
            text: text.into(),
            status: EpistemicStatus::Stated,
            confidence: 0.9,
            source_refs: vec!["Mm:T1".into()],
            related_items: vec![],
            owner: None,
            due: None,
        }
    }

    fn log(ops: Vec<ResolvedOp>) -> Vec<AppliedOp> {
        ops.into_iter()
            .enumerate()
            .map(|(i, op)| AppliedOp {
                seq: i as u64,
                at_ms: 100 + i as i64,
                op,
            })
            .collect()
    }

    #[test]
    fn replay_rebuilds_state_and_links_supersession() {
        let ops = log(vec![
            add("COM-1", ItemKind::Commitment, "Send traffic numbers Friday"),
            add(
                "COM-2",
                ItemKind::Commitment,
                "Send traffic numbers, date open",
            ),
            ResolvedOp::SetLifecycle {
                id: "COM-1".into(),
                lifecycle: Lifecycle::Superseded,
                superseded_by: Some("COM-2".into()),
                add_refs: vec!["Mm:T9".into()],
            },
            ResolvedOp::Update {
                id: "COM-2".into(),
                text: None,
                confidence: Some(0.7),
                owner: Some("Customer".into()),
                due: None,
                add_refs: vec!["Mm:T9".into(), "Mm:T1".into()],
                add_related: vec![],
            },
        ]);
        let state = MeetingState::replay("m", &ops).unwrap();
        let old = state.item("COM-1").unwrap();
        let new = state.item("COM-2").unwrap();
        assert_eq!(old.lifecycle, Lifecycle::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some("COM-2"));
        assert_eq!(old.source_refs, ["Mm:T1", "Mm:T9"]);
        assert_eq!(new.supersedes.as_deref(), Some("COM-1"));
        assert_eq!(new.owner.as_deref(), Some("Customer"));
        assert_eq!(new.confidence, 0.7);
        assert_eq!(new.source_refs, ["Mm:T1", "Mm:T9"], "no duplicate refs");
        assert_eq!((new.revision, new.updated_at_ms), (2, 103));
        assert_eq!(state.op_count, 4);
        assert_eq!(
            state
                .live_items()
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["COM-2"]
        );

        // Serialized and back, it replays to the same state.
        let json = serde_json::to_string(&ops).unwrap();
        let back: Vec<AppliedOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(MeetingState::replay("m", &back).unwrap(), state);
    }

    #[test]
    fn a_damaged_log_fails_instead_of_diverging() {
        let mut ops = log(vec![add("REQ-1", ItemKind::Requirement, "Azure")]);
        ops[0].seq = 1;
        assert_eq!(
            MeetingState::replay("m", &ops),
            Err(ReplayError::OutOfOrder {
                seq: 1,
                expected: 0
            })
        );
        let ops = log(vec![ResolvedOp::SetLifecycle {
            id: "REQ-9".into(),
            lifecycle: Lifecycle::Resolved,
            superseded_by: None,
            add_refs: vec![],
        }]);
        assert!(matches!(
            MeetingState::replay("m", &ops),
            Err(ReplayError::UnknownItem { .. })
        ));
        let ops = log(vec![
            add("REQ-1", ItemKind::Requirement, "a"),
            add("REQ-1", ItemKind::Requirement, "b"),
        ]);
        assert!(matches!(
            MeetingState::replay("m", &ops),
            Err(ReplayError::Duplicate { .. })
        ));
    }

    #[test]
    fn the_schema_is_strict_and_accepts_a_full_op() {
        let schema = output_schema();
        let op = json!({"ops": [{
            "op": "add", "id": null, "temp_id": "a", "kind": "requirement", "text": "Azure",
            "epistemic_status": "stated", "confidence": 0.9, "lifecycle": null,
            "superseded_by": null, "owner": null, "due": null,
            "source_refs": ["T1"], "related_items": []
        }]});
        wisp_reasoning::validate(&schema, &op).unwrap();
        let batch: OpBatch = serde_json::from_value(op).unwrap();
        assert_eq!(batch.ops[0].kind, Some(ItemKind::Requirement));

        let missing = json!({"ops": [{"op": "add"}]});
        assert!(wisp_reasoning::validate(&schema, &missing).is_err());
        let extra = json!({"ops": [], "note": "hi"});
        assert!(wisp_reasoning::validate(&schema, &extra).is_err());
        // Every object property is listed as required (strict mode needs that).
        let item = &schema["properties"]["ops"]["items"];
        let props = item["properties"].as_object().unwrap().len();
        assert_eq!(item["required"].as_array().unwrap().len(), props);
    }
}

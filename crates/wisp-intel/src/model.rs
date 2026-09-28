//! The structured meeting state: independently addressable items, each with an epistemic status,
//! a confidence, a lifecycle, and the evidence it rests on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Participant,
    Requirement,
    Constraint,
    Fact,
    Decision,
    Assumption,
    Commitment,
    OpenQuestion,
    Risk,
    Topic,
    Artifact,
    Conflict,
    TaskCandidate,
    Objective,
}

impl ItemKind {
    pub const ALL: [ItemKind; 14] = [
        ItemKind::Participant,
        ItemKind::Requirement,
        ItemKind::Constraint,
        ItemKind::Fact,
        ItemKind::Decision,
        ItemKind::Assumption,
        ItemKind::Commitment,
        ItemKind::OpenQuestion,
        ItemKind::Risk,
        ItemKind::Topic,
        ItemKind::Artifact,
        ItemKind::Conflict,
        ItemKind::TaskCandidate,
        ItemKind::Objective,
    ];

    /// The id prefix for items of this kind (`REQ` in `REQ-17`).
    pub fn prefix(self) -> &'static str {
        match self {
            ItemKind::Participant => "PER",
            ItemKind::Requirement => "REQ",
            ItemKind::Constraint => "CON",
            ItemKind::Fact => "FACT",
            ItemKind::Decision => "DEC",
            ItemKind::Assumption => "ASM",
            ItemKind::Commitment => "COM",
            ItemKind::OpenQuestion => "Q",
            ItemKind::Risk => "RISK",
            ItemKind::Topic => "TOP",
            ItemKind::Artifact => "ART",
            ItemKind::Conflict => "CONF",
            ItemKind::TaskCandidate => "TASK",
            ItemKind::Objective => "OBJ",
        }
    }

    /// The name used in the output schema and in prompts.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Participant => "participant",
            ItemKind::Requirement => "requirement",
            ItemKind::Constraint => "constraint",
            ItemKind::Fact => "fact",
            ItemKind::Decision => "decision",
            ItemKind::Assumption => "assumption",
            ItemKind::Commitment => "commitment",
            ItemKind::OpenQuestion => "open_question",
            ItemKind::Risk => "risk",
            ItemKind::Topic => "topic",
            ItemKind::Artifact => "artifact",
            ItemKind::Conflict => "conflict",
            ItemKind::TaskCandidate => "task_candidate",
            ItemKind::Objective => "objective",
        }
    }

    /// Whether the system may hold an item of this kind as a mere suggestion. A decision, a
    /// commitment or a requirement is either said (stated) or deduced (inferred), never suggested.
    pub fn allows_suggested(self) -> bool {
        matches!(
            self,
            ItemKind::OpenQuestion | ItemKind::Risk | ItemKind::TaskCandidate | ItemKind::Objective
        )
    }
}

/// How the system knows an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpistemicStatus {
    /// Someone said it, or a source document contains it.
    Stated,
    /// The system believes it follows from the evidence.
    Inferred,
    /// The system recommends it: a question, an interpretation, a next step.
    Suggested,
}

impl EpistemicStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EpistemicStatus::Stated => "stated",
            EpistemicStatus::Inferred => "inferred",
            EpistemicStatus::Suggested => "suggested",
        }
    }
}

/// Where an item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Active,
    Resolved,
    /// Replaced by another item (see [`StateItem::superseded_by`]). Terminal.
    Superseded,
    /// Taken back by whoever said it. Terminal.
    Withdrawn,
    Uncertain,
}

impl Lifecycle {
    /// Superseded and withdrawn items stay for the record but no longer change.
    pub fn is_terminal(self) -> bool {
        matches!(self, Lifecycle::Superseded | Lifecycle::Withdrawn)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Active => "active",
            Lifecycle::Resolved => "resolved",
            Lifecycle::Superseded => "superseded",
            Lifecycle::Withdrawn => "withdrawn",
            Lifecycle::Uncertain => "uncertain",
        }
    }
}

/// A canonical evidence address, as the library writes it: `M<meeting>:T<segment>` for a
/// transcript line, `S<source>:C<chunk>` for a chunk of a project source. It stays meaningful after
/// the text behind it is deleted by retention; it just stops resolving.
pub type SourceRef = String;

/// One addressable item of meeting state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateItem {
    /// `REQ-17` and the like; assigned by the reducer, never by a model.
    pub id: String,
    pub kind: ItemKind,
    pub text: String,
    pub status: EpistemicStatus,
    pub confidence: f64,
    pub lifecycle: Lifecycle,
    /// The evidence the item rests on, in the order it was cited.
    pub source_refs: Vec<SourceRef>,
    /// Other items this one is about (the two sides of a conflict, say).
    pub related_items: Vec<String>,
    /// Who owns a commitment or task, as said in the meeting.
    pub owner: Option<String>,
    /// When it is due, as said in the meeting ("Friday"); not parsed.
    pub due: Option<String>,
    /// The item this one replaced.
    pub supersedes: Option<String>,
    /// The item that replaced this one.
    pub superseded_by: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Bumped on every change.
    pub revision: u32,
}

/// The state of one meeting: its items, and how far through the transcript analysis has read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeetingState {
    pub meeting_id: String,
    /// Every item ever created, by id. Terminal items stay.
    pub items: BTreeMap<String, StateItem>,
    /// The highest transcript segment index a completed analysis pass has seen.
    pub analyzed_through: Option<i64>,
    /// How many ops have been applied; the next op's sequence number.
    pub op_count: u64,
    /// The last number used for each id prefix.
    #[serde(default)]
    next_numbers: BTreeMap<String, u32>,
}

impl MeetingState {
    pub fn new(meeting_id: impl Into<String>) -> Self {
        Self {
            meeting_id: meeting_id.into(),
            ..Self::default()
        }
    }

    pub fn item(&self, id: &str) -> Option<&StateItem> {
        self.items.get(id)
    }

    /// Items not superseded or withdrawn, by kind then id number.
    pub fn live_items(&self) -> Vec<&StateItem> {
        let mut items: Vec<&StateItem> = self
            .items
            .values()
            .filter(|i| !i.lifecycle.is_terminal())
            .collect();
        items.sort_by_key(|i| (i.kind, id_number(&i.id)));
        items
    }

    /// The id the next item of `kind` gets.
    pub(crate) fn next_id(&self, kind: ItemKind) -> String {
        let last = self.next_numbers.get(kind.prefix()).copied().unwrap_or(0);
        format!("{}-{}", kind.prefix(), last + 1)
    }

    /// Records that `id` is taken, so later ids of its kind number past it.
    pub(crate) fn note_id(&mut self, kind: ItemKind, id: &str) {
        let n = self
            .next_numbers
            .entry(kind.prefix().to_owned())
            .or_insert(0);
        *n = (*n).max(id_number(id));
    }
}

/// The number part of an item id, for ordering `REQ-2` before `REQ-10`.
fn id_number(id: &str) -> u32 {
    id.rsplit_once('-')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

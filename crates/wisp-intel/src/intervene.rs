//! Candidate interventions and the local filter that decides which reach the user.
//!
//! A reasoning pass may propose [`RawCandidate`]s: a question to ask or a warning worth raising
//! while the people are still present. The model scores them, but it does not decide what is
//! shown. [`validate_candidate`] checks a candidate's evidence the way the reducer checks ops, and
//! [`InterventionFilter::consider`] applies a deliberately conservative local policy: a confidence
//! floor, a score threshold (value × confidence × urgency − interruption cost), a cooldown between
//! cards, an hourly cap, and duplicate suppression against what was already shown or dismissed.
//! Every decision is logged, with the user's dismissals, so the policy can be tuned from real
//! meetings. The filter is clock-free: callers pass the time.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::evidence::EvidencePacket;
use crate::model::{MeetingState, SourceRef};
use crate::reducer::RejectReason;

/// Most candidates taken from one pass.
pub const MAX_CANDIDATES_PER_PASS: usize = 5;
const MAX_TITLE_CHARS: usize = 160;
const MAX_DETAIL_CHARS: usize = 600;

/// What kind of intervention a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// Something the discussion relies on that nobody confirmed.
    UnconfirmedAssumption,
    /// Two things said (or a statement and a document) that can't both hold.
    Conflict,
    /// A commitment with no owner or no date.
    MissingOwner,
    /// Something needed to proceed that hasn't been asked.
    MissingRequirement,
    /// A decision talked about but never actually made.
    UnresolvedDecision,
    /// Scope that is ambiguous or changed without agreement.
    ScopeAmbiguity,
    /// A follow-up that will be forgotten unless raised now.
    FollowUp,
}

impl CandidateKind {
    const ALL: [CandidateKind; 7] = [
        CandidateKind::UnconfirmedAssumption,
        CandidateKind::Conflict,
        CandidateKind::MissingOwner,
        CandidateKind::MissingRequirement,
        CandidateKind::UnresolvedDecision,
        CandidateKind::ScopeAmbiguity,
        CandidateKind::FollowUp,
    ];

    fn as_str(self) -> &'static str {
        match self {
            CandidateKind::UnconfirmedAssumption => "unconfirmed_assumption",
            CandidateKind::Conflict => "conflict",
            CandidateKind::MissingOwner => "missing_owner",
            CandidateKind::MissingRequirement => "missing_requirement",
            CandidateKind::UnresolvedDecision => "unresolved_decision",
            CandidateKind::ScopeAmbiguity => "scope_ambiguity",
            CandidateKind::FollowUp => "follow_up",
        }
    }
}

/// A candidate as a model returns it (every field present, for strict structured output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawCandidate {
    pub kind: CandidateKind,
    /// At most six words, read at a glance. Derived from `title` when missing or too long.
    #[serde(default)]
    pub headline: String,
    /// The issue in one sentence.
    pub title: String,
    /// Why it matters, briefly.
    pub detail: String,
    /// What You could ask, if there is a question to ask.
    pub suggested_question: Option<String>,
    pub source_refs: Vec<String>,
    pub related_items: Vec<String>,
    /// How much later work or rework raising it now would save, 0..1.
    pub importance: f64,
    /// How much it matters to raise it now rather than later, 0..1.
    pub urgency: f64,
    /// How sure the model is that the issue is real, 0..1.
    pub confidence: f64,
    /// How likely leaving it causes rework or another meeting, 0..1.
    pub future_work_risk: f64,
}

/// A candidate whose evidence checked out, with canonical refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub kind: CandidateKind,
    /// At most six words, shown first; see [`crate::headline`].
    #[serde(default)]
    pub headline: String,
    pub title: String,
    pub detail: String,
    pub suggested_question: Option<String>,
    pub source_refs: Vec<SourceRef>,
    /// The evidence IDs as cited in this pass, for display next to the card.
    pub cited: Vec<String>,
    pub related_items: Vec<String>,
    pub importance: f64,
    pub urgency: f64,
    pub confidence: f64,
    pub future_work_risk: f64,
}

/// Checks a candidate: text present and bounded, scores in range, at least one piece of evidence
/// and all of it from the packet, related items known.
pub fn validate_candidate(
    raw: &RawCandidate,
    packet: &EvidencePacket,
    state: &MeetingState,
) -> Result<Candidate, RejectReason> {
    let title = clean(&raw.title, MAX_TITLE_CHARS)?;
    let detail = clean(&raw.detail, MAX_DETAIL_CHARS).unwrap_or_default();
    for score in [
        raw.importance,
        raw.urgency,
        raw.confidence,
        raw.future_work_risk,
    ] {
        if !(score.is_finite() && (0.0..=1.0).contains(&score)) {
            return Err(RejectReason::BadConfidence(score));
        }
    }
    if raw.source_refs.is_empty() {
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
    Ok(Candidate {
        kind: raw.kind,
        headline: crate::headline::headline(&raw.headline, &title),
        title,
        detail,
        suggested_question: raw
            .suggested_question
            .as_deref()
            .and_then(|q| clean(q, MAX_DETAIL_CHARS).ok()),
        source_refs,
        cited,
        related_items,
        importance: raw.importance,
        urgency: raw.urgency,
        confidence: raw.confidence,
        future_work_risk: raw.future_work_risk,
    })
}

fn clean(text: &str, max: usize) -> Result<String, RejectReason> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = t.chars().count();
    if n == 0 {
        return Err(RejectReason::EmptyText);
    }
    if n > max {
        return Err(RejectReason::TextTooLong(n));
    }
    Ok(t)
}

/// The JSON Schema for a pass's candidate list (strict: every property required, closed objects).
pub fn candidates_schema() -> Value {
    let kinds: Vec<Value> = CandidateKind::ALL
        .iter()
        .map(|k| Value::from(k.as_str()))
        .collect();
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": [
                "kind", "headline", "title", "detail", "suggested_question", "source_refs", "related_items",
                "importance", "urgency", "confidence", "future_work_risk"
            ],
            "properties": {
                "kind": {"type": "string", "enum": kinds},
                "headline": {"type": "string"},
                "title": {"type": "string"},
                "detail": {"type": "string"},
                "suggested_question": {"type": ["string", "null"]},
                "source_refs": {"type": "array", "items": {"type": "string"}},
                "related_items": {"type": "array", "items": {"type": "string"}},
                "importance": {"type": "number"},
                "urgency": {"type": "number"},
                "confidence": {"type": "number"},
                "future_work_risk": {"type": "number"}
            }
        }
    })
}

/// The knobs of the local policy. The defaults are deliberately strict: missing a moderately useful
/// insight is better than showing five mediocre ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionPolicy {
    /// Candidates below this confidence are never shown.
    pub min_confidence: f64,
    /// The score a candidate must reach.
    pub threshold: f64,
    /// Subtracted from every score: the cost of interrupting.
    pub interruption_cost: f64,
    /// Least time between two shown cards.
    pub cooldown_ms: i64,
    /// Most cards shown in any hour.
    pub max_per_hour: usize,
    /// Word overlap (0..1) at which a candidate counts as a repeat.
    pub duplicate_overlap: f64,
}

impl Default for InterventionPolicy {
    fn default() -> Self {
        Self {
            min_confidence: 0.7,
            threshold: 0.35,
            interruption_cost: 0.1,
            cooldown_ms: 4 * 60 * 1000,
            max_per_hour: 5,
            duplicate_overlap: 0.6,
        }
    }
}

impl InterventionPolicy {
    /// value × confidence × urgency − interruption cost, where value is the mean of importance and
    /// future-work risk.
    pub fn score(&self, c: &Candidate) -> f64 {
        let value = (c.importance + c.future_work_risk) / 2.0;
        value * c.confidence * c.urgency - self.interruption_cost
    }
}

/// Why a candidate was not shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Suppressed {
    LowConfidence,
    BelowThreshold,
    Cooldown,
    HourlyCap,
    /// Repeats a card already shown or dismissed.
    Duplicate {
        of: String,
    },
    /// Its evidence didn't check out.
    Invalid {
        detail: String,
    },
}

/// What the filter decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Shown { card_id: String },
    Suppressed(Suppressed),
}

/// A shown card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub candidate: Candidate,
    pub score: f64,
    pub shown_at_ms: i64,
}

/// One line of the tuning log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum LogEntry {
    Considered {
        at_ms: i64,
        /// Absent when the candidate failed validation.
        candidate: Option<Box<Candidate>>,
        title: String,
        score: Option<f64>,
        decision: Decision,
    },
    Dismissed {
        at_ms: i64,
        card_id: String,
    },
}

/// The local filter for one meeting.
#[derive(Debug, Clone, Default)]
pub struct InterventionFilter {
    pub policy: InterventionPolicy,
    /// In endgame the cooldown is a quarter as long: gaps matter more now than interruptions.
    endgame: bool,
    cards: Vec<Card>,
    dismissed: BTreeSet<String>,
    log: Vec<LogEntry>,
}

impl InterventionFilter {
    pub fn new(policy: InterventionPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// Decides on a validated candidate, logs the decision, and returns the card if shown.
    pub fn consider(&mut self, candidate: Candidate, now_ms: i64) -> Option<Card> {
        let score = self.policy.score(&candidate);
        let decision = self.decide(&candidate, score, now_ms);
        let card = match &decision {
            Decision::Shown { card_id } => {
                let card = Card {
                    id: card_id.clone(),
                    candidate: candidate.clone(),
                    score,
                    shown_at_ms: now_ms,
                };
                self.cards.push(card.clone());
                Some(card)
            }
            Decision::Suppressed(_) => None,
        };
        self.log.push(LogEntry::Considered {
            at_ms: now_ms,
            title: candidate.title.clone(),
            candidate: Some(Box::new(candidate)),
            score: Some(score),
            decision,
        });
        card
    }

    /// Logs a candidate that failed validation.
    pub fn reject(&mut self, title: &str, reason: &RejectReason, now_ms: i64) {
        self.log.push(LogEntry::Considered {
            at_ms: now_ms,
            candidate: None,
            title: title.to_owned(),
            score: None,
            decision: Decision::Suppressed(Suppressed::Invalid {
                detail: reason.to_string(),
            }),
        });
    }

    /// Enters or leaves endgame.
    pub fn set_endgame(&mut self, on: bool) {
        self.endgame = on;
    }

    /// Records that the user dismissed a card. `false` if there is no such card.
    pub fn dismiss(&mut self, card_id: &str, now_ms: i64) -> bool {
        if !self.cards.iter().any(|c| c.id == card_id) || !self.dismissed.insert(card_id.to_owned())
        {
            return false;
        }
        self.log.push(LogEntry::Dismissed {
            at_ms: now_ms,
            card_id: card_id.to_owned(),
        });
        true
    }

    /// Cards shown and not dismissed, oldest first.
    pub fn active_cards(&self) -> Vec<&Card> {
        self.cards
            .iter()
            .filter(|c| !self.dismissed.contains(&c.id))
            .collect()
    }

    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    fn decide(&self, c: &Candidate, score: f64, now_ms: i64) -> Decision {
        let key = words(&c.title);
        if let Some(prev) = self.cards.iter().find(|card| {
            overlap(&key, &words(&card.candidate.title)) >= self.policy.duplicate_overlap
        }) {
            return Decision::Suppressed(Suppressed::Duplicate {
                of: prev.id.clone(),
            });
        }
        if c.confidence < self.policy.min_confidence {
            return Decision::Suppressed(Suppressed::LowConfidence);
        }
        if score < self.policy.threshold {
            return Decision::Suppressed(Suppressed::BelowThreshold);
        }
        if let Some(last) = self.cards.last() {
            let cooldown = if self.endgame {
                self.policy.cooldown_ms / 4
            } else {
                self.policy.cooldown_ms
            };
            if now_ms - last.shown_at_ms < cooldown {
                return Decision::Suppressed(Suppressed::Cooldown);
            }
        }
        let hour_ago = now_ms - 60 * 60 * 1000;
        if self
            .cards
            .iter()
            .filter(|card| card.shown_at_ms > hour_ago)
            .count()
            >= self.policy.max_per_hour
        {
            return Decision::Suppressed(Suppressed::HourlyCap);
        }
        Decision::Shown {
            card_id: format!("CARD-{}", self.cards.len() + 1),
        }
    }
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

/// Share of the smaller set's words found in the other (0 when either is empty).
fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let smaller = a.len().min(b.len());
    if smaller == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / smaller as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{render_transcript, TranscriptLine};

    const MIN: i64 = 60 * 1000;

    fn packet() -> EvidencePacket {
        let mut p = EvidencePacket::default();
        let lines: Vec<TranscriptLine> = (0..5)
            .map(|idx| TranscriptLine {
                idx,
                start_ms: idx * 1000,
                speaker: "Them".into(),
                text: format!("line {idx}"),
            })
            .collect();
        render_transcript(&mut p, "live", &[], &lines);
        p
    }

    fn raw(title: &str) -> RawCandidate {
        RawCandidate {
            kind: CandidateKind::UnconfirmedAssumption,
            headline: String::new(),
            title: title.into(),
            detail: "The estimate assumes it.".into(),
            suggested_question: Some("Is customer-hosted deployment confirmed?".into()),
            source_refs: vec!["T1".into(), "T2".into(), "T1".into()],
            related_items: vec![],
            importance: 0.9,
            urgency: 0.9,
            confidence: 0.9,
            future_work_risk: 0.9,
        }
    }

    fn valid(title: &str) -> Candidate {
        validate_candidate(&raw(title), &packet(), &MeetingState::new("live")).unwrap()
    }

    #[test]
    fn candidates_need_known_evidence_and_sane_fields() {
        let s = MeetingState::new("live");
        let c =
            validate_candidate(&raw("  Customer-hosted  never confirmed "), &packet(), &s).unwrap();
        assert_eq!(c.title, "Customer-hosted never confirmed");
        assert_eq!(c.source_refs, ["Mlive:T1", "Mlive:T2"]);
        assert_eq!(c.cited, ["T1", "T2"]);

        let mut r = raw("x");
        r.source_refs = vec!["T9".into()];
        assert_eq!(
            validate_candidate(&r, &packet(), &s),
            Err(RejectReason::UnknownEvidence("T9".into()))
        );
        r.source_refs = vec![];
        assert_eq!(
            validate_candidate(&r, &packet(), &s),
            Err(RejectReason::NoEvidence)
        );
        let mut r = raw("x");
        r.urgency = 1.2;
        assert_eq!(
            validate_candidate(&r, &packet(), &s),
            Err(RejectReason::BadConfidence(1.2))
        );
        let mut r = raw(" ");
        r.title = " ".into();
        assert_eq!(
            validate_candidate(&r, &packet(), &s),
            Err(RejectReason::EmptyText)
        );
        let mut r = raw("x");
        r.related_items = vec!["REQ-9".into()];
        assert_eq!(
            validate_candidate(&r, &packet(), &s),
            Err(RejectReason::UnknownItem("REQ-9".into()))
        );
    }

    #[test]
    fn the_score_is_value_times_confidence_times_urgency_minus_cost() {
        let p = InterventionPolicy::default();
        let mut c = valid("x");
        c.importance = 1.0;
        c.future_work_risk = 0.6;
        c.confidence = 0.9;
        c.urgency = 0.5;
        assert!((p.score(&c) - (0.8 * 0.9 * 0.5 - 0.1)).abs() < 1e-9);
    }

    #[test]
    fn weak_candidates_are_suppressed_and_logged() {
        let mut f = InterventionFilter::default();
        let mut shaky = valid("They seem interested");
        shaky.confidence = 0.5;
        assert!(f.consider(shaky, 0).is_none());
        let mut meh = valid("You may want more detail");
        meh.urgency = 0.3;
        assert!(f.consider(meh, 0).is_none());
        let decisions: Vec<&Decision> = f
            .log()
            .iter()
            .map(|e| match e {
                LogEntry::Considered { decision, .. } => decision,
                LogEntry::Dismissed { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            decisions,
            [
                &Decision::Suppressed(Suppressed::LowConfidence),
                &Decision::Suppressed(Suppressed::BelowThreshold)
            ]
        );
        assert!(f.active_cards().is_empty());
    }

    #[test]
    fn strong_candidates_show_but_respect_cooldown_cap_and_repeats() {
        let mut f = InterventionFilter::default();
        let a = f
            .consider(valid("Customer-hosted deployment never confirmed"), 0)
            .unwrap();
        assert_eq!(a.id, "CARD-1");
        // Too soon after the last card.
        assert!(f
            .consider(
                valid("Retention period conflicts with security doc"),
                2 * MIN
            )
            .is_none());
        // A rephrasing of a shown card is a repeat, whenever it comes.
        assert!(f
            .consider(
                valid("Deployment customer-hosted: never actually confirmed"),
                30 * MIN
            )
            .is_none());
        let b = f
            .consider(
                valid("Retention period conflicts with security doc"),
                5 * MIN,
            )
            .unwrap();
        assert_eq!(b.id, "CARD-2");

        // Five an hour at most.
        let mut f = InterventionFilter::default();
        let titles = [
            "Hosting region unclear",
            "Retention conflicts with policy",
            "Dataset owner missing",
            "Budget never confirmed",
            "Migration scope changed",
        ];
        for (i, t) in titles.iter().enumerate() {
            assert!(f.consider(valid(t), i as i64 * 5 * MIN).is_some(), "{t}");
        }
        assert!(f
            .consider(valid("yet another separate problem"), 25 * MIN)
            .is_none());
        assert!(matches!(
            f.log().last(),
            Some(LogEntry::Considered {
                decision: Decision::Suppressed(Suppressed::HourlyCap),
                ..
            })
        ));
        assert!(f
            .consider(valid("yet another separate problem"), 61 * MIN)
            .is_some());
    }

    #[test]
    fn endgame_shortens_the_cooldown() {
        let mut f = InterventionFilter::default();
        f.consider(valid("Hosting region unclear"), 0).unwrap();
        assert!(f.consider(valid("Dataset owner missing"), MIN).is_none());
        f.set_endgame(true);
        assert!(f
            .consider(valid("Budget never confirmed"), MIN + 1)
            .is_some());
    }

    #[test]
    fn dismissals_hide_the_card_block_repeats_and_are_logged() {
        let mut f = InterventionFilter::default();
        let card = f.consider(valid("Test dataset has no owner"), 0).unwrap();
        assert!(f.dismiss(&card.id, MIN));
        assert!(!f.dismiss(&card.id, MIN), "already dismissed");
        assert!(!f.dismiss("CARD-9", MIN));
        assert!(f.active_cards().is_empty());
        assert!(f
            .consider(valid("Owner missing for test dataset"), 10 * MIN)
            .is_none());
        assert!(matches!(
            &f.log()[1],
            LogEntry::Dismissed { card_id, at_ms: 60_000 } if card_id == "CARD-1"
        ));
    }

    #[test]
    fn invalid_candidates_are_logged_as_such() {
        let mut f = InterventionFilter::default();
        f.reject("Bad", &RejectReason::UnknownEvidence("T9".into()), 5);
        let json = serde_json::to_value(&f.log()[0]).unwrap();
        assert_eq!(json["decision"]["reason"], "invalid");
        assert_eq!(json["title"], "Bad");
    }

    #[test]
    fn the_candidates_schema_is_strict() {
        let schema = candidates_schema();
        let item = &schema["items"];
        assert_eq!(
            item["required"].as_array().unwrap().len(),
            item["properties"].as_object().unwrap().len()
        );
        let ok = serde_json::to_value(vec![raw("x")]).unwrap();
        wisp_reasoning::validate(&schema, &ok).unwrap();
        assert!(item["required"]
            .as_array()
            .unwrap()
            .contains(&json!("headline")));
    }

    #[test]
    fn a_candidate_keeps_a_short_headline_and_derives_a_missing_or_long_one() {
        let s = MeetingState::new("live");
        let mut r = raw("The plan relies on customer-hosted deployment, which nobody confirmed");
        r.headline = " Confirm customer hosting ".into();
        let c = validate_candidate(&r, &packet(), &s).unwrap();
        assert_eq!(c.headline, "Confirm customer hosting");

        r.headline = String::new();
        let c = validate_candidate(&r, &packet(), &s).unwrap();
        assert_eq!(c.headline, "The plan relies on customer-hosted deployment…");

        r.headline = "Please go and confirm the customer hosting plan today".into();
        let c = validate_candidate(&r, &packet(), &s).unwrap();
        assert_eq!(c.headline, "Please go and confirm the customer…");
    }

    #[test]
    fn output_without_a_headline_still_parses() {
        let old = json!({
            "kind": "follow_up", "title": "Send the DPA", "detail": "", "suggested_question": null,
            "source_refs": ["T1"], "related_items": [], "importance": 0.5, "urgency": 0.5,
            "confidence": 0.9, "future_work_risk": 0.5
        });
        let r: RawCandidate = serde_json::from_value(old).unwrap();
        assert_eq!(r.headline, "");
        let c = validate_candidate(&r, &packet(), &MeetingState::new("live")).unwrap();
        assert_eq!(c.headline, "Send the DPA");
        let mut saved = serde_json::to_value(&c).unwrap();
        saved.as_object_mut().unwrap().remove("headline");
        let back: Candidate = serde_json::from_value(saved).unwrap();
        assert_eq!(back.headline, "");
    }
}

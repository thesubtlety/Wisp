//! How a meeting type ([`wisp_library::MeetingType`]) shapes live intelligence: the pass cadence,
//! the card policy, a "This meeting" block in every pass, the wrap-up checklist, and the
//! suggestion of a type when none was chosen.
//!
//! General (the default) leaves everything as it was without types: default policies and no block.

use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_core::meeting_types::{Cadence, CardStyle, GENERAL_TYPE_ID};
use wisp_library::MeetingType;

use crate::intervene::{InterventionPolicy, Prefer};
use crate::runtime::{RuntimeConfig, TriggerPolicy};

/// Least confidence for a type suggestion to reach the user.
pub const MIN_GUESS_CONFIDENCE: f64 = 0.6;
/// How many passes may suggest a type; after these, the meeting stays as it is.
pub const GUESS_PASSES: u32 = 2;
const MAX_REASON_CHARS: usize = 200;

/// When passes run, for a cadence. Normal is the default policy.
pub fn trigger_policy(cadence: Cadence) -> TriggerPolicy {
    let policy = |chars, min, max| TriggerPolicy {
        min_new_chars: chars,
        min_interval: Duration::from_secs(min),
        max_wait: Duration::from_secs(max),
    };
    match cadence {
        Cadence::Fast => policy(300, 15, 30),
        Cadence::Normal => TriggerPolicy::default(),
        Cadence::Calm => policy(2_000, 30, 150),
    }
}

/// Which cards reach the user, for a card style. Gaps and balanced keep the default bar.
pub fn intervention_policy(style: CardStyle) -> InterventionPolicy {
    let base = InterventionPolicy::default();
    match style {
        CardStyle::Questions => InterventionPolicy {
            min_confidence: 0.5,
            threshold: 0.2,
            cooldown_ms: 2 * 60 * 1000,
            prefer: Prefer::Questions,
            ..base
        },
        CardStyle::Risks => InterventionPolicy {
            prefer: Prefer::Risks,
            ..base
        },
        CardStyle::Gaps | CardStyle::Balanced => base,
    }
}

/// Runs the meeting as `t`: its cadence, its card policy, and its block in every pass.
pub fn apply_type(config: &mut RuntimeConfig, t: MeetingType) {
    config.policy = trigger_policy(t.cadence());
    config.interventions = intervention_policy(t.card_style());
    config.meeting_type = Some(t);
}

fn style_hint(style: CardStyle) -> Option<&'static str> {
    match style {
        CardStyle::Questions => Some(
            "Candidates: prefer a short question You can ask right now, in suggested_question.",
        ),
        CardStyle::Gaps => Some(
            "Candidates: prefer gaps: missing owners or dates, missing requirements, unconfirmed \
             assumptions.",
        ),
        CardStyle::Risks => {
            Some("Candidates: prefer risks, conflicts and unconfirmed assumptions.")
        }
        CardStyle::Balanced => None,
    }
}

/// The "This meeting" block for an observer pass: what to watch for, and which candidates to
/// prefer. Empty when the type adds nothing (General).
pub fn render_type_block(t: &MeetingType) -> String {
    let watch = t.watch_for.trim();
    let hint = style_hint(t.card_style());
    if watch.is_empty() && hint.is_none() {
        return String::new();
    }
    let mut out = format!("## This meeting: {}\n\n", one_line(&t.name));
    if !watch.is_empty() {
        let _ = write!(out, "Watch for:\n{watch}\n\n");
    }
    if let Some(hint) = hint {
        let _ = write!(out, "{hint}\n\n");
    }
    out
}

/// The type's wrap-up checklist for the gap audit. Empty when it has none.
pub fn render_checklist(t: &MeetingType) -> String {
    if t.wrap_checklist.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "## This meeting: {}\n\nBefore it ends, check each of these. Report each one not yet done \
         as a gap (\"missing\" when nobody raised it):\n",
        one_line(&t.name)
    );
    for item in &t.wrap_checklist {
        let _ = writeln!(out, "- {}", one_line(item));
    }
    out.push('\n');
    out
}

/// Added to the observer instructions while the pass may suggest a type.
pub const GUESS_INSTRUCTIONS: &str = "\
Meeting type: nobody chose what kind of meeting this is. If the transcript makes it clear that it \
is one of the types listed under \"Meeting types\", put one entry in meeting_type_guess: the \
type_id exactly as listed, a confidence from 0 to 1, and a short reason. If it is unclear, or none \
fits, leave meeting_type_guess empty. Never guess from a greeting alone.";

/// The types a pass may suggest, for its context. General is never offered.
pub fn render_type_choices(choices: &[MeetingType]) -> String {
    let mut out = String::from("## Meeting types\n\n");
    for t in choices.iter().filter(|t| t.id != GENERAL_TYPE_ID) {
        let _ = writeln!(
            out,
            "- {}: {} — {}",
            t.id,
            one_line(&t.name),
            one_line(&t.description)
        );
    }
    out.push('\n');
    out
}

/// The schema of the pass's `meeting_type_guess` list: at most one entry is read, and the list may
/// be left out (see [`wisp_reasoning::OPTIONAL_MARK`]).
pub fn type_guess_schema() -> Value {
    json!({
        "type": "array",
        "x-optional": true,
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["type_id", "confidence", "reason"],
            "properties": {
                "type_id": {"type": "string"},
                "confidence": {"type": "number"},
                "reason": {"type": "string"}
            }
        }
    })
}

/// A suggested type as the model returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawTypeGuess {
    pub type_id: String,
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

/// A suggested type that checked out, for the user to accept or dismiss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeGuess {
    pub type_id: String,
    pub name: String,
    pub confidence: f64,
    pub reason: String,
}

/// Why a suggested type was dropped.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GuessReject {
    #[error("unknown meeting type {0}")]
    UnknownType(String),
    #[error("General is the default, not a suggestion")]
    General,
    #[error("confidence {0} is too low")]
    LowConfidence(f64),
}

/// Checks a suggestion: a type among `choices` other than General, confident enough.
pub fn validate_type_guess(
    raw: &RawTypeGuess,
    choices: &[MeetingType],
) -> Result<TypeGuess, GuessReject> {
    let id = raw.type_id.trim();
    if id == GENERAL_TYPE_ID {
        return Err(GuessReject::General);
    }
    let t = choices
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| GuessReject::UnknownType(id.to_owned()))?;
    if !(raw.confidence.is_finite() && (MIN_GUESS_CONFIDENCE..=1.0).contains(&raw.confidence)) {
        return Err(GuessReject::LowConfidence(raw.confidence));
    }
    Ok(TypeGuess {
        type_id: t.id.clone(),
        name: t.name.clone(),
        confidence: raw.confidence,
        reason: one_line(&raw.reason)
            .chars()
            .take(MAX_REASON_CHARS)
            .collect(),
    })
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intervene::{Candidate, CandidateKind};
    use wisp_core::meeting_types::{builtin_meeting_type, BUILTIN_MEETING_TYPES};

    fn builtin(id: &str) -> MeetingType {
        MeetingType::from_builtin(builtin_meeting_type(id).unwrap())
    }

    fn all() -> Vec<MeetingType> {
        BUILTIN_MEETING_TYPES
            .iter()
            .map(MeetingType::from_builtin)
            .collect()
    }

    fn candidate(kind: CandidateKind, question: Option<&str>) -> Candidate {
        Candidate {
            kind,
            headline: String::new(),
            title: "t".into(),
            detail: String::new(),
            suggested_question: question.map(Into::into),
            source_refs: vec![],
            cited: vec![],
            related_items: vec![],
            importance: 0.6,
            urgency: 0.7,
            confidence: 0.7,
            future_work_risk: 0.6,
        }
    }

    #[test]
    fn cadence_sets_the_trigger_policy() {
        let fast = trigger_policy(Cadence::Fast);
        assert_eq!(fast.min_new_chars, 300);
        assert_eq!(fast.min_interval, Duration::from_secs(15));
        assert_eq!(fast.max_wait, Duration::from_secs(30));
        assert_eq!(trigger_policy(Cadence::Normal), TriggerPolicy::default());
        let calm = trigger_policy(Cadence::Calm);
        assert_eq!(
            (calm.min_new_chars, calm.min_interval, calm.max_wait),
            (2_000, Duration::from_secs(30), Duration::from_secs(150))
        );
    }

    #[test]
    fn card_style_tunes_the_filter() {
        let default = InterventionPolicy::default();
        assert_eq!(intervention_policy(CardStyle::Gaps), default);
        assert_eq!(intervention_policy(CardStyle::Balanced), default);

        let questions = intervention_policy(CardStyle::Questions);
        assert!(questions.min_confidence < default.min_confidence);
        assert!(questions.threshold < default.threshold);
        assert!(questions.cooldown_ms < default.cooldown_ms);
        // This candidate scores 0.6 × 0.7 × 0.7 − 0.1 ≈ 0.194: under both bars on its own.
        let asked = candidate(CandidateKind::FollowUp, Some("Which region?"));
        let plain = candidate(CandidateKind::FollowUp, None);
        assert!(
            questions.score(&asked) >= questions.threshold,
            "a question is lifted"
        );
        assert!(questions.score(&plain) < questions.threshold);
        assert!(default.score(&asked) < default.threshold);

        let risks = intervention_policy(CardStyle::Risks);
        let conflict = candidate(CandidateKind::Conflict, None);
        let owner = candidate(CandidateKind::MissingOwner, None);
        assert!(risks.score(&conflict) > risks.score(&owner));
        assert_eq!(default.score(&conflict), default.score(&owner));
    }

    #[test]
    fn general_runs_exactly_as_before() {
        let general = builtin(GENERAL_TYPE_ID);
        let mut config = RuntimeConfig::default();
        apply_type(&mut config, general.clone());
        assert_eq!(config.policy, TriggerPolicy::default());
        assert_eq!(config.interventions, InterventionPolicy::default());
        assert_eq!(render_type_block(&general), "");
        assert_eq!(render_checklist(&general), "");
    }

    #[test]
    fn a_type_renders_its_block_and_checklist() {
        let interview = builtin("builtin-interview");
        let block = render_type_block(&interview);
        assert!(block.starts_with("## This meeting: Interview\n"));
        assert!(block.contains("follow-up question"));
        assert!(block.contains("suggested_question"));
        let checklist = render_checklist(&interview);
        assert!(checklist.contains("- Next steps and timeline stated\n"));
    }

    #[test]
    fn a_guess_must_name_a_listed_type_other_than_general_and_be_confident() {
        let choices = all();
        let raw = |id: &str, confidence: f64| RawTypeGuess {
            type_id: id.into(),
            confidence,
            reason: "  They asked about\nher last role. ".into(),
        };
        let ok = validate_type_guess(&raw(" builtin-interview ", 0.8), &choices).unwrap();
        assert_eq!(ok.type_id, "builtin-interview");
        assert_eq!(ok.name, "Interview");
        assert_eq!(ok.reason, "They asked about her last role.");
        assert_eq!(
            validate_type_guess(&raw("builtin-poker", 0.9), &choices),
            Err(GuessReject::UnknownType("builtin-poker".into()))
        );
        assert_eq!(
            validate_type_guess(&raw(GENERAL_TYPE_ID, 0.9), &choices),
            Err(GuessReject::General)
        );
        assert_eq!(
            validate_type_guess(&raw("builtin-interview", 0.5), &choices),
            Err(GuessReject::LowConfidence(0.5))
        );
        assert!(validate_type_guess(&raw("builtin-interview", f64::NAN), &choices).is_err());
        let listed = render_type_choices(&choices);
        assert!(listed.contains("- builtin-interview: Interview — "));
        assert!(!listed.contains(GENERAL_TYPE_ID));
    }
}

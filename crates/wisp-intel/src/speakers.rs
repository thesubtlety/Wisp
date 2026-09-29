//! Speaker-name suggestions: an observer pass may say that `Speaker 2` is probably "Laurie", when
//! the transcript shows it (a self-introduction, being addressed by name and answering, others
//! referring to them). The model proposes; [`validate_speaker_name`] checks the proposal against
//! the lines and the evidence packet, and [`SpeakerSuggestions`] keeps at most one live suggestion
//! per speaker for the meeting and remembers what the user dismissed. Accepting one is the shell's
//! job: it names the speaker the same way a manual rename does.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_core::speakers::unnamed_speaker_id;

use crate::evidence::{EvidencePacket, TranscriptLine};

/// Longest name accepted, in characters.
pub const MAX_NAME_CHARS: usize = 40;
/// Suggestions below this confidence are dropped.
pub const MIN_CONFIDENCE: f64 = 0.5;
/// Most suggestions taken from one pass.
pub const MAX_PER_PASS: usize = 4;

/// A speaker name as a model proposes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSpeakerName {
    /// The speaker label as the transcript shows it, like "Speaker 2".
    pub speaker: String,
    pub name: String,
    /// Evidence IDs, like "T12".
    pub evidence: Vec<String>,
    pub confidence: f64,
}

/// A checked suggestion, ready to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerSuggestion {
    /// The label the lines carry, like "Speaker 2".
    pub speaker: String,
    /// The diarized id behind that label (0-based; "Speaker 2" is 1).
    pub speaker_id: u32,
    pub name: String,
    pub confidence: f64,
    /// The evidence IDs as cited.
    pub evidence: Vec<String>,
    /// The first cited evidence as a person reads it, like "Speaker 1: Good afternoon Laurie".
    pub quote: Option<String>,
}

/// Why a proposed name was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakerReject {
    /// No line in the meeting carries this label.
    UnknownSpeaker(String),
    /// "You", "Them", or a speaker that already has a name.
    NotNameable(String),
    BadName,
    /// Another speaker already goes by this name.
    NameInUse(String),
    NoEvidence,
    UnknownEvidence(String),
    LowConfidence,
}

/// The JSON Schema for the pass's `speaker_names` list (strict, but the list itself may be left
/// out: see [`wisp_reasoning::OPTIONAL_MARK`]).
pub fn speaker_names_schema() -> Value {
    json!({
        "type": "array",
        "x-optional": true,
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["speaker", "name", "evidence", "confidence"],
            "properties": {
                "speaker": {"type": "string"},
                "name": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "string"}},
                "confidence": {"type": "number"}
            }
        }
    })
}

/// Checks one proposal against the meeting's lines and the pass's evidence packet.
pub fn validate_speaker_name(
    raw: &RawSpeakerName,
    packet: &EvidencePacket,
    lines: &[TranscriptLine],
) -> Result<SpeakerSuggestion, SpeakerReject> {
    let speaker = raw.speaker.trim();
    if !lines.iter().any(|l| l.speaker == speaker) {
        return Err(SpeakerReject::UnknownSpeaker(speaker.to_owned()));
    }
    let speaker_id = unnamed_speaker_id(speaker)
        .ok_or_else(|| SpeakerReject::NotNameable(speaker.to_owned()))?;
    let name = raw.name.trim();
    let n = name.chars().count();
    if n == 0 || n > MAX_NAME_CHARS || is_reserved(name) {
        return Err(SpeakerReject::BadName);
    }
    let in_use = lines
        .iter()
        .any(|l| unnamed_speaker_id(&l.speaker).is_none() && l.speaker.eq_ignore_ascii_case(name));
    if in_use {
        return Err(SpeakerReject::NameInUse(name.to_owned()));
    }
    if !(raw.confidence.is_finite() && (MIN_CONFIDENCE..=1.0).contains(&raw.confidence)) {
        return Err(SpeakerReject::LowConfidence);
    }
    if raw.evidence.is_empty() {
        return Err(SpeakerReject::NoEvidence);
    }
    let mut evidence: Vec<String> = Vec::new();
    for alias in &raw.evidence {
        let alias = alias.trim();
        if packet.resolve(alias).is_none() {
            return Err(SpeakerReject::UnknownEvidence(alias.to_owned()));
        }
        if !evidence.iter().any(|e| e == alias) {
            evidence.push(alias.to_owned());
        }
    }
    let first = &evidence[0];
    let quote = first
        .strip_prefix('T')
        .and_then(|n| n.parse::<i64>().ok())
        .and_then(|idx| lines.iter().find(|l| l.idx == idx))
        .map(|l| format!("{}: {}", l.speaker, l.text.trim()))
        .or_else(|| packet.detail(first).map(|d| d.text.clone()));
    Ok(SpeakerSuggestion {
        speaker: speaker.to_owned(),
        speaker_id: speaker_id.0,
        name: name.to_owned(),
        confidence: raw.confidence,
        evidence,
        quote,
    })
}

/// Labels that are never a person's name.
fn is_reserved(name: &str) -> bool {
    ["you", "them", "me", "unknown"].contains(&name.to_lowercase().as_str())
        || unnamed_speaker_id(name).is_some()
}

/// A meeting's live suggestions (one per speaker) and the ones the user dismissed.
#[derive(Debug, Clone, Default)]
pub struct SpeakerSuggestions {
    live: BTreeMap<String, SpeakerSuggestion>,
    /// (speaker label, lowercased name)
    dismissed: BTreeSet<(String, String)>,
}

impl SpeakerSuggestions {
    /// Takes a pass's checked suggestions: dismissed pairs are skipped, and a speaker keeps the
    /// most confident one. Returns whether the live list changed.
    pub fn offer(&mut self, suggestions: Vec<SpeakerSuggestion>) -> bool {
        let mut changed = false;
        for s in suggestions {
            if self
                .dismissed
                .contains(&(s.speaker.clone(), s.name.to_lowercase()))
            {
                continue;
            }
            let replace = match self.live.get(&s.speaker) {
                None => true,
                Some(old) => s.confidence > old.confidence,
            };
            if replace {
                self.live.insert(s.speaker.clone(), s);
                changed = true;
            }
        }
        changed
    }

    /// The user dismissed `name` for `speaker`: drop it and never suggest the pair again.
    pub fn dismiss(&mut self, speaker: &str, name: &str) {
        let key = (speaker.trim().to_owned(), name.trim().to_lowercase());
        if self
            .live
            .get(&key.0)
            .is_some_and(|s| s.name.to_lowercase() == key.1)
        {
            self.live.remove(&key.0);
        }
        self.dismissed.insert(key);
    }

    /// The speaker now has a name (or a different label): nothing to suggest for it.
    pub fn named(&mut self, speaker: &str) {
        self.live.remove(speaker);
    }

    /// The live suggestions, by speaker label.
    pub fn current(&self) -> Vec<SpeakerSuggestion> {
        self.live.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::render_lines;

    fn line(idx: i64, speaker: &str, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 1000,
            speaker: speaker.into(),
            text: text.into(),
        }
    }

    fn meeting() -> (Vec<TranscriptLine>, EvidencePacket) {
        let lines = vec![
            line(0, "Speaker 1", "Good afternoon Laurie."),
            line(1, "Speaker 2", "Hi! Thanks for having me."),
            line(2, "You", "Let's start."),
            line(3, "Bob", "Agenda first."),
        ];
        let mut packet = EvidencePacket::default();
        render_lines(&mut packet, "live", "New", &lines);
        (lines, packet)
    }

    fn raw(speaker: &str, name: &str, evidence: &[&str], confidence: f64) -> RawSpeakerName {
        RawSpeakerName {
            speaker: speaker.into(),
            name: name.into(),
            evidence: evidence.iter().map(|e| (*e).to_owned()).collect(),
            confidence,
        }
    }

    fn check(r: RawSpeakerName) -> Result<SpeakerSuggestion, SpeakerReject> {
        let (lines, packet) = meeting();
        validate_speaker_name(&r, &packet, &lines)
    }

    #[test]
    fn a_grounded_suggestion_passes_with_its_id_and_quote() {
        let s = check(raw(" Speaker 2 ", " Laurie ", &["T0", "T1", "T0"], 0.8)).unwrap();
        assert_eq!((s.speaker.as_str(), s.speaker_id), ("Speaker 2", 1));
        assert_eq!(s.name, "Laurie");
        assert_eq!(s.evidence, ["T0", "T1"]);
        assert_eq!(
            s.quote.as_deref(),
            Some("Speaker 1: Good afternoon Laurie.")
        );
    }

    #[test]
    fn the_speaker_must_be_an_unnamed_label_in_the_meeting() {
        assert_eq!(
            check(raw("Speaker 7", "Laurie", &["T0"], 0.9)),
            Err(SpeakerReject::UnknownSpeaker("Speaker 7".into()))
        );
        assert_eq!(
            check(raw("You", "Laurie", &["T0"], 0.9)),
            Err(SpeakerReject::NotNameable("You".into()))
        );
        // Already named.
        assert_eq!(
            check(raw("Bob", "Robert", &["T3"], 0.9)),
            Err(SpeakerReject::NotNameable("Bob".into()))
        );
    }

    #[test]
    fn names_are_short_real_and_not_taken() {
        for bad in [
            "",
            "  ",
            "You",
            "Speaker 3",
            &"x".repeat(MAX_NAME_CHARS + 1),
        ] {
            assert_eq!(
                check(raw("Speaker 2", bad, &["T0"], 0.9)),
                Err(SpeakerReject::BadName),
                "{bad}"
            );
        }
        assert!(check(raw("Speaker 2", &"x".repeat(MAX_NAME_CHARS), &["T0"], 0.9)).is_ok());
        assert_eq!(
            check(raw("Speaker 2", "bob", &["T0"], 0.9)),
            Err(SpeakerReject::NameInUse("bob".into()))
        );
    }

    #[test]
    fn evidence_must_exist_and_confidence_must_be_real() {
        assert_eq!(
            check(raw("Speaker 2", "Laurie", &[], 0.9)),
            Err(SpeakerReject::NoEvidence)
        );
        assert_eq!(
            check(raw("Speaker 2", "Laurie", &["T0", "T9"], 0.9)),
            Err(SpeakerReject::UnknownEvidence("T9".into()))
        );
        for c in [0.2, 1.5, f64::NAN] {
            assert_eq!(
                check(raw("Speaker 2", "Laurie", &["T0"], c)),
                Err(SpeakerReject::LowConfidence)
            );
        }
    }

    fn suggestion(speaker: &str, name: &str, confidence: f64) -> SpeakerSuggestion {
        SpeakerSuggestion {
            speaker: speaker.into(),
            speaker_id: unnamed_speaker_id(speaker).unwrap().0,
            name: name.into(),
            confidence,
            evidence: vec!["T0".into()],
            quote: None,
        }
    }

    #[test]
    fn one_live_suggestion_per_speaker_the_most_confident() {
        let mut s = SpeakerSuggestions::default();
        assert!(s.offer(vec![
            suggestion("Speaker 2", "Laurie", 0.7),
            suggestion("Speaker 2", "Lori", 0.6),
            suggestion("Speaker 3", "Sam", 0.8),
        ]));
        let names: Vec<String> = s.current().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["Laurie", "Sam"]);
        assert!(!s.offer(vec![suggestion("Speaker 2", "Lori", 0.65)]));
        assert!(s.offer(vec![suggestion("Speaker 2", "Lori", 0.9)]));
        assert_eq!(s.current()[0].name, "Lori");
    }

    #[test]
    fn dismissed_pairs_are_not_suggested_again_and_named_speakers_drop() {
        let mut s = SpeakerSuggestions::default();
        s.offer(vec![suggestion("Speaker 2", "Laurie", 0.7)]);
        s.dismiss("Speaker 2", "laurie");
        assert!(s.current().is_empty());
        assert!(!s.offer(vec![suggestion("Speaker 2", "Laurie", 0.95)]));
        // Another name for the same speaker is still fair game.
        assert!(s.offer(vec![suggestion("Speaker 2", "Sam", 0.7)]));
        s.named("Speaker 2");
        assert!(s.current().is_empty());
    }
}

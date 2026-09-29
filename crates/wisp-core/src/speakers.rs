//! Speaker names: the labels a user gives diarized speakers, and the one rule for what a line shows.
//!
//! Diarized ids are 0-based ([`SpeakerId`]); unnamed speakers read as `Speaker n+1`. Every shell
//! (exports, the reasoning context, the Library view) labels speakers through these functions, so a
//! rename shows up the same way everywhere.

use std::collections::BTreeMap;

use crate::transcript::SpeakerId;

/// Names the user gave a meeting's speakers, keyed by the diarized id (`SpeakerId.0`).
pub type SpeakerNames = BTreeMap<u32, String>;

/// The user's name for `id`, if one is set and not blank.
pub fn speaker_name(id: SpeakerId, names: &SpeakerNames) -> Option<&str> {
    names.get(&id.0).map(|n| n.trim()).filter(|n| !n.is_empty())
}

/// The label for a diarized speaker: its name when set, else `Speaker n+1`.
pub fn speaker_display(id: SpeakerId, names: &SpeakerNames) -> String {
    speaker_name(id, names)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Speaker {}", id.0 + 1))
}

/// The id behind an unnamed speaker's label (`Speaker 3` is id 2): the inverse of
/// [`speaker_display`] for a speaker with no name. `None` for anything else ("You", "Them", a name).
pub fn unnamed_speaker_id(label: &str) -> Option<SpeakerId> {
    let digits = label.strip_prefix("Speaker ")?;
    let n: u32 = digits.parse().ok()?;
    if n.to_string() != digits {
        return None; // "Speaker 02", "Speaker +2"
    }
    n.checked_sub(1).map(SpeakerId)
}

/// Who spoke a line, as the reasoning context and the Library see it.
///
/// The microphone is "You", unless diarization split the mic stream (a room with several people on
/// one mic) and the user named that speaker: then the name wins, since "You" would be wrong. An
/// unnamed mic speaker stays "You", which is right in the common one-person-per-mic case. The other
/// side is its speaker's name or `Speaker n+1`, and "Them" when not diarized.
pub fn line_speaker(is_mic: bool, speaker: Option<SpeakerId>, names: &SpeakerNames) -> String {
    match (is_mic, speaker) {
        (true, Some(id)) => speaker_name(id, names).unwrap_or("You").to_owned(),
        (true, None) => "You".to_owned(),
        (false, Some(id)) => speaker_display(id, names),
        (false, None) => "Them".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pairs: &[(u32, &str)]) -> SpeakerNames {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn unnamed_speakers_are_one_based() {
        assert_eq!(
            speaker_display(SpeakerId(0), &SpeakerNames::new()),
            "Speaker 1"
        );
    }

    #[test]
    fn unnamed_labels_parse_back_to_their_id() {
        for id in [0, 1, 41] {
            let label = speaker_display(SpeakerId(id), &SpeakerNames::new());
            assert_eq!(unnamed_speaker_id(&label), Some(SpeakerId(id)));
        }
        for other in [
            "You",
            "Them",
            "Laurie",
            "Speaker 0",
            "Speaker x",
            "Speaker",
            "Speaker 02",
        ] {
            assert_eq!(unnamed_speaker_id(other), None, "{other}");
        }
    }

    #[test]
    fn a_name_replaces_the_number_and_blank_names_are_ignored() {
        let n = names(&[(1, " Alice "), (2, "  ")]);
        assert_eq!(speaker_display(SpeakerId(1), &n), "Alice");
        assert_eq!(speaker_display(SpeakerId(2), &n), "Speaker 3");
    }

    #[test]
    fn the_mic_is_you_unless_its_diarized_speaker_has_a_name() {
        let n = names(&[(0, "Bob")]);
        assert_eq!(line_speaker(true, None, &n), "You");
        assert_eq!(line_speaker(true, Some(SpeakerId(1)), &n), "You");
        assert_eq!(line_speaker(true, Some(SpeakerId(0)), &n), "Bob");
    }

    #[test]
    fn the_other_side_is_named_numbered_or_them() {
        let n = names(&[(0, "Bob")]);
        assert_eq!(line_speaker(false, None, &n), "Them");
        assert_eq!(line_speaker(false, Some(SpeakerId(0)), &n), "Bob");
        assert_eq!(line_speaker(false, Some(SpeakerId(1)), &n), "Speaker 2");
    }
}

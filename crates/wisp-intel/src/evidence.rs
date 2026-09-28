//! The evidence packet for one reasoning call: every piece of context gets a short ID the model
//! cites, and the packet remembers which canonical [`SourceRef`] each ID stands for. The reducer
//! accepts only IDs that are in the packet, so a model cannot cite evidence it was never given.
//!
//! IDs: `T12` is segment 12 of the current meeting, `D17:C4` chunk 4 of project source 17, `M1:T221`
//! segment 221 of the first previous meeting in the packet.

use std::collections::BTreeMap;
use std::fmt::Write;

use wisp_library::{meeting_ref, Snippet, SnippetOrigin};

use crate::model::SourceRef;

/// A final transcript line of the current meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    /// The segment index, as stored.
    pub idx: i64,
    /// Offset into the meeting.
    pub start_ms: i64,
    /// Who spoke: "You", "Them", "Speaker 2", a name.
    pub speaker: String,
    pub text: String,
}

impl TranscriptLine {
    /// A stored segment as a line: the microphone is "You"; the other side is its diarized speaker
    /// ("Speaker 2") when known, else "Them".
    pub fn from_segment(segment: &wisp_library::Segment) -> Self {
        let speaker = match (segment.source.as_str(), segment.speaker) {
            ("mic", _) => "You".to_owned(),
            (_, Some(n)) => format!("Speaker {}", n + 1),
            _ => "Them".to_owned(),
        };
        Self {
            idx: segment.idx,
            start_ms: segment.start_ms,
            speaker,
            text: segment.text.clone(),
        }
    }
}

/// What an evidence ID shows a person: where it is from, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceDetail {
    /// "This meeting 03:04, Them", "security.md, line 40", "previous meeting \"Kickoff\" at 31:04".
    pub label: String,
    pub text: String,
}

/// The IDs a reasoning call may cite and what they stand for.
#[derive(Debug, Clone, Default)]
pub struct EvidencePacket {
    by_alias: BTreeMap<String, SourceRef>,
    details: BTreeMap<String, EvidenceDetail>,
    /// Previous meetings by canonical id, in order of first appearance.
    meetings: Vec<String>,
}

impl EvidencePacket {
    /// The canonical ref for `alias`, if the packet holds it.
    pub fn resolve(&self, alias: &str) -> Option<&SourceRef> {
        self.by_alias.get(alias.trim())
    }

    /// The short ID this packet uses for a canonical ref, if it holds one.
    pub fn alias_for(&self, canonical: &str) -> Option<&str> {
        self.by_alias
            .iter()
            .find(|(_, r)| r.as_str() == canonical)
            .map(|(a, _)| a.as_str())
    }

    /// Where an ID is from and its text, for showing a citation.
    pub fn detail(&self, alias: &str) -> Option<&EvidenceDetail> {
        self.details.get(alias.trim())
    }

    pub fn len(&self) -> usize {
        self.by_alias.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_alias.is_empty()
    }

    /// The short ID for a line of the current meeting, registering it.
    fn add_line(&mut self, meeting_id: &str, line: &TranscriptLine) -> String {
        let alias = format!("T{}", line.idx);
        self.by_alias
            .insert(alias.clone(), meeting_ref(meeting_id, line.idx));
        self.details.insert(
            alias.clone(),
            EvidenceDetail {
                label: format!("This meeting {}, {}", clock(line.start_ms), line.speaker),
                text: one_line(&line.text),
            },
        );
        alias
    }

    /// The short ID for a retrieved snippet, registering it.
    fn add_snippet(&mut self, snippet: &Snippet, label: &str) -> String {
        let alias = match &snippet.origin {
            SnippetOrigin::Source {
                source_id,
                chunk_idx,
                ..
            } => format!("D{source_id}:C{chunk_idx}"),
            SnippetOrigin::Meeting {
                meeting_id,
                segment_idx,
                ..
            } => {
                let n = match self.meetings.iter().position(|m| m == meeting_id) {
                    Some(i) => i + 1,
                    None => {
                        self.meetings.push(meeting_id.clone());
                        self.meetings.len()
                    }
                };
                format!("M{n}:T{segment_idx}")
            }
        };
        self.by_alias.insert(alias.clone(), snippet.ref_id.clone());
        self.details.insert(
            alias.clone(),
            EvidenceDetail {
                label: label.to_owned(),
                text: snippet.text.trim().to_owned(),
            },
        );
        alias
    }
}

/// `mm:ss` (or `h:mm:ss`) for an offset into a meeting.
pub(crate) fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// The transcript lines, split into lines already analyzed (context) and new lines, rendered with
/// their IDs. Registers every line in `packet`.
pub(crate) fn render_transcript(
    packet: &mut EvidencePacket,
    meeting_id: &str,
    earlier: &[TranscriptLine],
    new: &[TranscriptLine],
) -> String {
    let mut out = render_lines(
        packet,
        meeting_id,
        "Earlier in this meeting (already analyzed)",
        earlier,
    );
    out.push_str(&render_lines(
        packet,
        meeting_id,
        "New in this meeting",
        new,
    ));
    out
}

/// One titled section of transcript lines with their IDs; empty when there are no lines. Registers
/// every line in `packet`.
pub(crate) fn render_lines(
    packet: &mut EvidencePacket,
    meeting_id: &str,
    title: &str,
    lines: &[TranscriptLine],
) -> String {
    let mut out = String::new();
    if lines.is_empty() {
        return out;
    }
    let _ = writeln!(out, "## {title}\n");
    for line in lines {
        let alias = packet.add_line(meeting_id, line);
        let _ = writeln!(
            out,
            "[{alias}] {} {}: {}",
            clock(line.start_ms),
            line.speaker,
            one_line(&line.text)
        );
    }
    out.push('\n');
    out
}

/// Retrieved project context, rendered with IDs. Registers every snippet in `packet`.
pub(crate) fn render_snippets(packet: &mut EvidencePacket, snippets: &[Snippet]) -> String {
    if snippets.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Project context (retrieved)\n\n");
    for snippet in snippets {
        let heading = match &snippet.origin {
            SnippetOrigin::Source {
                label, line_start, ..
            } => match line_start {
                Some(line) => format!("{label}, line {line}"),
                None => label.clone(),
            },
            SnippetOrigin::Meeting {
                title, start_ms, ..
            } => format!("previous meeting \"{title}\" at {}", clock(*start_ms)),
        };
        let alias = packet.add_snippet(snippet, &heading);
        let _ = writeln!(out, "[{alias}] {heading}\n{}\n", snippet.text.trim());
    }
    out
}

/// Collapses a line's whitespace so one transcript line stays one prompt line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(idx: i64, text: &str) -> TranscriptLine {
        TranscriptLine {
            idx,
            start_ms: idx * 1000,
            speaker: "Them".into(),
            text: text.into(),
        }
    }

    fn source(id: i64, chunk: i64) -> Snippet {
        Snippet {
            ref_id: wisp_library::source_ref(id, chunk),
            origin: SnippetOrigin::Source {
                source_id: id,
                chunk_idx: chunk,
                label: "security.md".into(),
                line_start: Some(40),
            },
            text: "Data stays in the EU.".into(),
            score: 1.0,
        }
    }

    fn previous(meeting: &str, seg: i64) -> Snippet {
        Snippet {
            ref_id: meeting_ref(meeting, seg),
            origin: SnippetOrigin::Meeting {
                meeting_id: meeting.into(),
                title: "Kickoff".into(),
                started_at_ms: 0,
                segment_idx: seg,
                start_ms: 1_864_000,
            },
            text: "We said SAML.".into(),
            score: 1.0,
        }
    }

    #[test]
    fn ids_map_back_to_canonical_refs() {
        let mut p = EvidencePacket::default();
        let text = render_transcript(&mut p, "live", &[line(3, "old")], &[line(4, "new\n  line")]);
        let ctx = render_snippets(
            &mut p,
            &[
                source(17, 4),
                previous("m-a", 221),
                previous("m-b", 2),
                previous("m-a", 5),
            ],
        );
        assert_eq!(p.resolve("T3").map(String::as_str), Some("Mlive:T3"));
        assert_eq!(p.resolve(" T4 ").map(String::as_str), Some("Mlive:T4"));
        assert_eq!(p.resolve("D17:C4").map(String::as_str), Some("S17:C4"));
        assert_eq!(p.resolve("M1:T221").map(String::as_str), Some("Mm-a:T221"));
        assert_eq!(p.resolve("M2:T2").map(String::as_str), Some("Mm-b:T2"));
        assert_eq!(p.resolve("M1:T5").map(String::as_str), Some("Mm-a:T5"));
        assert_eq!(p.resolve("T5"), None);
        assert_eq!(p.resolve("S17:C4"), None, "canonical refs are not citable");
        assert_eq!(p.len(), 6);
        assert_eq!(
            p.detail("T4"),
            Some(&EvidenceDetail {
                label: "This meeting 00:04, Them".into(),
                text: "new line".into(),
            })
        );
        assert_eq!(p.detail("D17:C4").unwrap().label, "security.md, line 40");
        assert_eq!(p.detail("M1:T221").unwrap().text, "We said SAML.");

        assert!(
            text.contains("## Earlier in this meeting (already analyzed)\n\n[T3] 00:03 Them: old")
        );
        assert!(text.contains("## New in this meeting\n\n[T4] 00:04 Them: new line"));
        assert!(ctx.contains("[D17:C4] security.md, line 40\nData stays in the EU."));
        assert!(ctx.contains("[M1:T221] previous meeting \"Kickoff\" at 31:04\nWe said SAML."));
    }

    #[test]
    fn stored_segments_become_lines_with_speaker_labels() {
        let seg = |source: &str, speaker: Option<i64>| wisp_library::Segment {
            idx: 7,
            start_ms: 7000,
            end_ms: 8000,
            speaker,
            source: source.into(),
            text: "hi".into(),
        };
        assert_eq!(
            TranscriptLine::from_segment(&seg("mic", Some(0))).speaker,
            "You"
        );
        assert_eq!(
            TranscriptLine::from_segment(&seg("system", Some(1))).speaker,
            "Speaker 2"
        );
        assert_eq!(
            TranscriptLine::from_segment(&seg("system", None)).speaker,
            "Them"
        );
        assert_eq!(TranscriptLine::from_segment(&seg("file", None)).idx, 7);
    }

    #[test]
    fn alias_for_maps_canonical_refs_back() {
        let mut p = EvidencePacket::default();
        render_transcript(&mut p, "live", &[], &[line(4, "x")]);
        assert_eq!(p.alias_for("Mlive:T4"), Some("T4"));
        assert_eq!(p.alias_for("Mlive:T5"), None);
    }

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(65_000), "01:05");
        assert_eq!(clock(3_725_000), "1:02:05");
        assert_eq!(clock(-5), "00:00");
    }
}

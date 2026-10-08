//! Rewriting a speaker's label where the model already wrote it into a meeting's artifacts.
//!
//! The transcript stores speaker *ids* and resolves the name at display time, so renaming a speaker
//! relabels it for free (see [`crate::speakers`]). The derived artifacts — the state log, a saved
//! ask thread, prompt-run outputs, the summary — are instead frozen model prose with the label
//! baked in as text. When the user renames a speaker after those were produced, these functions
//! replace the old label with the new one in that prose.
//!
//! The match is whole-token (bounded by non-word characters) so `Speaker 4` does not touch
//! `Speaker 40` and `Bob` does not touch `Bobby`, while `Bob's` and `Bob.` still match. It is a
//! shallow text swap: a label the model paraphrased ("the designer", "she") is not reached, and a
//! name that is also a common word can over-match. Quoted evidence — a citation's transcript text —
//! is left as spoken; only the model's own words are rewritten.

use crate::ask::AskAnswer;
use crate::ops::ResolvedOp;

/// Replaces whole-token occurrences of `from` with `to`. A token is bounded by non-word characters
/// (letters, digits and `_`) or the ends of the string, mirroring a `\b..\b` match. Case-sensitive.
pub fn relabel_text(s: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from == to || !s.contains(from) {
        return s.to_owned();
    }
    let from_chars: Vec<char> = from.chars().collect();
    let starts_word = from_chars.first().copied().is_some_and(is_word_char);
    let ends_word = from_chars.last().copied().is_some_and(is_word_char);
    let chars: Vec<char> = s.chars().collect();
    let m = from_chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if i + m <= chars.len() && chars[i..i + m] == from_chars[..] {
            // A boundary holds when the neighbouring char is not a word char — or there is none.
            let before_ok = !starts_word || i == 0 || !is_word_char(chars[i - 1]);
            let after_ok = !ends_word || i + m == chars.len() || !is_word_char(chars[i + m]);
            if before_ok && after_ok {
                out.push_str(to);
                i += m;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn relabel_opt(s: &mut Option<String>, from: &str, to: &str) {
    if let Some(text) = s {
        *text = relabel_text(text, from, to);
    }
}

/// Relabels the model's prose in one state op: an item's text and its owner. Evidence refs, ids,
/// dates and lifecycle are untouched.
pub fn relabel_op(op: &mut ResolvedOp, from: &str, to: &str) {
    match op {
        ResolvedOp::Add { text, owner, .. } => {
            *text = relabel_text(text, from, to);
            relabel_opt(owner, from, to);
        }
        ResolvedOp::Update { text, owner, .. } => {
            relabel_opt(text, from, to);
            relabel_opt(owner, from, to);
        }
        ResolvedOp::UserEdit { text, owner, .. } => {
            relabel_opt(text, from, to);
            relabel_opt(owner, from, to);
        }
        ResolvedOp::SetLifecycle { .. } => {}
    }
}

/// Relabels a checked answer: the short and full answer, and each citation's display label. A
/// citation's `text` is quoted transcript, so it is left as spoken.
pub fn relabel_answer(answer: &mut AskAnswer, from: &str, to: &str) {
    answer.short = relabel_text(&answer.short, from, to);
    answer.answer = relabel_text(&answer.answer, from, to);
    for c in &mut answer.citations {
        c.label = relabel_text(&c.label, from, to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::Citation;
    use crate::model::{EpistemicStatus, ItemKind};

    #[test]
    fn replaces_whole_tokens_only() {
        assert_eq!(
            relabel_text("Speaker 4 spoke", "Speaker 4", "Bob"),
            "Bob spoke"
        );
        // A longer number next to it is a different speaker and must not match.
        assert_eq!(
            relabel_text("Speaker 40 spoke", "Speaker 4", "Bob"),
            "Speaker 40 spoke"
        );
        // A name inside a longer word is not the speaker.
        assert_eq!(relabel_text("Bobby waved", "Bob", "Robert"), "Bobby waved");
        // Trailing punctuation and possessives still bound the token.
        assert_eq!(relabel_text("Ask Bob.", "Bob", "Robert"), "Ask Robert.");
        assert_eq!(relabel_text("Bob's plan", "Bob", "Robert"), "Robert's plan");
    }

    #[test]
    fn replaces_every_occurrence() {
        assert_eq!(
            relabel_text("Speaker 4 and Speaker 4", "Speaker 4", "Bob"),
            "Bob and Bob"
        );
    }

    #[test]
    fn no_op_cases_are_cheap_and_safe() {
        assert_eq!(
            relabel_text("nothing here", "Speaker 4", "Bob"),
            "nothing here"
        );
        assert_eq!(relabel_text("same", "same", "same"), "same");
        assert_eq!(relabel_text("x", "", "Bob"), "x");
    }

    #[test]
    fn relabels_an_add_ops_text_and_owner() {
        let mut op = ResolvedOp::Add {
            id: "REQ-1".into(),
            kind: ItemKind::Decision,
            text: "Speaker 4 owns the deploy".into(),
            status: EpistemicStatus::Stated,
            confidence: 0.9,
            source_refs: vec![],
            related_items: vec![],
            owner: Some("Speaker 4".into()),
            due: Some("Speaker 4 by Friday".into()),
        };
        relabel_op(&mut op, "Speaker 4", "Bob");
        let ResolvedOp::Add {
            text, owner, due, ..
        } = op
        else {
            panic!("kind changed");
        };
        assert_eq!(text, "Bob owns the deploy");
        assert_eq!(owner.as_deref(), Some("Bob"));
        assert_eq!(
            due.as_deref(),
            Some("Speaker 4 by Friday"),
            "dates untouched"
        );
    }

    #[test]
    fn relabels_an_answer_but_not_quoted_evidence() {
        let mut answer = AskAnswer {
            short: "Speaker 4 decided".into(),
            answer: "Speaker 4 will ship it.".into(),
            citations: vec![Citation {
                id: "T12".into(),
                label: "Speaker 4 · 00:12".into(),
                text: "I am Speaker 4 and I will ship it.".into(),
                source_ref: None,
                item_id: None,
            }],
            unknown_citations: vec![],
            grounded: true,
        };
        relabel_answer(&mut answer, "Speaker 4", "Bob");
        assert_eq!(answer.short, "Bob decided");
        assert_eq!(answer.answer, "Bob will ship it.");
        assert_eq!(answer.citations[0].label, "Bob · 00:12");
        assert_eq!(
            answer.citations[0].text, "I am Speaker 4 and I will ship it.",
            "quoted transcript is left as spoken"
        );
    }
}

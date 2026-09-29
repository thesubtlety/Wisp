//! Short forms for reading at a glance during a meeting: a card's or gap's headline, and Ask's
//! short answer. The model is asked for them; when one is missing or too long, it is derived
//! here from the text it summarizes, so older or partial outputs still show something skimmable.

/// Most words in a card's or gap's headline.
pub const HEADLINE_WORDS: usize = 6;
/// Most words in Ask's short answer.
pub const SHORT_ANSWER_WORDS: usize = 12;
/// Characters per allowed word before a short form counts as too long anyway (one long run-on
/// "word" in a language written without spaces).
const CHARS_PER_WORD: usize = 10;

/// `given` with whitespace collapsed, if it has at most `max_words` words; otherwise the first
/// `max_words` words of `given` (or of `full`, when `given` is blank), ending in "…" when cut.
pub fn fit(given: &str, full: &str, max_words: usize) -> String {
    let given = collapse(given);
    if !given.is_empty() && fits(&given, max_words) {
        return given;
    }
    let source = if given.is_empty() {
        collapse(full)
    } else {
        given
    };
    truncate(&source, max_words)
}

/// A headline for a card or gap: see [`fit`] with [`HEADLINE_WORDS`].
pub fn headline(given: &str, full: &str) -> String {
    fit(given, full, HEADLINE_WORDS)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fits(text: &str, max_words: usize) -> bool {
    text.split(' ').count() <= max_words && text.chars().count() <= max_words * CHARS_PER_WORD
}

fn truncate(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    let mut out = words
        .iter()
        .take(max_words)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let mut cut = words.len() > max_words;
    let max_chars = max_words * CHARS_PER_WORD;
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        cut = true;
    }
    if !cut {
        return out;
    }
    let trimmed = out.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, ',' | '.' | ';' | ':' | '-' | '–' | '—' | '(')
    });
    format!("{trimmed}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_enough_headline_is_kept_as_given() {
        assert_eq!(
            headline("  Confirm Azure   EU region ", "ignored"),
            "Confirm Azure EU region"
        );
    }

    #[test]
    fn a_missing_headline_comes_from_the_first_words_of_the_full_text() {
        assert_eq!(
            headline(
                "",
                "US East hosting breaks the EU-only data rule, per the DPA."
            ),
            "US East hosting breaks the EU-only…"
        );
        assert_eq!(
            headline("", "Dataset date slipped."),
            "Dataset date slipped."
        );
    }

    #[test]
    fn a_long_headline_is_cut_to_six_words_without_trailing_punctuation() {
        assert_eq!(
            headline("Nobody owns the migration, and no date was set", "x"),
            "Nobody owns the migration, and no…"
        );
        assert_eq!(
            headline("One two three four five, six seven", "x"),
            "One two three four five, six…"
        );
        assert_eq!(
            headline("One two three four five: six seven", "x"),
            "One two three four five: six…"
        );
        assert_eq!(
            headline("One two three four five six, seven", "x"),
            "One two three four five six…"
        );
    }

    #[test]
    fn text_without_spaces_is_cut_by_length() {
        let long = "数据集日期推迟了而且没有人确认新的交付时间".repeat(4);
        let h = headline(&long, "");
        assert_eq!(h.chars().count(), HEADLINE_WORDS * CHARS_PER_WORD + 1);
        assert!(h.ends_with('…'));
    }

    #[test]
    fn short_answers_allow_twelve_words() {
        let answer = "Yes: they agreed to host in Azure EU West, pending a legal review next week.";
        assert_eq!(
            fit("", answer, SHORT_ANSWER_WORDS),
            "Yes: they agreed to host in Azure EU West, pending a legal…"
        );
        assert_eq!(
            fit("Azure EU West.", answer, SHORT_ANSWER_WORDS),
            "Azure EU West."
        );
    }

    #[test]
    fn nothing_in_gives_nothing_out() {
        assert_eq!(headline("", "  "), "");
    }
}

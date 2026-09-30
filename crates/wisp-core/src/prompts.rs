//! Saved prompts: the built-in set, their scope, and filling a prompt's `{variables}`.
//!
//! A prompt is an instruction the user runs over one meeting's transcript. Its body may name
//! variables in braces ([`PROMPT_VARIABLES`]); [`fill_prompt`] replaces the known ones and leaves
//! anything else untouched, so a literal `{like this}` in a prompt survives. The library stores the
//! prompts; this module holds the rules, so every shell fills a prompt the same way.

/// Who a prompt is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptScope {
    /// The whole meeting.
    Meeting,
    /// One speaker, named when the prompt runs (the `{speaker}` variable).
    Speaker,
}

impl PromptScope {
    /// The stored name: `meeting` or `speaker`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Meeting => "meeting",
            Self::Speaker => "speaker",
        }
    }

    /// Parses a stored name. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "meeting" => Some(Self::Meeting),
            "speaker" => Some(Self::Speaker),
            _ => None,
        }
    }
}

/// A prompt that ships with the app. The user may edit it and reset it to this text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinPrompt {
    /// Stable id, so a reset finds the original after the user renames it.
    pub id: &'static str,
    pub name: &'static str,
    pub body: &'static str,
    pub scope: PromptScope,
}

/// Every variable a prompt body may use.
pub const PROMPT_VARIABLES: &[&str] = &["speaker", "title", "date", "about_me", "project"];

/// What the model gets for `{speaker}` in a meeting-wide prompt run without a chosen speaker.
pub const EVERYONE: &str = "everyone";

/// The prompts the app ships with.
pub const BUILTIN_PROMPTS: &[BuiltinPrompt] = &[
    BuiltinPrompt {
        id: "builtin-summary",
        name: "Summary",
        body: "Summarize the meeting \"{title}\" ({date}) for someone who missed it. Start with a \
               TL;DR of one or two sentences. Then list 3 to 7 highlights: the main topics, \
               decisions and outcomes, most important first. Use only what was said. Do not \
               invent names, numbers or facts. Use Markdown. Reply in the meeting's main language.",
        scope: PromptScope::Meeting,
    },
    BuiltinPrompt {
        id: "builtin-action-items",
        name: "Action items",
        body: "List the action items from this meeting as a Markdown checklist, one per line: \
               \"- [ ] action — owner · due\". The owner is the person who made the commitment, \
               or \"unassigned\". Add \"· due\" only when someone said a date. Include only real \
               commitments, not ideas. If there are none, reply \"No action items.\" Reply in the \
               meeting's main language.",
        scope: PromptScope::Meeting,
    },
    BuiltinPrompt {
        id: "builtin-follow-up-email",
        name: "Follow-up email draft",
        body: "Write the follow-up email after the meeting \"{title}\". Include a subject line, a \
               one-line opener, a short summary (2 to 4 bullets), the action items with owners, \
               and a short closing. Use only what was discussed. Write placeholders like [date] \
               instead of inventing details. Output only the email. Reply in the meeting's main \
               language.",
        scope: PromptScope::Meeting,
    },
    BuiltinPrompt {
        id: "builtin-meeting-critique",
        name: "Meeting critique",
        body: "Review this meeting for \"You\" (the person on the microphone). About me: \
               {about_me}\n\nWrite two sections in Markdown: \"What went well\" and \"What could \
               be better\". Cover the agenda, time use, decisions reached, and how the discussion \
               was run. Give each point a short quote or example from the transcript, and end \
               with 3 concrete things to do differently next time. Be direct and fair. Reply in \
               the meeting's main language.",
        scope: PromptScope::Meeting,
    },
    BuiltinPrompt {
        id: "builtin-communication-feedback",
        name: "Communication feedback",
        body: "Give communication feedback to {speaker}, based only on what {speaker} said in \
               this meeting. Cover, in Markdown sections: clarity (was the point easy to \
               follow?), listening (did they build on what others said?), questions asked \
               (how many, and how good), interruptions (did they cut others off, or get cut \
               off?), and concrete suggestions (3 to 5 specific changes, each with an example \
               from the transcript). Be specific, kind and honest. If {speaker} said little, say \
               so. Reply in the meeting's main language.",
        scope: PromptScope::Speaker,
    },
    BuiltinPrompt {
        id: "builtin-decisions",
        name: "Decisions & rationale",
        body: "List the decisions made in this meeting as a Markdown table with the columns \
               Decision | Owner | Rationale. Include only settled decisions, not open \
               questions. Give the reason when someone said one. If nothing was decided, reply \
               \"No decisions.\" Reply in the meeting's main language.",
        scope: PromptScope::Meeting,
    },
];

/// The built-in prompt with this id.
pub fn builtin_prompt(id: &str) -> Option<&'static BuiltinPrompt> {
    BUILTIN_PROMPTS.iter().find(|p| p.id == id)
}

/// Values for a prompt's variables. Empty fields fill in as empty text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptVars {
    /// The chosen speaker's label or name. Required for a [`PromptScope::Speaker`] prompt.
    pub speaker: Option<String>,
    pub title: String,
    pub date: String,
    pub about_me: String,
    /// The project's name and instructions.
    pub project: String,
}

/// Why a prompt could not be filled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// A speaker-scoped prompt was run without a speaker.
    #[error("choose a speaker for this prompt")]
    SpeakerRequired,
}

/// Fills `body`'s known `{variables}` from `vars`. Unknown ones stay as they are. A speaker-scoped
/// prompt needs a speaker; a meeting-wide one reads `{speaker}` as [`EVERYONE`] when none is chosen.
pub fn fill_prompt(
    body: &str,
    scope: PromptScope,
    vars: &PromptVars,
) -> Result<String, PromptError> {
    let speaker = vars
        .speaker
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let speaker = match (scope, speaker) {
        (PromptScope::Speaker, None) => return Err(PromptError::SpeakerRequired),
        (_, Some(s)) => s,
        (PromptScope::Meeting, None) => EVERYONE,
    };
    let value = |name: &str| -> Option<&str> {
        Some(match name {
            "speaker" => speaker,
            "title" => vars.title.trim(),
            "date" => vars.date.trim(),
            "about_me" => vars.about_me.trim(),
            "project" => vars.project.trim(),
            _ => return None,
        })
    };

    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').and_then(|close| {
            let name = &after[..close];
            value(name).map(|v| (v, close))
        }) {
            Some((v, close)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A UTC calendar date `YYYY-MM-DD` for epoch milliseconds, for `{date}` when the UI gives none.
pub fn utc_date(epoch_ms: i64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = epoch_ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> PromptVars {
        PromptVars {
            speaker: Some("Alice".into()),
            title: "Q4 plan".into(),
            date: "2026-09-30".into(),
            about_me: "PM".into(),
            project: "Acme".into(),
        }
    }

    #[test]
    fn known_variables_are_filled_and_unknown_ones_kept() {
        let out = fill_prompt(
            "{speaker} in {title} on {date} ({about_me}, {project}) {unknown} {speaker",
            PromptScope::Speaker,
            &vars(),
        )
        .unwrap();
        assert_eq!(
            out,
            "Alice in Q4 plan on 2026-09-30 (PM, Acme) {unknown} {speaker"
        );
        assert_eq!(
            fill_prompt("{ {} {{title}}", PromptScope::Meeting, &vars()).unwrap(),
            "{ {} {Q4 plan}"
        );
    }

    #[test]
    fn a_speaker_prompt_needs_a_speaker() {
        let none = PromptVars {
            speaker: Some("  ".into()),
            ..vars()
        };
        assert_eq!(
            fill_prompt("for {speaker}", PromptScope::Speaker, &none),
            Err(PromptError::SpeakerRequired)
        );
        assert_eq!(
            fill_prompt("for {speaker}", PromptScope::Meeting, &none).unwrap(),
            "for everyone"
        );
    }

    #[test]
    fn empty_values_fill_as_empty_text() {
        let out =
            fill_prompt("[{about_me}]", PromptScope::Meeting, &PromptVars::default()).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn builtins_have_unique_ids_and_use_only_known_variables() {
        for (i, p) in BUILTIN_PROMPTS.iter().enumerate() {
            assert!(BUILTIN_PROMPTS[i + 1..].iter().all(|q| q.id != p.id));
            assert_eq!(builtin_prompt(p.id), Some(p));
            let filled = fill_prompt(p.body, p.scope, &vars()).unwrap();
            assert!(!filled.contains('{'), "{}: {filled}", p.id);
        }
        let feedback = builtin_prompt("builtin-communication-feedback").unwrap();
        assert_eq!(feedback.scope, PromptScope::Speaker);
    }

    #[test]
    fn scopes_round_trip() {
        for s in [PromptScope::Meeting, PromptScope::Speaker] {
            assert_eq!(PromptScope::parse(s.as_str()), Some(s));
        }
        assert_eq!(PromptScope::parse("team"), None);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_790_000_000_000), "2026-09-21");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(-86_400_000), "1969-12-31");
    }
}

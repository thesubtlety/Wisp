//! Meeting types: what kind of meeting this is, and how live intelligence should behave in it.
//!
//! A type tunes the live passes (how often they run, what to watch for, which cards are worth an
//! interruption), the wrap-up checklist, the summary's extra sections, and which saved prompts to
//! offer first afterwards. The library stores the types; this module holds the built-in set and the
//! names of the knobs, so every shell reads them the same way.

/// How often live passes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Small batches, soon: for meetings where the moment to ask passes quickly (interviews).
    Fast,
    /// The default.
    Normal,
    /// Larger batches, less often: for long discussions where little changes minute to minute.
    Calm,
}

impl Cadence {
    pub const ALL: [Cadence; 3] = [Cadence::Fast, Cadence::Normal, Cadence::Calm];

    /// The stored name: `fast`, `normal` or `calm`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Normal => "normal",
            Self::Calm => "calm",
        }
    }

    /// Parses a stored name. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Which live cards a meeting type favours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStyle {
    /// Questions to ask now: a lower bar, a shorter cooldown, cards with a question preferred.
    Questions,
    /// Gaps: missing owners, dates and requirements (the default bar).
    Gaps,
    /// Risks and conflicts preferred.
    Risks,
    /// No preference (the default bar).
    Balanced,
}

impl CardStyle {
    pub const ALL: [CardStyle; 4] = [
        CardStyle::Questions,
        CardStyle::Gaps,
        CardStyle::Risks,
        CardStyle::Balanced,
    ];

    /// The stored name: `questions`, `gaps`, `risks` or `balanced`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Questions => "questions",
            Self::Gaps => "gaps",
            Self::Risks => "risks",
            Self::Balanced => "balanced",
        }
    }

    /// Parses a stored name. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// A meeting type that ships with the app. The user may edit it and reset it to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinMeetingType {
    /// Stable id, so a reset finds the original after the user renames it.
    pub id: &'static str,
    pub name: &'static str,
    /// One line on when to use it; also what inference reads to tell the types apart.
    pub description: &'static str,
    /// What the live passes should watch for, added to their context. Empty adds nothing.
    pub watch_for: &'static str,
    pub card_style: CardStyle,
    pub cadence: Cadence,
    /// What the wrap-up audit checks before the meeting ends.
    pub wrap_checklist: &'static [&'static str],
    /// Extra sections the summary fills, in order.
    pub summary_sections: &'static [&'static str],
    /// Saved prompts (by name) offered first after the meeting.
    pub suggested_prompts: &'static [&'static str],
}

/// The type used when nothing else is chosen: today's behaviour, unchanged.
pub const GENERAL_TYPE_ID: &str = "builtin-general";

/// The meeting types the app ships with, General first.
pub const BUILTIN_MEETING_TYPES: &[BuiltinMeetingType] = &[
    BuiltinMeetingType {
        id: GENERAL_TYPE_ID,
        name: "General",
        description: "Any meeting. The default behaviour.",
        watch_for: "",
        card_style: CardStyle::Balanced,
        cadence: Cadence::Normal,
        wrap_checklist: &[],
        summary_sections: &[],
        suggested_prompts: &[],
    },
    BuiltinMeetingType {
        id: "builtin-interview",
        name: "Interview",
        description: "A job interview: one side asks about the other's experience and skills.",
        watch_for: "- Answers that are vague, generic or unsupported by a concrete example: \
                    suggest a follow-up question that asks for specifics (what they did, how, \
                    the result).\n\
                    - Claims that contradict something the candidate said earlier.\n\
                    - Core topics for the role not covered yet.\n\
                    - Good moments to dig deeper into something the candidate only touched on.\n\
                    Prefer short, ready-to-ask questions over observations.",
        card_style: CardStyle::Questions,
        cadence: Cadence::Fast,
        wrap_checklist: &[
            "Core topics for the role covered",
            "Candidate's questions answered",
            "Next steps and timeline stated",
        ],
        summary_sections: &["Strengths", "Concerns", "Evidence", "Recommendation"],
        suggested_prompts: &["Interview scorecard", "Communication feedback"],
    },
    BuiltinMeetingType {
        id: "builtin-project-scoping",
        name: "Project scoping",
        description: "Defining a project: requirements, constraints, scope and who does what.",
        watch_for: "- Requirements and constraints, and when they are ambiguous or untestable.\n\
                    - Statements that conflict with each other or with the project documents.\n\
                    - Assumptions the plan relies on that nobody confirmed.\n\
                    - Work with no owner or no date.\n\
                    - Scope that grew or changed without agreement.",
        card_style: CardStyle::Gaps,
        cadence: Cadence::Normal,
        wrap_checklist: &[
            "Scope and out-of-scope agreed",
            "Every requirement has an owner",
            "Open questions have an owner and a date",
            "Next milestone and date stated",
        ],
        summary_sections: &["Scope", "Requirements", "Constraints", "Open questions"],
        suggested_prompts: &["Action items", "Decisions & rationale"],
    },
    BuiltinMeetingType {
        id: "builtin-engineering",
        name: "Engineering",
        description: "A technical discussion: design, architecture, trade-offs, implementation.",
        watch_for: "- Decisions, and the reason given for each.\n\
                    - Trade-offs weighed, and the options rejected.\n\
                    - Technical risks: performance, security, migration, dependencies.\n\
                    - Open technical questions nobody owns.\n\
                    - Action items, with owner and date when said.",
        card_style: CardStyle::Risks,
        cadence: Cadence::Calm,
        wrap_checklist: &[
            "Decisions recorded with their rationale",
            "Risks have an owner or a mitigation",
            "Open questions have an owner",
        ],
        summary_sections: &["Decisions and rationale", "Trade-offs", "Risks"],
        suggested_prompts: &["Decisions & rationale", "Action items"],
    },
    BuiltinMeetingType {
        id: "builtin-one-on-one",
        name: "1:1",
        description: "A one-on-one between a manager and a report, or two peers.",
        watch_for: "- Feedback given or asked for, in either direction.\n\
                    - Blockers, and whether anyone offered to remove them.\n\
                    - Commitments each person made.\n\
                    - Career, growth and wellbeing topics raised.\n\
                    Be sparing: interrupt only for a commitment or a blocker that would be lost.",
        card_style: CardStyle::Balanced,
        cadence: Cadence::Calm,
        wrap_checklist: &[
            "Blockers have a next step",
            "Commitments have an owner and a date",
        ],
        summary_sections: &["Feedback", "Blockers", "Career and growth"],
        suggested_prompts: &["Action items", "Communication feedback"],
    },
    BuiltinMeetingType {
        id: "builtin-sales-discovery",
        name: "Sales discovery",
        description: "A first sales call: learning the prospect's needs, budget and process.",
        watch_for: "- The prospect's pain: the problem, its cost, and who feels it.\n\
                    - Budget, decision authority and the buying process.\n\
                    - Timeline and what drives it.\n\
                    - Objections, and whether they were answered.\n\
                    - Gaps in that picture: suggest the question that fills one.",
        card_style: CardStyle::Questions,
        cadence: Cadence::Normal,
        wrap_checklist: &[
            "Pain and its impact understood",
            "Budget discussed",
            "Decision maker and process identified",
            "Timeline stated",
            "Next step agreed with a date",
        ],
        summary_sections: &["Pain", "Budget", "Authority", "Timeline", "Objections"],
        suggested_prompts: &["Follow-up email draft", "Action items"],
    },
    BuiltinMeetingType {
        id: "builtin-incident-review",
        name: "Incident review",
        description: "A post-incident review: what happened, why, and what changes.",
        watch_for: "- The timeline: detection, response and resolution, with times.\n\
                    - Impact: who and what was affected, for how long.\n\
                    - The root cause and the contributing factors, kept apart.\n\
                    - Claims about the timeline or cause that conflict.\n\
                    - Action items, each with an owner and a date.",
        card_style: CardStyle::Gaps,
        cadence: Cadence::Normal,
        wrap_checklist: &[
            "Timeline agreed",
            "Impact stated",
            "Root cause identified",
            "Every action item has an owner and a date",
        ],
        summary_sections: &[
            "Timeline",
            "Impact",
            "Root cause",
            "Contributing factors",
            "Action items",
        ],
        suggested_prompts: &["Action items", "Follow-up email draft"],
    },
];

/// The built-in meeting type with this id.
pub fn builtin_meeting_type(id: &str) -> Option<&'static BuiltinMeetingType> {
    BUILTIN_MEETING_TYPES.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::BUILTIN_PROMPTS;

    #[test]
    fn names_round_trip() {
        for c in Cadence::ALL {
            assert_eq!(Cadence::parse(c.as_str()), Some(c));
        }
        for s in CardStyle::ALL {
            assert_eq!(CardStyle::parse(s.as_str()), Some(s));
        }
        assert_eq!(Cadence::parse("quick"), None);
        assert_eq!(CardStyle::parse(""), None);
    }

    #[test]
    fn general_is_first_and_changes_nothing() {
        let general = &BUILTIN_MEETING_TYPES[0];
        assert_eq!(general.id, GENERAL_TYPE_ID);
        assert!(general.watch_for.is_empty());
        assert_eq!(general.cadence, Cadence::Normal);
        assert_eq!(general.card_style, CardStyle::Balanced);
        assert!(general.wrap_checklist.is_empty() && general.summary_sections.is_empty());
    }

    #[test]
    fn builtins_have_unique_ids_and_suggest_only_builtin_prompts() {
        let mut ids: Vec<&str> = BUILTIN_MEETING_TYPES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), BUILTIN_MEETING_TYPES.len());
        for t in BUILTIN_MEETING_TYPES {
            assert!(builtin_meeting_type(t.id).is_some());
            for name in t.suggested_prompts {
                assert!(
                    BUILTIN_PROMPTS.iter().any(|p| p.name == *name),
                    "{} suggests unknown prompt {name}",
                    t.name
                );
            }
        }
        let interview = builtin_meeting_type("builtin-interview").unwrap();
        assert_eq!(interview.cadence, Cadence::Fast);
        assert_eq!(interview.card_style, CardStyle::Questions);
    }
}

//! The project brief: one Markdown page on where a project stands, built on demand.
//!
//! [`project_brief`] takes the project's instructions, its accepted knowledge and the saved state
//! of each of its meetings, and lists what is still live: open commitments, open questions, recent
//! decisions and active risks. The same text said in two meetings is listed once, from the newest.
//! It reads nothing and formats no dates itself; the caller passes display dates in.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use wisp_library::MemoryEntry;

use crate::model::{ItemKind, Lifecycle, MeetingState, StateItem};

/// Decisions from meetings this recent are listed.
pub const RECENT_DECISION_DAYS: i64 = 30;
/// Decisions from this many of the newest meetings are listed, however old.
pub const RECENT_DECISION_MEETINGS: usize = 3;

const DAY_MS: i64 = 86_400_000;

/// One saved meeting of the project.
#[derive(Debug, Clone)]
pub struct BriefMeeting {
    pub id: String,
    pub title: String,
    /// When it happened, already formatted for display.
    pub when: String,
    pub started_at_ms: i64,
    pub state: MeetingState,
}

/// Everything the brief is built from.
#[derive(Debug, Clone)]
pub struct BriefInput<'a> {
    pub project: &'a str,
    /// Today, formatted for display.
    pub date: &'a str,
    pub now_ms: i64,
    pub instructions: &'a str,
    pub memory: &'a [MemoryEntry],
    /// The project's meetings, in any order.
    pub meetings: &'a [BriefMeeting],
}

/// `YYYY-MM-DD` for epoch ms `ms`, shifted by `offset_minutes` east of UTC (so local dates match
/// the user's calendar).
pub fn iso_date(ms: i64, offset_minutes: i32) -> String {
    let days = (ms + i64::from(offset_minutes) * 60_000).div_euclid(DAY_MS);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Knowledge kinds in the order the brief lists them, with their headings.
const KNOWLEDGE_HEADINGS: [(&str, &str); 5] = [
    ("fact", "Facts"),
    ("requirement", "Requirements"),
    ("decision", "Decisions"),
    ("person", "People"),
    ("open_issue", "Open issues"),
];

/// The brief as Markdown.
pub fn project_brief(input: &BriefInput) -> String {
    let mut meetings: Vec<&BriefMeeting> = input.meetings.iter().collect();
    meetings.sort_by_key(|m| std::cmp::Reverse(m.started_at_ms));

    let mut out = format!("# Project brief: {}\n", input.project.trim());
    let _ = writeln!(
        out,
        "\n_{} · {} meeting{}_",
        input.date,
        meetings.len(),
        if meetings.len() == 1 { "" } else { "s" }
    );
    let instructions = input.instructions.trim();
    if !instructions.is_empty() {
        let _ = writeln!(out, "\n## Instructions\n\n{instructions}");
    }
    knowledge(&mut out, input.memory, &meetings);

    let open = |i: &StateItem| matches!(i.lifecycle, Lifecycle::Active | Lifecycle::Uncertain);
    let recent_from = input.now_ms - RECENT_DECISION_DAYS * DAY_MS;
    let recent: HashSet<&str> = meetings
        .iter()
        .enumerate()
        .filter(|(n, m)| *n < RECENT_DECISION_MEETINGS || m.started_at_ms >= recent_from)
        .map(|(_, m)| m.id.as_str())
        .collect();
    items(&mut out, "Open commitments", &meetings, |_, i| {
        i.kind == ItemKind::Commitment && open(i)
    });
    items(&mut out, "Open questions", &meetings, |_, i| {
        i.kind == ItemKind::OpenQuestion && open(i)
    });
    items(
        &mut out,
        &format!(
            "Recent decisions (last {RECENT_DECISION_DAYS} days or {RECENT_DECISION_MEETINGS} meetings)"
        ),
        &meetings,
        |m, i| i.kind == ItemKind::Decision && recent.contains(m.id.as_str()),
    );
    items(&mut out, "Active risks", &meetings, |_, i| {
        i.kind == ItemKind::Risk && open(i)
    });
    out
}

/// The project's knowledge by kind, each entry with the meeting it came from.
fn knowledge(out: &mut String, memory: &[MemoryEntry], meetings: &[&BriefMeeting]) {
    let _ = writeln!(out, "\n## Project knowledge");
    if memory.is_empty() {
        out.push_str("\n- (none)\n");
        return;
    }
    let by_id: HashMap<&str, &BriefMeeting> =
        meetings.iter().map(|m| (m.id.as_str(), *m)).collect();
    let known: Vec<&str> = KNOWLEDGE_HEADINGS.iter().map(|(k, _)| *k).collect();
    let mut groups: Vec<(String, &str)> = KNOWLEDGE_HEADINGS
        .iter()
        .map(|(k, h)| ((*h).to_owned(), *k))
        .collect();
    for entry in memory {
        if !known.contains(&entry.kind.as_str()) && !groups.iter().any(|(_, k)| *k == entry.kind) {
            groups.push((heading_for(&entry.kind), entry.kind.as_str()));
        }
    }
    for (heading, kind) in groups {
        let entries: Vec<&MemoryEntry> = memory.iter().filter(|e| e.kind == kind).collect();
        if entries.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n### {heading}\n");
        for e in entries {
            let source = e
                .meeting_id
                .as_deref()
                .and_then(|id| by_id.get(id))
                .map(|m| format!("{}, {}", m.title, m.when))
                .or_else(|| e.provenance.first().map(|p| p.label.clone()));
            match source {
                Some(s) => {
                    let _ = writeln!(out, "- {} _({s})_", e.text);
                }
                None => {
                    let _ = writeln!(out, "- {}", e.text);
                }
            }
        }
    }
}

/// `open_issue` → `Open issue`.
fn heading_for(kind: &str) -> String {
    let words = kind.replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Lowercased, with runs of whitespace and trailing punctuation folded, for deduplication.
fn dedup_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!', '?', '。', '！', '？'])
        .to_lowercase()
}

/// One section of live items across meetings, newest meeting first, each text once.
fn items(
    out: &mut String,
    heading: &str,
    meetings: &[&BriefMeeting],
    keep: impl Fn(&BriefMeeting, &StateItem) -> bool,
) {
    let _ = writeln!(out, "\n## {heading}\n");
    let mut seen = HashSet::new();
    let mut any = false;
    for m in meetings {
        for item in m.state.live_items() {
            if !keep(m, item) || !seen.insert(dedup_key(&item.text)) {
                continue;
            }
            any = true;
            let _ = writeln!(out, "{}", line(item, m));
        }
    }
    if !any {
        out.push_str("- (none)\n");
    }
}

fn line(item: &StateItem, meeting: &BriefMeeting) -> String {
    let mut out = format!("- {}", item.text.trim());
    let mut extra = Vec::new();
    if let Some(o) = item.owner.as_deref().filter(|o| !o.trim().is_empty()) {
        extra.push(format!("owner: {o}"));
    }
    if let Some(d) = item.due.as_deref().filter(|d| !d.trim().is_empty()) {
        extra.push(format!("due: {d}"));
    }
    if item.lifecycle != Lifecycle::Active {
        extra.push(item.lifecycle.as_str().to_owned());
    }
    if !extra.is_empty() {
        let _ = write!(out, " ({})", extra.join("; "));
    }
    let _ = write!(out, " _({}, {})_", meeting.title, meeting.when);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EpistemicStatus;
    use wisp_library::ProvenanceRef;

    fn item(id: &str, kind: ItemKind, text: &str, lifecycle: Lifecycle) -> StateItem {
        StateItem {
            id: id.into(),
            kind,
            text: text.into(),
            status: EpistemicStatus::Stated,
            confidence: 0.9,
            lifecycle,
            source_refs: Vec::new(),
            related_items: Vec::new(),
            owner: None,
            due: None,
            supersedes: None,
            superseded_by: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            revision: 1,
        }
    }

    fn meeting(id: &str, title: &str, day: i64, items: Vec<StateItem>) -> BriefMeeting {
        let mut state = MeetingState::new(id);
        for i in items {
            state.items.insert(i.id.clone(), i);
        }
        BriefMeeting {
            id: id.into(),
            title: title.into(),
            when: format!("day {day}"),
            started_at_ms: day * DAY_MS,
            state,
        }
    }

    fn memory(kind: &str, text: &str, meeting_id: Option<&str>, label: &str) -> MemoryEntry {
        MemoryEntry {
            id: 0,
            project_id: "p".into(),
            kind: kind.into(),
            text: text.into(),
            status: "stated".into(),
            confidence: 0.9,
            provenance: vec![ProvenanceRef {
                source_ref: "Mx:T0".into(),
                label: label.into(),
                sha256: String::new(),
            }],
            meeting_id: meeting_id.map(Into::into),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn brief(meetings: &[BriefMeeting], memory: &[MemoryEntry], instructions: &str) -> String {
        project_brief(&BriefInput {
            project: "Acme",
            date: "day 100",
            now_ms: 100 * DAY_MS,
            instructions,
            memory,
            meetings,
        })
    }

    fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
        let start = doc.find(&format!("## {heading}")).expect(heading);
        let rest = &doc[start + 3..];
        &rest[..rest.find("\n## ").unwrap_or(rest.len())]
    }

    #[test]
    fn dates_follow_the_local_calendar() {
        assert_eq!(iso_date(0, 0), "1970-01-01");
        assert_eq!(iso_date(-1, 0), "1969-12-31");
        // 2026-09-30 23:30 UTC is already Oct 1 in Shanghai and still Sept 30 in New York.
        let ms = 1_790_811_000_000;
        assert_eq!(iso_date(ms, 0), "2026-09-30");
        assert_eq!(iso_date(ms, 8 * 60), "2026-10-01");
        assert_eq!(iso_date(ms, -4 * 60), "2026-09-30");
        assert_eq!(iso_date(951_782_400_000, 0), "2000-02-29");
    }

    #[test]
    fn the_header_instructions_and_knowledge_come_first() {
        let meetings = [meeting("m1", "Kickoff", 90, vec![])];
        let memory = [
            memory("requirement", "EU data only", Some("m1"), "Meeting Sept 1"),
            memory(
                "fact",
                "Runs in Azure",
                Some("gone"),
                "Meeting Aug 3, 10:00",
            ),
            memory("fact", "Sarah owns security", Some("m1"), "Meeting Sept 1"),
            memory("glossary", "SoW means statement of work", None, "notes.md"),
        ];
        let doc = brief(&meetings, &memory, "  Watch the budget.  ");
        assert!(doc.starts_with("# Project brief: Acme\n\n_day 100 · 1 meeting_\n"));
        assert!(doc.contains("## Instructions\n\nWatch the budget.\n"));
        let k = section(&doc, "Project knowledge");
        let facts = k.find("### Facts").unwrap();
        let reqs = k.find("### Requirements").unwrap();
        let other = k.find("### Glossary").unwrap();
        assert!(
            facts < reqs && reqs < other,
            "known kinds in order, others last"
        );
        assert!(k.contains("- Sarah owns security _(Kickoff, day 90)_"));
        assert!(
            k.contains("- Runs in Azure _(Meeting Aug 3, 10:00)_"),
            "a meeting no longer here falls back to the provenance label"
        );
        assert!(k.contains("- SoW means statement of work _(notes.md)_"));
    }

    #[test]
    fn only_live_items_are_listed_once_newest_first() {
        let mut owned = item(
            "COM-1",
            ItemKind::Commitment,
            "Send the SOC 2 report",
            Lifecycle::Active,
        );
        owned.owner = Some("Sarah".into());
        owned.due = Some("Friday".into());
        let old = meeting(
            "m1",
            "Kickoff",
            10,
            vec![
                item(
                    "COM-1",
                    ItemKind::Commitment,
                    "send the SOC 2  report.",
                    Lifecycle::Active,
                ),
                item(
                    "COM-2",
                    ItemKind::Commitment,
                    "Book the venue",
                    Lifecycle::Resolved,
                ),
                item(
                    "COM-3",
                    ItemKind::Commitment,
                    "Old plan",
                    Lifecycle::Superseded,
                ),
                item(
                    "Q-1",
                    ItemKind::OpenQuestion,
                    "Who signs off?",
                    Lifecycle::Uncertain,
                ),
                item(
                    "RISK-1",
                    ItemKind::Risk,
                    "Vendor lock-in",
                    Lifecycle::Active,
                ),
                item(
                    "RISK-2",
                    ItemKind::Risk,
                    "Missed deadline",
                    Lifecycle::Resolved,
                ),
                item(
                    "DEC-1",
                    ItemKind::Decision,
                    "Use Postgres",
                    Lifecycle::Active,
                ),
            ],
        );
        let newer = meeting("m2", "Design review", 95, vec![owned]);
        let doc = brief(&[old, newer], &[], "");
        assert!(!doc.contains("## Instructions"), "no empty instructions");
        assert!(section(&doc, "Project knowledge").contains("- (none)"));

        let com = section(&doc, "Open commitments");
        assert!(com.contains(
            "- Send the SOC 2 report (owner: Sarah; due: Friday) _(Design review, day 95)_"
        ));
        assert_eq!(com.matches("SOC 2").count(), 1, "deduped by text");
        assert!(!com.contains("Book the venue") && !com.contains("Old plan"));

        assert!(section(&doc, "Open questions")
            .contains("- Who signs off? (uncertain) _(Kickoff, day 10)_"));
        let risks = section(&doc, "Active risks");
        assert!(risks.contains("Vendor lock-in") && !risks.contains("Missed deadline"));
        assert!(
            section(&doc, "Recent decisions").contains("Use Postgres"),
            "old, but one of the last meetings"
        );
    }

    #[test]
    fn recent_decisions_are_from_the_last_days_or_meetings() {
        let dec = |id: &str, text: &str| item(id, ItemKind::Decision, text, Lifecycle::Active);
        let meetings: Vec<BriefMeeting> = [
            ("m1", 1, "Ancient"),
            ("m2", 2, "Old"),
            ("m3", 3, "Older"),
            ("m4", 4, "Oldish"),
            ("m5", 80, "This month"),
            ("m6", 90, "Also this month"),
        ]
        .into_iter()
        .map(|(id, day, text)| meeting(id, id, day, vec![dec("DEC-1", text)]))
        .collect();
        let doc = brief(&meetings, &[], "");
        let d = section(&doc, "Recent decisions");
        for kept in ["This month", "Also this month", "Oldish"] {
            assert!(d.contains(kept), "{kept}");
        }
        for dropped in ["Ancient", "- Old ", "Older"] {
            assert!(!d.contains(dropped), "{dropped}");
        }
        assert!(section(&doc, "Open commitments").contains("- (none)"));
    }
}

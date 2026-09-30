//! The project brief: one Markdown page on where a project stands, built on demand.
//!
//! [`project_brief`] takes the project's instructions, its accepted knowledge, the items the user
//! added by hand and the saved state of each of its meetings, and lists what is still live: open
//! commitments, open questions, recent decisions and active risks. [`project_overview`] is the same
//! selection as data, for the Projects view. The same text said in two meetings is listed once, as
//! the newest meeting has it (so an item marked done there hides older copies). It reads nothing and
//! formats no dates itself; the caller passes display dates in.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use serde::Serialize;
use wisp_library::{MemoryEntry, ProjectItem, ProvenanceRef};

use crate::edit::parse_name;
use crate::model::{EpistemicStatus, ItemKind, Lifecycle, MeetingState, StateItem};

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
    /// Each item's text before the user's hand edits, by item id. Duplicates across meetings are
    /// matched on this, so rewording one copy doesn't bring back the older one. Empty when there
    /// were no edits.
    pub original_text: std::collections::HashMap<String, String>,
    /// The meeting's saved summary (Markdown), if one was made.
    pub summary: Option<String>,
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
    /// Items the user added to the project by hand.
    pub manual: &'a [ProjectItem],
}

/// The meeting an overview item comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemMeeting {
    pub id: String,
    pub title: String,
    pub when: String,
    pub started_at_ms: i64,
    /// The item's id in that meeting's state (`COM-3`).
    pub item_id: String,
}

/// One item in the project overview: from a meeting's state, or added by hand.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewItem {
    pub kind: ItemKind,
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub lifecycle: Lifecycle,
    /// Where it was said; `None` for an item the user added.
    pub meeting: Option<ItemMeeting>,
    /// The project item's id, for an item the user added.
    pub manual_id: Option<String>,
}

/// What is live in a project, by section. Hand-added items come first in each section, then
/// meeting items, newest meeting first.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOverview {
    pub commitments: Vec<OverviewItem>,
    pub open_questions: Vec<OverviewItem>,
    /// Decisions from recent meetings (see [`RECENT_DECISION_DAYS`]) and every hand-added one.
    pub decisions: Vec<OverviewItem>,
    pub risks: Vec<OverviewItem>,
    /// Commitments, questions and risks marked resolved, so they can be reopened.
    pub done: Vec<OverviewItem>,
    /// Each meeting's gist, newest first.
    pub meetings: Vec<MeetingGist>,
    /// Requirements, constraints, decisions and facts said in meetings that aren't project
    /// knowledge yet (matched on text), newest meeting first.
    pub from_meetings: Vec<KnowledgeCandidate>,
}

/// A meeting's summary in one line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingGist {
    pub id: String,
    /// The summary's TL;DR (see [`summary_tldr`]); `None` when there is no summary.
    pub tldr: Option<String>,
    /// What it recorded, as [`item_counts`] says it.
    pub counts: String,
}

/// A meeting item that could become project knowledge.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeCandidate {
    #[serde(flatten)]
    pub item: OverviewItem,
    /// The knowledge kind it would be kept as (`requirement`, `decision`, `fact`).
    pub knowledge_kind: String,
    /// `stated` or `inferred`.
    pub status: String,
    pub confidence: f64,
    /// Its evidence refs in the meeting, labelled with the meeting.
    pub provenance: Vec<ProvenanceRef>,
}

/// Most items listed as done.
pub const MAX_DONE: usize = 50;

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

/// `1 meeting`, `2 meetings`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The brief as Markdown: GitHub-flavoured, meant to be pasted anywhere.
pub fn project_brief(input: &BriefInput) -> String {
    let mut meetings: Vec<&BriefMeeting> = input.meetings.iter().collect();
    meetings.sort_by_key(|m| std::cmp::Reverse(m.started_at_ms));
    let overview = project_overview(input);

    let mut out = format!("# {} — project brief\n", input.project.trim());
    let _ = writeln!(
        out,
        "\n_Updated {} · {} · {} · {}_",
        input.date,
        count(meetings.len(), "meeting", "meetings"),
        count(
            overview.commitments.len(),
            "open commitment",
            "open commitments"
        ),
        count(
            overview.open_questions.len(),
            "open question",
            "open questions"
        ),
    );
    let instructions = input.instructions.trim();
    if !instructions.is_empty() {
        let _ = writeln!(out, "\n## Instructions\n");
        for l in instructions.lines() {
            let l = l.trim_end();
            let _ = writeln!(
                out,
                "{}",
                if l.is_empty() {
                    ">".to_owned()
                } else {
                    format!("> {l}")
                }
            );
        }
    }
    items(&mut out, "Open commitments", &overview.commitments, true);
    items(&mut out, "Open questions", &overview.open_questions, true);
    items(&mut out, "Recent decisions", &overview.decisions, false);
    items(&mut out, "Active risks", &overview.risks, false);
    meeting_list(&mut out, &meetings);
    knowledge(&mut out, input.memory, &meetings);
    out
}

/// One section of items. An empty one is left out, or says so when `always`.
fn items(out: &mut String, heading: &str, items: &[OverviewItem], always: bool) {
    if items.is_empty() && !always {
        return;
    }
    let _ = writeln!(out, "\n## {heading} ({})\n", items.len());
    if items.is_empty() {
        out.push_str("Nothing open.\n");
    }
    for item in items {
        let _ = writeln!(out, "{}", line(item));
    }
}

/// `- **Owner** — text · due X · _Meeting, date_`, leaving out what is empty.
fn line(item: &OverviewItem) -> String {
    let text = one_line(&item.text);
    let mut parts = vec![match item
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        Some(o) => format!("**{o}** — {text}"),
        None => text,
    }];
    if let Some(d) = item.due.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        parts.push(format!("due {d}"));
    }
    if item.lifecycle != Lifecycle::Active {
        parts.push(item.lifecycle.as_str().to_owned());
    }
    parts.push(match &item.meeting {
        Some(m) => format!("_{}, {}_", one_line(&m.title), m.when),
        None => "_Added by you_".to_owned(),
    });
    format!("- {}", parts.join(" · "))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The meetings, newest first, each with its TL;DR and what it recorded.
fn meeting_list(out: &mut String, meetings: &[&BriefMeeting]) {
    if meetings.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Meetings ({})", meetings.len());
    for m in meetings {
        let _ = writeln!(out, "\n### {} — {}\n", one_line(&m.title), m.when);
        match m.summary.as_deref().and_then(summary_tldr) {
            Some(tldr) => {
                let _ = writeln!(out, "{tldr}");
            }
            None => out.push_str("_No summary yet._\n"),
        }
        let counts = item_counts(&m.state);
        if !counts.is_empty() {
            let _ = writeln!(out, "\n{counts}");
        }
    }
}

/// `3 decisions · 2 commitments · 1 open question`: a meeting's live items by kind, zeros left out.
pub fn item_counts(state: &MeetingState) -> String {
    let live = state.live_items();
    let of = |k: ItemKind| live.iter().filter(|i| i.kind == k).count();
    [
        (ItemKind::Decision, "decision", "decisions"),
        (ItemKind::Commitment, "commitment", "commitments"),
        (ItemKind::OpenQuestion, "open question", "open questions"),
        (ItemKind::Risk, "risk", "risks"),
    ]
    .into_iter()
    .map(|(k, one, many)| (of(k), one, many))
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| count(n, one, many))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The one-line gist of a meeting summary: its TL;DR (`**TL;DR:** …` or a `TL;DR` heading's
/// paragraph), else its first paragraph (or first bullet). `None` when there is no text.
pub fn summary_tldr(summary: &str) -> Option<String> {
    let lines: Vec<&str> = summary.lines().map(str::trim).collect();
    let is_heading = |l: &str| l.starts_with('#');
    let tldr_label = |l: &str| {
        let bare = l.trim_start_matches('#').trim().replace("**", "");
        let lower = bare.to_lowercase();
        lower.starts_with("tl;dr").then(|| {
            bare.get("tl;dr".len()..)
                .unwrap_or_default()
                .trim_start_matches([':', ' '])
                .trim()
                .to_owned()
        })
    };
    // A paragraph from line `n` on: consecutive plain lines, or the first bullet.
    let paragraph = |n: usize| -> Option<String> {
        let first = lines.get(n)?;
        if let Some(b) = bullet(first) {
            return Some(one_line(b)).filter(|s| !s.is_empty());
        }
        let text: Vec<&str> = lines[n..]
            .iter()
            .take_while(|l| !l.is_empty() && !is_heading(l) && bullet(l).is_none())
            .copied()
            .collect();
        Some(one_line(&text.join(" "))).filter(|s| !s.is_empty())
    };
    for (n, l) in lines.iter().enumerate() {
        let Some(rest) = tldr_label(l) else { continue };
        if !rest.is_empty() {
            return Some(one_line(&rest));
        }
        let next = (n + 1..lines.len()).find(|&i| !lines[i].is_empty())?;
        if !is_heading(lines[next]) {
            return paragraph(next);
        }
    }
    let first = lines.iter().position(|l| !l.is_empty() && !is_heading(l))?;
    paragraph(first)
}

/// A bullet line's text.
fn bullet(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .map(str::trim)
}

/// The project's knowledge by kind, each entry with the meeting it came from.
fn knowledge(out: &mut String, memory: &[MemoryEntry], meetings: &[&BriefMeeting]) {
    if memory.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Knowledge ({})", memory.len());
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
                .map(|m| format!("{}, {}", one_line(&m.title), m.when))
                .or_else(|| e.provenance.first().map(|p| p.label.clone()));
            match source {
                Some(s) => {
                    let _ = writeln!(out, "- {} · _{s}_", one_line(&e.text));
                }
                None => {
                    let _ = writeln!(out, "- {}", one_line(&e.text));
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

fn is_open(l: Lifecycle) -> bool {
    matches!(l, Lifecycle::Active | Lifecycle::Uncertain)
}

/// The overview: each section's hand-added items, then its meeting items.
pub fn project_overview(input: &BriefInput) -> ProjectOverview {
    let mut meetings: Vec<&BriefMeeting> = input.meetings.iter().collect();
    meetings.sort_by_key(|m| std::cmp::Reverse(m.started_at_ms));
    let recent_from = input.now_ms - RECENT_DECISION_DAYS * DAY_MS;
    let recent: HashSet<&str> = meetings
        .iter()
        .enumerate()
        .filter(|(n, m)| *n < RECENT_DECISION_MEETINGS || m.started_at_ms >= recent_from)
        .map(|(_, m)| m.id.as_str())
        .collect();
    let manual: Vec<OverviewItem> = input.manual.iter().filter_map(manual_item).collect();
    let section = |kind: ItemKind, keep: &dyn Fn(&BriefMeeting, &StateItem) -> bool| {
        let mut out: Vec<OverviewItem> = manual
            .iter()
            .filter(|i| i.kind == kind && is_open(i.lifecycle))
            .cloned()
            .collect();
        out.extend(meeting_items(&meetings, &[kind], keep));
        out
    };
    let mut decisions: Vec<OverviewItem> = manual
        .iter()
        .filter(|i| i.kind == ItemKind::Decision)
        .cloned()
        .collect();
    decisions.extend(meeting_items(&meetings, &[ItemKind::Decision], &|m, _| {
        recent.contains(m.id.as_str())
    }));
    let done_kinds = [ItemKind::Commitment, ItemKind::OpenQuestion, ItemKind::Risk];
    let mut done: Vec<OverviewItem> = manual
        .iter()
        .filter(|i| done_kinds.contains(&i.kind) && i.lifecycle == Lifecycle::Resolved)
        .cloned()
        .collect();
    for kind in done_kinds {
        done.extend(meeting_items(&meetings, &[kind], &|_, i| {
            i.lifecycle == Lifecycle::Resolved
        }));
    }
    done.truncate(MAX_DONE);
    let gists = meetings
        .iter()
        .map(|m| MeetingGist {
            id: m.id.clone(),
            tldr: m.summary.as_deref().and_then(summary_tldr),
            counts: item_counts(&m.state),
        })
        .collect();
    ProjectOverview {
        commitments: section(ItemKind::Commitment, &|_, i| is_open(i.lifecycle)),
        open_questions: section(ItemKind::OpenQuestion, &|_, i| is_open(i.lifecycle)),
        decisions,
        risks: section(ItemKind::Risk, &|_, i| is_open(i.lifecycle)),
        done,
        meetings: gists,
        from_meetings: knowledge_candidates(&meetings, input.memory),
    }
}

/// Meeting kinds that can be kept as knowledge, with the knowledge kind each becomes.
const KNOWLEDGE_FROM: [(ItemKind, &str); 4] = [
    (ItemKind::Requirement, "requirement"),
    (ItemKind::Constraint, "requirement"),
    (ItemKind::Decision, "decision"),
    (ItemKind::Fact, "fact"),
];

/// Live requirements, constraints, decisions and facts across meetings, each text once, leaving
/// out what `memory` already says.
fn knowledge_candidates(
    meetings: &[&BriefMeeting],
    memory: &[MemoryEntry],
) -> Vec<KnowledgeCandidate> {
    let kept: HashSet<String> = memory.iter().map(|e| dedup_key(&e.text)).collect();
    let kinds: Vec<ItemKind> = KNOWLEDGE_FROM.iter().map(|(k, _)| *k).collect();
    select(meetings, &kinds, &|_, i| is_open(i.lifecycle))
        .into_iter()
        .filter(|(_, i)| !kept.contains(&dedup_key(&i.text)))
        .map(|(m, i)| KnowledgeCandidate {
            item: overview_item(m, i),
            knowledge_kind: KNOWLEDGE_FROM
                .iter()
                .find(|(k, _)| *k == i.kind)
                .map_or("fact", |(_, k)| k)
                .to_owned(),
            status: match i.status {
                EpistemicStatus::Stated => "stated",
                _ => "inferred",
            }
            .to_owned(),
            confidence: i.confidence,
            provenance: i
                .source_refs
                .iter()
                .map(|r| ProvenanceRef {
                    source_ref: r.clone(),
                    label: format!("{}, {}", one_line(&m.title), m.when),
                    sha256: String::new(),
                })
                .collect(),
        })
        .collect()
}

/// A hand-added item; `None` if its kind or lifecycle isn't one this version knows.
fn manual_item(item: &ProjectItem) -> Option<OverviewItem> {
    Some(OverviewItem {
        kind: parse_name(&item.kind)?,
        text: item.text.clone(),
        owner: item.owner.clone(),
        due: item.due.clone(),
        lifecycle: parse_name(&item.lifecycle)?,
        meeting: None,
        manual_id: Some(item.id.clone()),
    })
}

/// Items of `kinds` across meetings (newest first) that `keep` accepts, each text once. The newest
/// meeting to mention a text decides: if it resolved or withdrew the item, older copies stay hidden.
/// Within a meeting, live items claim a text before superseded and withdrawn ones.
fn select<'m>(
    meetings: &[&'m BriefMeeting],
    kinds: &[ItemKind],
    keep: &dyn Fn(&BriefMeeting, &StateItem) -> bool,
) -> Vec<(&'m BriefMeeting, &'m StateItem)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in meetings {
        let mut ordered: Vec<&StateItem> = m.state.live_items();
        ordered.extend(m.state.items.values().filter(|i| i.lifecycle.is_terminal()));
        for item in ordered {
            let text = m.original_text.get(&item.id).unwrap_or(&item.text);
            if !kinds.contains(&item.kind) || !seen.insert(dedup_key(text)) {
                continue;
            }
            if item.lifecycle.is_terminal() || !keep(m, item) {
                continue;
            }
            out.push((*m, item));
        }
    }
    out
}

fn meeting_items(
    meetings: &[&BriefMeeting],
    kinds: &[ItemKind],
    keep: &dyn Fn(&BriefMeeting, &StateItem) -> bool,
) -> Vec<OverviewItem> {
    select(meetings, kinds, keep)
        .into_iter()
        .map(|(m, i)| overview_item(m, i))
        .collect()
}

fn overview_item(m: &BriefMeeting, item: &StateItem) -> OverviewItem {
    OverviewItem {
        kind: item.kind,
        text: item.text.clone(),
        owner: item.owner.clone(),
        due: item.due.clone(),
        lifecycle: item.lifecycle,
        meeting: Some(ItemMeeting {
            id: m.id.clone(),
            title: m.title.clone(),
            when: m.when.clone(),
            started_at_ms: m.started_at_ms,
            item_id: item.id.clone(),
        }),
        manual_id: None,
    }
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
            original_text: Default::default(),
            summary: None,
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
            manual: &[],
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
        assert!(doc.starts_with(
            "# Acme — project brief\n\n\
             _Updated day 100 · 1 meeting · 0 open commitments · 0 open questions_\n"
        ));
        assert!(doc.contains("## Instructions\n\n> Watch the budget.\n"));
        let k = section(&doc, "Knowledge (4)");
        let facts = k.find("### Facts").unwrap();
        let reqs = k.find("### Requirements").unwrap();
        let other = k.find("### Glossary").unwrap();
        assert!(
            facts < reqs && reqs < other,
            "known kinds in order, others last"
        );
        assert!(k.contains("- Sarah owns security · _Kickoff, day 90_"));
        assert!(
            k.contains("- Runs in Azure · _Meeting Aug 3, 10:00_"),
            "a meeting no longer here falls back to the provenance label"
        );
        assert!(k.contains("- SoW means statement of work · _notes.md_"));
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
        assert!(!doc.contains("## Knowledge"), "no knowledge, no section");

        let com = section(&doc, "Open commitments (1)");
        assert!(com.contains(
            "- **Sarah** — Send the SOC 2 report · due Friday · _Design review, day 95_"
        ));
        assert_eq!(com.matches("SOC 2").count(), 1, "deduped by text");
        assert!(!com.contains("Book the venue") && !com.contains("Old plan"));

        assert!(section(&doc, "Open questions (1)")
            .contains("- Who signs off? · uncertain · _Kickoff, day 10_"));
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
        assert!(section(&doc, "Open commitments (0)").contains("Nothing open."));
    }

    fn manual(id: &str, kind: &str, text: &str, lifecycle: &str) -> ProjectItem {
        ProjectItem {
            id: id.into(),
            project_id: "p".into(),
            kind: kind.into(),
            text: text.into(),
            owner: Some("You".into()),
            due: None,
            lifecycle: lifecycle.into(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn hand_added_items_come_first_and_follow_their_lifecycle() {
        let meetings = [meeting(
            "m1",
            "Kickoff",
            90,
            vec![item(
                "RISK-1",
                ItemKind::Risk,
                "Vendor lock-in",
                Lifecycle::Active,
            )],
        )];
        let manual = [
            manual("pi-1", "risk", "Budget may slip", "active"),
            manual("pi-2", "commitment", "Book the room", "resolved"),
            manual("pi-3", "decision", "Ship in March", "active"),
            manual("pi-4", "someday_kind", "Unknown", "active"),
        ];
        let input = BriefInput {
            project: "Acme",
            date: "day 100",
            now_ms: 100 * DAY_MS,
            instructions: "",
            memory: &[],
            meetings: &meetings,
            manual: &manual,
        };
        let doc = project_brief(&input);
        let risks = section(&doc, "Active risks");
        let mine = risks
            .find("- **You** — Budget may slip · _Added by you_")
            .unwrap();
        assert!(
            mine < risks.find("Vendor lock-in").unwrap(),
            "hand-added first"
        );
        assert!(
            section(&doc, "Open commitments").contains("Nothing open."),
            "resolved is not open"
        );
        assert!(section(&doc, "Recent decisions")
            .contains("- **You** — Ship in March · _Added by you_"));
        assert!(!doc.contains("Unknown"), "an unknown kind is skipped");

        let overview = project_overview(&input);
        assert_eq!(overview.risks[0].manual_id.as_deref(), Some("pi-1"));
        let from_meeting = overview.risks[1].meeting.as_ref().unwrap();
        assert_eq!(
            (from_meeting.id.as_str(), from_meeting.item_id.as_str()),
            ("m1", "RISK-1")
        );
        assert_eq!(overview.done.len(), 1);
        assert_eq!(overview.done[0].text, "Book the room");
    }

    #[test]
    fn user_edits_in_a_meeting_log_reach_the_brief() {
        use crate::edit::{append_user_edit, ItemChange};
        use crate::ops::{AppliedOp, ResolvedOp};
        let add = |seq: u64, id: &str, kind: ItemKind, text: &str| AppliedOp {
            seq,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: id.into(),
                kind,
                text: text.into(),
                status: EpistemicStatus::Stated,
                confidence: 0.9,
                source_refs: vec![],
                related_items: vec![],
                owner: None,
                due: None,
            },
        };
        let mut log = vec![
            add(0, "COM-1", ItemKind::Commitment, "Send the SOC 2 report"),
            add(1, "COM-2", ItemKind::Commitment, "Draft the SOW"),
            add(2, "Q-1", ItemKind::OpenQuestion, "Who signs off?"),
        ];
        let edits = [
            (
                "COM-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Resolved),
                    ..Default::default()
                },
            ),
            (
                "COM-2",
                ItemChange {
                    text: Some("Draft the SOW v2".into()),
                    owner: Some("Sarah".into()),
                    ..Default::default()
                },
            ),
            (
                "Q-1",
                ItemChange {
                    lifecycle: Some(Lifecycle::Withdrawn),
                    ..Default::default()
                },
            ),
        ];
        let mut state = MeetingState::new("m2");
        for (id, change) in &edits {
            let (applied, s) = append_user_edit("m2", &log, id, change, 5).unwrap();
            log.push(applied);
            state = s;
        }
        // An older meeting said the same thing, still open there.
        let old = meeting(
            "m1",
            "Kickoff",
            10,
            vec![item(
                "COM-9",
                ItemKind::Commitment,
                "Send the SOC 2 report.",
                Lifecycle::Active,
            )],
        );
        let newer = BriefMeeting {
            id: "m2".into(),
            title: "Review".into(),
            when: "day 95".into(),
            started_at_ms: 95 * DAY_MS,
            state,
            original_text: Default::default(),
            summary: None,
        };
        let doc = brief(&[old, newer], &[], "");
        let com = section(&doc, "Open commitments");
        assert!(
            !com.contains("SOC 2"),
            "done in the newest meeting hides the older copy"
        );
        assert!(com.contains("- **Sarah** — Draft the SOW v2 · _Review, day 95_"));
        assert!(
            section(&doc, "Open questions").contains("Nothing open."),
            "deleted"
        );
    }

    #[test]
    fn the_tldr_comes_from_the_summary() {
        let wisp = "**TL;DR:** We picked  OIDC.\nShip in March.\n\n### Decisions\n\n- Use OIDC\n";
        assert_eq!(summary_tldr(wisp).as_deref(), Some("We picked OIDC."));
        let heading = "## Summary\n\n### TL;DR\n\nTwo lines\nof gist.\n\n### Risks\n- x";
        assert_eq!(summary_tldr(heading).as_deref(), Some("Two lines of gist."));
        let plain = "# Notes\n\nFirst paragraph\nhere.\n\nSecond.";
        assert_eq!(
            summary_tldr(plain).as_deref(),
            Some("First paragraph here.")
        );
        let bullets = "### Decisions\n\n- Use Postgres\n- Ship it";
        assert_eq!(summary_tldr(bullets).as_deref(), Some("Use Postgres"));
        assert_eq!(summary_tldr("  \n### Only a heading\n"), None);
        assert_eq!(summary_tldr(""), None);
    }

    #[test]
    fn meetings_are_listed_newest_first_with_their_gist() {
        let mut review = meeting(
            "m2",
            "Design review",
            95,
            vec![
                item("DEC-1", ItemKind::Decision, "Use OIDC", Lifecycle::Active),
                item(
                    "DEC-2",
                    ItemKind::Decision,
                    "Use SAML",
                    Lifecycle::Superseded,
                ),
                item(
                    "Q-1",
                    ItemKind::OpenQuestion,
                    "Who signs off?",
                    Lifecycle::Active,
                ),
            ],
        );
        review.summary = Some("**TL;DR:** We picked OIDC.\n\n### Decisions\n\n- Use OIDC\n".into());
        let kickoff = meeting("m1", "Kickoff", 10, vec![]);
        let doc = brief(&[kickoff, review], &[], "");
        let m = section(&doc, "Meetings (2)");
        assert!(m.contains(
            "### Design review — day 95\n\nWe picked OIDC.\n\n1 decision · 1 open question\n"
        ));
        assert!(m.contains("### Kickoff — day 10\n\n_No summary yet._\n"));
        assert!(m.find("Design review").unwrap() < m.find("Kickoff").unwrap());

        let overview = project_overview(&BriefInput {
            project: "",
            date: "",
            now_ms: 100 * DAY_MS,
            instructions: "",
            memory: &[],
            meetings: &[meeting("m1", "Kickoff", 10, vec![])],
            manual: &[],
        });
        assert_eq!(overview.meetings[0].tldr, None);
    }

    #[test]
    fn empty_sections_are_left_out_but_open_ones_say_so() {
        let doc = brief(&[], &[], "   ");
        assert!(doc.starts_with(
            "# Acme — project brief\n\n\
             _Updated day 100 · 0 meetings · 0 open commitments · 0 open questions_\n"
        ));
        assert!(doc.contains("## Open commitments (0)\n\nNothing open.\n"));
        assert!(doc.contains("## Open questions (0)\n\nNothing open.\n"));
        for gone in [
            "Instructions",
            "Recent decisions",
            "Active risks",
            "Meetings",
            "Knowledge",
        ] {
            assert!(!doc.contains(&format!("## {gone}")), "{gone}");
        }
    }

    #[test]
    fn meeting_knowledge_not_yet_kept_is_offered() {
        let mut req = item(
            "REQ-1",
            ItemKind::Requirement,
            "EU data only",
            Lifecycle::Active,
        );
        req.source_refs = vec!["Mm1:T4".into()];
        let mut con = item(
            "CON-1",
            ItemKind::Constraint,
            "Budget is fixed",
            Lifecycle::Active,
        );
        con.status = EpistemicStatus::Inferred;
        let meetings = [meeting(
            "m1",
            "Kickoff",
            90,
            vec![
                req,
                con,
                item("FACT-1", ItemKind::Fact, "Runs in Azure", Lifecycle::Active),
                item("FACT-2", ItemKind::Fact, "Old host", Lifecycle::Withdrawn),
                item("COM-1", ItemKind::Commitment, "Send it", Lifecycle::Active),
            ],
        )];
        let memory = [memory("fact", "runs in  azure.", None, "x")];
        let overview = project_overview(&BriefInput {
            project: "",
            date: "",
            now_ms: 100 * DAY_MS,
            instructions: "",
            memory: &memory,
            meetings: &meetings,
            manual: &[],
        });
        let got: Vec<(&str, &str, &str)> = overview
            .from_meetings
            .iter()
            .map(|c| {
                (
                    c.item.text.as_str(),
                    c.knowledge_kind.as_str(),
                    c.status.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("EU data only", "requirement", "stated"),
                ("Budget is fixed", "requirement", "inferred"),
            ],
            "kept and withdrawn items and commitments are left out"
        );
        let p = &overview.from_meetings[0].provenance[0];
        assert_eq!(
            (p.source_ref.as_str(), p.label.as_str()),
            ("Mm1:T4", "Kickoff, day 90")
        );
    }
}

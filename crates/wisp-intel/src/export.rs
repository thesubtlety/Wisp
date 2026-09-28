//! Portability: the meeting's structured state as documents to take elsewhere.
//!
//! - [`meeting_record`]: a Markdown meeting record for people.
//! - [`context_packet`]: the "AI context packet", everything another LLM needs to continue the
//!   work, in the brief's sections.
//! - [`state_json`]: the durable machine-readable export (items and the full op log).
//!
//! Evidence is quoted through a resolver while the raw source still exists; once retention deleted
//! it, the packet says so instead of quoting anything.

use std::fmt::Write;

use serde_json::{json, Value};
use wisp_library::MemoryEntry;

use crate::model::{ItemKind, MeetingState, StateItem};
use crate::ops::AppliedOp;

/// Most evidence lines quoted in a context packet.
pub const MAX_EVIDENCE: usize = 25;

/// What the export says about the meeting itself.
#[derive(Debug, Clone, Default)]
pub struct ExportMeta {
    pub title: String,
    /// When it happened, already formatted for display.
    pub when: String,
    pub project: Option<String>,
    /// What the user wanted from the meeting.
    pub focus: Option<String>,
    pub summary: Option<String>,
}

/// Turns a canonical evidence ref into a quotable line (`03:04 Them: …`), or `None` once the
/// raw source is gone.
pub type Resolver<'a> = &'a dyn Fn(&str) -> Option<String>;

fn line(item: &StateItem) -> String {
    let mut out = format!(
        "- {} [{} · {}%]",
        item.text,
        item.status.as_str().to_uppercase(),
        (item.confidence * 100.0).round() as i64
    );
    let mut extra = Vec::new();
    if let Some(o) = &item.owner {
        extra.push(format!("owner: {o}"));
    }
    if let Some(d) = &item.due {
        extra.push(format!("due: {d}"));
    }
    if !item.lifecycle.as_str().eq("active") {
        extra.push(item.lifecycle.as_str().to_owned());
    }
    if !extra.is_empty() {
        let _ = write!(out, " ({})", extra.join("; "));
    }
    let _ = write!(out, " {{{}}}", item.id);
    out
}

fn items_of<'a>(state: &'a MeetingState, kinds: &[ItemKind]) -> Vec<&'a StateItem> {
    state
        .live_items()
        .into_iter()
        .filter(|i| kinds.contains(&i.kind))
        .collect()
}

fn section(out: &mut String, heading: &str, items: &[&StateItem]) {
    let _ = writeln!(out, "\n## {heading}\n");
    if items.is_empty() {
        out.push_str("- (none)\n");
    }
    for i in items {
        let _ = writeln!(out, "{}", line(i));
    }
}

/// A Markdown meeting record: summary, then the live items by kind, then what was superseded or
/// withdrawn along the way.
pub fn meeting_record(meta: &ExportMeta, state: &MeetingState) -> String {
    let mut out = format!("# {}\n", meta.title);
    let mut sub = vec![meta.when.clone()];
    if let Some(p) = &meta.project {
        sub.push(format!("Project: {p}"));
    }
    let _ = writeln!(out, "\n_{}_", sub.join(" · "));
    if let Some(s) = meta.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        let _ = writeln!(out, "\n## Summary\n\n{}", s.trim());
    }
    for (heading, kinds) in [
        ("Decisions", &[ItemKind::Decision][..]),
        (
            "Requirements",
            &[ItemKind::Requirement, ItemKind::Constraint],
        ),
        (
            "Commitments and tasks",
            &[ItemKind::Commitment, ItemKind::TaskCandidate],
        ),
        ("Open questions", &[ItemKind::OpenQuestion]),
        ("Risks and conflicts", &[ItemKind::Risk, ItemKind::Conflict]),
        ("Facts", &[ItemKind::Fact, ItemKind::Assumption]),
        ("Participants", &[ItemKind::Participant]),
    ] {
        let items = items_of(state, kinds);
        if !items.is_empty() {
            section(&mut out, heading, &items);
        }
    }
    let history: Vec<&StateItem> = state
        .items
        .values()
        .filter(|i| i.lifecycle.is_terminal())
        .collect();
    if !history.is_empty() {
        let _ = writeln!(out, "\n## Changed during the meeting\n");
        for i in history {
            let replaced = i
                .superseded_by
                .as_deref()
                .map(|by| format!(" → replaced by {by}"))
                .unwrap_or_default();
            let _ = writeln!(out, "- ~~{}~~ ({}{replaced})", i.text, i.lifecycle.as_str());
        }
    }
    if state.items.is_empty() {
        out.push_str("\n_No structured state was recorded for this meeting._\n");
    }
    out
}

/// The AI context packet: enough to paste into another LLM and keep working.
pub fn context_packet(
    meta: &ExportMeta,
    state: &MeetingState,
    memory: &[MemoryEntry],
    evidence: Resolver,
) -> String {
    let mut out = format!("# AI context packet: {}\n\n_{}_\n", meta.title, meta.when);

    let _ = writeln!(out, "\n## Project context\n");
    match &meta.project {
        Some(p) => {
            let _ = writeln!(out, "Project: {p}");
        }
        None => out.push_str("Not part of a project.\n"),
    }
    for m in memory {
        let _ = writeln!(out, "- {} ({}, {})", m.text, m.kind, m.status);
    }

    let _ = writeln!(out, "\n## Meeting purpose\n");
    let _ = writeln!(
        out,
        "{}",
        meta.focus
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .unwrap_or("(not stated)")
    );

    for (heading, kinds) in [
        ("Participants", &[ItemKind::Participant][..]),
        ("Key facts", &[ItemKind::Fact, ItemKind::Constraint]),
        ("Requirements", &[ItemKind::Requirement]),
        ("Decisions", &[ItemKind::Decision]),
        ("Assumptions", &[ItemKind::Assumption]),
        ("Open questions", &[ItemKind::OpenQuestion]),
        ("Risks / conflicts", &[ItemKind::Risk, ItemKind::Conflict]),
        (
            "Tasks / commitments",
            &[ItemKind::Commitment, ItemKind::TaskCandidate],
        ),
    ] {
        section(&mut out, heading, &items_of(state, kinds));
    }

    let _ = writeln!(out, "\n## Relevant evidence\n");
    let mut seen = Vec::new();
    let mut expired = 0;
    for item in state.live_items() {
        for r in &item.source_refs {
            if seen.contains(r) || seen.len() >= MAX_EVIDENCE {
                continue;
            }
            seen.push(r.clone());
            match evidence(r) {
                Some(text) => {
                    let _ = writeln!(out, "- {{{}}} {text}", item.id);
                }
                None => expired += 1,
            }
        }
    }
    if seen.is_empty() {
        out.push_str("- (none)\n");
    }
    if expired > 0 {
        let _ = writeln!(
            out,
            "- {expired} source(s) no longer available: raw source expired under the retention policy."
        );
    }

    let _ = writeln!(out, "\n## What remains unclear\n");
    let unclear: Vec<&StateItem> = state
        .live_items()
        .into_iter()
        .filter(|i| {
            i.lifecycle == crate::model::Lifecycle::Uncertain
                || (i.status == crate::model::EpistemicStatus::Inferred && i.confidence < 0.8)
                || (i.kind == ItemKind::Commitment && (i.owner.is_none() || i.due.is_none()))
        })
        .collect();
    if unclear.is_empty() {
        out.push_str("- (nothing flagged)\n");
    }
    for i in unclear {
        let why = if i.lifecycle == crate::model::Lifecycle::Uncertain {
            "uncertain"
        } else if i.kind == ItemKind::Commitment {
            "commitment missing owner or date"
        } else {
            "inferred, not confirmed"
        };
        let _ = writeln!(out, "- {} ({why}) {{{}}}", i.text, i.id);
    }
    out
}

/// The durable export: meeting metadata, every item (history included), and the op log that
/// rebuilds them.
pub fn state_json(meta: &ExportMeta, state: &MeetingState, log: &[AppliedOp]) -> Value {
    json!({
        "format": "wisp-meeting-state",
        "version": 1,
        "meeting": {
            "id": state.meeting_id,
            "title": meta.title,
            "when": meta.when,
            "project": meta.project,
            "focus": meta.focus,
            "summary": meta.summary,
        },
        "items": state.items.values().collect::<Vec<_>>(),
        "log": log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpistemicStatus, Lifecycle};
    use crate::ops::ResolvedOp;

    fn add(
        seq: u64,
        id: &str,
        kind: ItemKind,
        text: &str,
        status: EpistemicStatus,
        conf: f64,
        owner: Option<&str>,
    ) -> AppliedOp {
        AppliedOp {
            seq,
            at_ms: 0,
            op: ResolvedOp::Add {
                id: id.into(),
                kind,
                text: text.into(),
                status,
                confidence: conf,
                source_refs: vec![format!("Mm:T{seq}")],
                related_items: vec![],
                owner: owner.map(str::to_owned),
                due: None,
            },
        }
    }

    fn log() -> Vec<AppliedOp> {
        use EpistemicStatus::*;
        vec![
            add(
                0,
                "REQ-1",
                ItemKind::Requirement,
                "Production runs in Azure",
                Stated,
                0.95,
                None,
            ),
            add(
                1,
                "DEC-1",
                ItemKind::Decision,
                "Use SAML",
                Stated,
                0.9,
                None,
            ),
            add(
                2,
                "COM-1",
                ItemKind::Commitment,
                "Send traffic numbers",
                Stated,
                0.9,
                Some("Customer"),
            ),
            add(
                3,
                "ASM-1",
                ItemKind::Assumption,
                "Customer-hosted deployment",
                Inferred,
                0.6,
                None,
            ),
            add(
                4,
                "DEC-2",
                ItemKind::Decision,
                "Use OIDC",
                Stated,
                0.9,
                None,
            ),
            AppliedOp {
                seq: 5,
                at_ms: 0,
                op: ResolvedOp::SetLifecycle {
                    id: "DEC-1".into(),
                    lifecycle: Lifecycle::Superseded,
                    superseded_by: Some("DEC-2".into()),
                    add_refs: vec![],
                },
            },
        ]
    }

    fn meta() -> ExportMeta {
        ExportMeta {
            title: "Acme scoping".into(),
            when: "Sept 26, 2026".into(),
            project: Some("Acme implementation".into()),
            focus: Some("Scope hosting".into()),
            summary: Some("Azure, SSO settled on OIDC.".into()),
        }
    }

    #[test]
    fn the_record_lists_live_items_by_kind_and_what_changed() {
        let state = MeetingState::replay("m", &log()).unwrap();
        let md = meeting_record(&meta(), &state);
        assert!(md.starts_with("# Acme scoping\n\n_Sept 26, 2026 · Project: Acme implementation_\n\n## Summary\n\nAzure, SSO settled on OIDC."));
        assert!(md.contains("## Decisions\n\n- Use OIDC [STATED · 90%] {DEC-2}"));
        assert!(md.contains("- Send traffic numbers [STATED · 90%] (owner: Customer) {COM-1}"));
        assert!(md.contains(
            "## Changed during the meeting\n\n- ~~Use SAML~~ (superseded → replaced by DEC-2)"
        ));
        assert!(
            !md.contains("- Use SAML ["),
            "superseded items aren't current"
        );
    }

    #[test]
    fn the_packet_has_the_briefs_sections_quotes_live_evidence_and_notes_expired_sources() {
        let state = MeetingState::replay("m", &log()).unwrap();
        let memory = vec![MemoryEntry {
            id: 1,
            project_id: "p".into(),
            kind: "fact".into(),
            text: "Customer is Acme Corp".into(),
            status: "stated".into(),
            confidence: 1.0,
            provenance: vec![],
            meeting_id: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }];
        // T0 and T2 still exist; the rest expired.
        let resolver = |r: &str| match r {
            "Mm:T0" => Some("00:10 Them: It has to run in our Azure tenant.".to_owned()),
            "Mm:T2" => Some("00:30 Them: We'll send traffic numbers.".to_owned()),
            _ => None,
        };
        let packet = context_packet(&meta(), &state, &memory, &resolver);
        let headings: Vec<&str> = packet.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            headings,
            [
                "## Project context",
                "## Meeting purpose",
                "## Participants",
                "## Key facts",
                "## Requirements",
                "## Decisions",
                "## Assumptions",
                "## Open questions",
                "## Risks / conflicts",
                "## Tasks / commitments",
                "## Relevant evidence",
                "## What remains unclear",
            ]
        );
        assert!(
            packet.contains("Project: Acme implementation\n- Customer is Acme Corp (fact, stated)")
        );
        assert!(packet.contains("## Meeting purpose\n\nScope hosting"));
        assert!(packet.contains("- {REQ-1} 00:10 Them: It has to run in our Azure tenant."));
        assert!(packet.contains("source(s) no longer available: raw source expired"));
        assert!(packet.contains("- Customer-hosted deployment (inferred, not confirmed) {ASM-1}"));
        assert!(
            packet.contains("- Send traffic numbers (commitment missing owner or date) {COM-1}")
        );
        assert!(packet.contains("## Participants\n\n- (none)"));
    }

    #[test]
    fn the_json_export_rebuilds_the_state() {
        let log = log();
        let state = MeetingState::replay("m", &log).unwrap();
        let v = state_json(&meta(), &state, &log);
        assert_eq!(v["format"], "wisp-meeting-state");
        assert_eq!(v["meeting"]["project"], "Acme implementation");
        assert_eq!(v["items"].as_array().unwrap().len(), 5);
        let back: Vec<AppliedOp> = serde_json::from_value(v["log"].clone()).unwrap();
        assert_eq!(MeetingState::replay("m", &back).unwrap(), state);
    }

    #[test]
    fn an_empty_meeting_still_exports() {
        let state = MeetingState::new("m");
        assert!(meeting_record(&ExportMeta::default(), &state).contains("No structured state"));
        let packet = context_packet(&ExportMeta::default(), &state, &[], &|_| None);
        assert!(packet.contains("Not part of a project.") && packet.contains("(not stated)"));
    }
}

//! Meeting intelligence for Wisp: a structured [`MeetingState`] kept up to date from final
//! transcript lines, with provenance that is part of the data model.
//!
//! - [`model`] — items (requirements, decisions, commitments, open questions, ...), each stated,
//!   inferred or suggested, with a confidence, a lifecycle, and the evidence it rests on.
//! - [`evidence`] — the packet of short evidence IDs (`T12`, `D17:C4`, `M1:T221`) given to one
//!   reasoning call, mapped to canonical library refs.
//! - [`reducer`] — validates proposed ops against that packet and the state, applies the valid
//!   ones deterministically, and says why the rest were rejected.
//! - [`ops`] — the op format, its JSON Schema, and the replayable log of applied ops.
//! - [`about`] — what matters to You: "About me" plus the project's instructions, for prompts.
//! - [`analyze`] — "Analyze Now": one observer pass through a
//!   [`wisp_reasoning::ReasoningBackend`].
//! - [`ask`] — questions about the meeting, answered with checked citations.
//! - [`brief`] — the project brief: knowledge and live items across a project's meetings.
//! - [`edit`] — the user's hand edits to items, appended to a meeting's log, and hand-added
//!   project items.
//! - [`export`] — the meeting record, the AI context packet and the state as JSON.
//! - [`endgame`] — wrap-up signals and the gap audit run when the meeting is closing.
//! - [`intervene`] — proposed interventions and the conservative local filter that decides
//!   which reach the user, with a log for tuning.
//! - [`meeting_type`] — how a meeting type tunes the passes, the audit, and the type suggestion.
//! - [`learning`] — end-of-meeting proposals for project memory, with hashed provenance.
//! - [`review`] — post-call follow-up review, corrected in plain words, applied to the state.
//! - [`headline`] — short forms (headlines, short answers) for reading at a glance.
//! - [`screenshot`] — a screenshot attached as context, described once by a vision backend.
//! - [`speakers`] — speaker-name suggestions from the transcript, checked and deduplicated.
//! - [`summary`] — the meeting summary, from its state (or the transcript's end when it has none).
//! - [`runtime`] — the live worker: final lines in over a channel that never blocks, batched
//!   passes out, one at a time.
//!
//! Nothing here knows about Tauri, audio or the UI. The model proposes; the reducer decides.

pub mod about;
pub mod analyze;
pub mod ask;
pub mod brief;
pub mod edit;
pub mod endgame;
pub mod evidence;
pub mod export;
pub mod headline;
pub mod intervene;
pub mod learning;
pub mod meeting_type;
pub mod model;
pub mod ops;
pub mod reducer;
pub mod review;
pub mod runtime;
pub mod screenshot;
pub mod speakers;
pub mod summary;

pub use about::{about_you, ABOUT_HEADING};
pub use analyze::{
    analyze_now, prepare_observe, retrieval_text, AnalyzeInput, AnalyzeOutcome, IntelError,
    PreparedPass,
};
pub use ask::{ask, ask_schema, prepare_ask, AskAnswer, AskInput, AskTurn, Citation};
pub use brief::{
    iso_date, item_counts, project_brief, project_overview, summary_tldr, BriefInput, BriefMeeting,
    ItemMeeting, KnowledgeCandidate, MeetingGist, OverviewItem, ProjectOverview,
};
pub use edit::{
    append_user_edit, edit_manual_item, new_manual_item, original_texts, user_edit, EditError,
    ItemChange, MANUAL_KINDS,
};
pub use endgame::{
    audit, prepare_audit, wrap_probability, AuditInput, AuditReport, EndgameTrigger, Gap,
    GapCategory,
};
pub use evidence::{memory_ref, EvidenceDetail, EvidencePacket, TranscriptLine};
pub use export::{context_packet, meeting_record, state_json, ExportMeta};
pub use intervene::{
    validate_candidate, Candidate, CandidateKind, Card, Decision, InterventionFilter,
    InterventionPolicy, LogEntry, Prefer, RawCandidate, Suppressed,
};
pub use learning::{prepare_learning, propose_learning, LearningInput, Proposal, KNOWLEDGE_KINDS};
pub use meeting_type::{
    apply_type, intervention_policy, trigger_policy, validate_type_guess, GuessReject,
    RawTypeGuess, TypeGuess,
};
pub use model::{EpistemicStatus, ItemKind, Lifecycle, MeetingState, SourceRef, StateItem};
pub use ops::{output_schema, AppliedOp, ModelOp, OpBatch, OpKind, ReplayError, ResolvedOp};
pub use reducer::{reduce, ApplyReport, RejectReason, Rejection};
pub use review::{
    apply_edits, fallback_followups, generate_followups, interpret_reply, parse_reply, review_ops,
    FollowUp, FollowUpClass, ReviewEdit,
};
pub use runtime::{
    remap_refs, saved_positions, Finished, IntelRuntime, IntelUpdate, NoRetrieval, ProjectContext,
    Retriever, RuntimeConfig, TriggerPolicy, LIVE_MEETING_ID,
};
pub use screenshot::{describe_screenshot, ScreenshotDescription, ScreenshotInput};
pub use speakers::{
    validate_speaker_name, RawSpeakerName, SpeakerReject, SpeakerSuggestion, SpeakerSuggestions,
};
pub use summary::{
    parse_summary, participants, summary_context, summary_request, summary_request_with_sections,
    MeetingSummary, SummaryContext, SummaryMeta, SummarySection,
};

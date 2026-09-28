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
//! - [`analyze`] — "Analyze Now": one observer pass through a
//!   [`wisp_reasoning::ReasoningBackend`].
//! - [`ask`] — questions about the meeting, answered with checked citations.
//! - [`runtime`] — the live worker: final lines in over a channel that never blocks, batched
//!   passes out, one at a time.
//!
//! Nothing here knows about Tauri, audio or the UI. The model proposes; the reducer decides.

pub mod analyze;
pub mod ask;
pub mod evidence;
pub mod model;
pub mod ops;
pub mod reducer;
pub mod runtime;

pub use analyze::{
    analyze_now, prepare_observe, retrieval_text, AnalyzeInput, AnalyzeOutcome, IntelError,
    PreparedPass,
};
pub use ask::{ask, ask_schema, prepare_ask, AskAnswer, AskInput, AskTurn, Citation};
pub use evidence::{EvidenceDetail, EvidencePacket, TranscriptLine};
pub use model::{EpistemicStatus, ItemKind, Lifecycle, MeetingState, SourceRef, StateItem};
pub use ops::{output_schema, AppliedOp, ModelOp, OpBatch, OpKind, ReplayError, ResolvedOp};
pub use reducer::{reduce, ApplyReport, RejectReason, Rejection};
pub use runtime::{
    remap_refs, saved_positions, Finished, IntelRuntime, IntelUpdate, NoRetrieval, Retriever,
    RuntimeConfig, TriggerPolicy, LIVE_MEETING_ID,
};

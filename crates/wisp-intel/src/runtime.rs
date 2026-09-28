//! The live intelligence runtime: final transcript lines go in, observer passes come out, and
//! nothing on the audio side ever waits for a model.
//!
//! [`IntelRuntime::spawn`] starts a worker thread that owns the [`MeetingState`]. Callers hand it
//! final lines with [`IntelRuntime::push_final`], which only sends on an unbounded channel. The
//! worker batches new lines and runs a pass when a [`TriggerPolicy`] says there is enough new
//! material, or at once on [`IntelRuntime::analyze_now`]. One pass runs at a time; lines that
//! arrive meanwhile wait for the next. [`IntelRuntime::stop`] cancels any pass in flight and hands
//! back the state and its log.
//!
//! Lines are numbered in arrival order under the meeting id [`LIVE_MEETING_ID`]. When the meeting
//! is saved, [`remap_refs`] rewrites those refs to the stored transcript's ids.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wisp_library::Snippet;
use wisp_reasoning::{CancelToken, ReasoningBackend};

use crate::analyze::{analyze_now, retrieval_text, AnalyzeInput, IntelError};
use crate::endgame::{
    audit, wrap_probability, AuditInput, AuditReport, EndgameTrigger, SCHEDULED_LEAD_MS,
    WRAP_SUGGEST_AT,
};
use crate::evidence::TranscriptLine;
use crate::intervene::{Card, InterventionFilter, InterventionPolicy, LogEntry};
use crate::model::{MeetingState, StateItem};
use crate::ops::{AppliedOp, ResolvedOp};

/// The meeting id the runtime uses until the meeting is saved under its real id.
pub const LIVE_MEETING_ID: &str = "live";

/// The longest wait after repeated failures.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// How often the worker wakes to re-check the policy when no line arrives.
const TICK: Duration = Duration::from_millis(500);

/// When the worker runs a pass on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerPolicy {
    /// Enough new text to be worth a pass, in characters.
    pub min_new_chars: usize,
    /// Least time between the start of one pass and the next. After failures the wait doubles
    /// per failure, up to [`MAX_BACKOFF`].
    pub min_interval: Duration,
    /// Run anyway once the oldest unanalyzed line is this old, if there is any new text.
    pub max_wait: Duration,
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self {
            min_new_chars: 400,
            min_interval: Duration::from_secs(20),
            max_wait: Duration::from_secs(60),
        }
    }
}

impl TriggerPolicy {
    /// Whether to run a pass now. `since_last` is `None` before the first pass.
    pub fn should_run(
        &self,
        pending_chars: usize,
        since_last: Option<Duration>,
        oldest_pending_age: Duration,
    ) -> bool {
        if pending_chars == 0 {
            return false;
        }
        if since_last.is_some_and(|t| t < self.min_interval) {
            return false;
        }
        pending_chars >= self.min_new_chars || oldest_pending_age >= self.max_wait
    }
}

/// Retrieves project context for a pass. Errors should come back as an empty list: a pass without
/// context is better than no pass.
pub trait Retriever: Send {
    fn retrieve(&self, text: &str) -> Vec<Snippet>;
}

/// A retriever that finds nothing.
pub struct NoRetrieval;

impl Retriever for NoRetrieval {
    fn retrieve(&self, _text: &str) -> Vec<Snippet> {
        Vec::new()
    }
}

/// What the runtime reports after each attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum IntelUpdate {
    /// A pass finished.
    Pass {
        applied: usize,
        rejected: usize,
        /// Items not superseded or withdrawn, after the pass.
        live_items: Vec<StateItem>,
        /// New lines still waiting because the pass was full.
        remaining_lines: usize,
        backend: String,
        /// Cards the local filter chose to show from this pass (usually none).
        cards: Vec<Card>,
    },
    /// Analyze Now was asked for with nothing new.
    NothingNew,
    /// The meeting looks like it is ending; advisory, sent at most once per meeting.
    WrapSuggested(EndgameTrigger),
    /// The gap audit Wrapping Up asked for.
    Audit {
        report: AuditReport,
        backend: String,
    },
    /// A pass failed; the state is unchanged and the lines will be tried again.
    Failed(String),
}

/// Settings for one meeting's runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub policy: TriggerPolicy,
    /// What the user wants from this meeting.
    pub focus: Option<String>,
    /// Longest a single pass may take.
    pub timeout: Duration,
    /// Which proposed interventions reach the user.
    pub interventions: InterventionPolicy,
    /// When the meeting is scheduled to end (epoch ms), if known.
    pub scheduled_end_ms: Option<i64>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            policy: TriggerPolicy::default(),
            focus: None,
            timeout: Duration::from_secs(180),
            interventions: InterventionPolicy::default(),
            scheduled_end_ms: None,
        }
    }
}

/// What a stopped runtime hands back.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub state: MeetingState,
    /// Every applied op, in order; replays to `state`.
    pub log: Vec<AppliedOp>,
    /// Final lines received, in arrival order.
    pub lines: usize,
    /// Every intervention candidate considered, what was decided, and what was dismissed.
    pub candidate_log: Vec<LogEntry>,
}

enum Msg {
    Final {
        speaker: String,
        start_ms: i64,
        text: String,
    },
    AnalyzeNow,
    Dismiss(String),
    WrapUp,
    SetScheduledEnd(Option<i64>),
    Stop,
}

/// A running intelligence worker for one meeting.
pub struct IntelRuntime {
    tx: Sender<Msg>,
    cancel: CancelToken,
    snapshot: Arc<Mutex<MeetingState>>,
    worker: Option<JoinHandle<Finished>>,
}

type OnUpdate = Box<dyn Fn(IntelUpdate) + Send>;
type Clock = Box<dyn Fn() -> i64 + Send>;

impl IntelRuntime {
    /// Starts the worker. `on_update` runs on the worker thread after each attempt; `now_ms` stamps
    /// applied ops.
    pub fn spawn(
        backend: Arc<dyn ReasoningBackend>,
        retriever: Box<dyn Retriever>,
        config: RuntimeConfig,
        on_update: OnUpdate,
        now_ms: Clock,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let snapshot = Arc::new(Mutex::new(MeetingState::new(LIVE_MEETING_ID)));
        let worker_snapshot = snapshot.clone();
        let config_interventions = config.interventions.clone();
        let worker = std::thread::Builder::new()
            .name("wisp-intel".into())
            .spawn(move || {
                Worker {
                    backend,
                    retriever,
                    config,
                    on_update,
                    now_ms,
                    cancel: worker_cancel,
                    state: MeetingState::new(LIVE_MEETING_ID),
                    log: Vec::new(),
                    lines: Vec::new(),
                    last_pass: None,
                    pending_since: None,
                    failures: 0,
                    snapshot: worker_snapshot,
                    filter: InterventionFilter::new(config_interventions),
                    endgame: None,
                    wrap_suggested: false,
                }
                .run(rx)
            })
            .expect("spawn wisp-intel worker");
        Self {
            tx,
            cancel,
            snapshot,
            worker: Some(worker),
        }
    }

    /// Queues a final line. Never blocks; blank lines are dropped (the library drops them too, so
    /// numbering stays aligned with the saved transcript).
    pub fn push_final(&self, speaker: impl Into<String>, start_ms: i64, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        let _ = self.tx.send(Msg::Final {
            speaker: speaker.into(),
            start_ms,
            text,
        });
    }

    /// Asks for a pass as soon as the worker is free, whatever the policy says.
    pub fn analyze_now(&self) {
        let _ = self.tx.send(Msg::AnalyzeNow);
    }

    /// Enters endgame and runs the gap audit as soon as the worker is free. Recording goes on; press
    /// again for a fresh audit.
    pub fn wrap_up(&self) {
        let _ = self.tx.send(Msg::WrapUp);
    }

    /// Sets (or clears) when the meeting is scheduled to end, in epoch ms.
    pub fn set_scheduled_end(&self, end_ms: Option<i64>) {
        let _ = self.tx.send(Msg::SetScheduledEnd(end_ms));
    }

    /// Records that the user dismissed a card, so the filter holds back repeats.
    pub fn dismiss(&self, card_id: impl Into<String>) {
        let _ = self.tx.send(Msg::Dismiss(card_id.into()));
    }

    /// A copy of the state as of the last completed pass.
    pub fn snapshot(&self) -> MeetingState {
        self.snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| MeetingState::new(LIVE_MEETING_ID))
    }

    /// Cancels any pass in flight, stops the worker, and returns the state and its log.
    pub fn stop(mut self) -> Finished {
        self.shutdown().unwrap_or_else(|| Finished {
            state: MeetingState::new(LIVE_MEETING_ID),
            log: Vec::new(),
            lines: 0,
            candidate_log: Vec::new(),
        })
    }

    fn shutdown(&mut self) -> Option<Finished> {
        self.cancel.cancel();
        let _ = self.tx.send(Msg::Stop);
        self.worker.take().and_then(|w| w.join().ok())
    }
}

impl Drop for IntelRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct Worker {
    backend: Arc<dyn ReasoningBackend>,
    retriever: Box<dyn Retriever>,
    config: RuntimeConfig,
    on_update: OnUpdate,
    now_ms: Clock,
    cancel: CancelToken,
    state: MeetingState,
    log: Vec<AppliedOp>,
    lines: Vec<TranscriptLine>,
    last_pass: Option<Instant>,
    /// When the oldest line not yet analyzed arrived.
    pending_since: Option<Instant>,
    /// Failed passes in a row.
    failures: u32,
    /// Shared copy of `state`, refreshed after each pass.
    snapshot: Arc<Mutex<MeetingState>>,
    filter: InterventionFilter,
    /// Set once the user presses Wrapping Up.
    endgame: Option<EndgameTrigger>,
    /// Whether the advisory wrap-up suggestion has been sent.
    wrap_suggested: bool,
}

impl Worker {
    fn run(mut self, rx: Receiver<Msg>) -> Finished {
        loop {
            let mut manual = false;
            let mut wrap_up = false;
            let lines_before = self.lines.len();
            let first = match rx.recv_timeout(TICK) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut stop = false;
            for msg in first.into_iter().chain(rx.try_iter()) {
                match msg {
                    Msg::Final {
                        speaker,
                        start_ms,
                        text,
                    } => {
                        self.lines.push(TranscriptLine {
                            idx: self.lines.len() as i64,
                            start_ms,
                            speaker,
                            text,
                        });
                        self.pending_since.get_or_insert_with(Instant::now);
                    }
                    Msg::AnalyzeNow => manual = true,
                    Msg::Dismiss(id) => {
                        self.filter.dismiss(&id, (self.now_ms)());
                    }
                    Msg::WrapUp => wrap_up = true,
                    Msg::SetScheduledEnd(end) => self.config.scheduled_end_ms = end,
                    Msg::Stop => stop = true,
                }
            }
            if stop || self.cancel.is_cancelled() {
                break;
            }
            if wrap_up {
                self.endgame = Some(EndgameTrigger::Manual);
                self.wrap_suggested = true;
                self.filter.set_endgame(true);
                self.run_audit();
                continue;
            }
            self.maybe_suggest_wrap(self.lines.len() > lines_before);
            let pending = self.pending_chars();
            let backing_off = self.last_pass.is_some_and(|t| t.elapsed() < self.backoff());
            let due = !backing_off
                && self.config.policy.should_run(
                    pending,
                    self.last_pass.map(|t| t.elapsed()),
                    self.pending_since.map_or(Duration::ZERO, |t| t.elapsed()),
                );
            if manual || due {
                self.pass();
            }
        }
        Finished {
            state: self.state,
            log: self.log,
            lines: self.lines.len(),
            candidate_log: self.filter.log().to_vec(),
        }
    }

    /// The wait after the last failure: the interval doubled per failure, capped. Zero with no
    /// failures (the policy's own interval applies then).
    fn backoff(&self) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        let factor = 1u32 << self.failures.min(8);
        (self.config.policy.min_interval * factor).min(MAX_BACKOFF)
    }

    /// Sends the advisory wrap-up suggestion once: when the scheduled end is near, or when the
    /// latest lines sound like a wrap-up.
    fn maybe_suggest_wrap(&mut self, new_lines: bool) {
        if self.endgame.is_some() || self.wrap_suggested {
            return;
        }
        let trigger = if self
            .config
            .scheduled_end_ms
            .is_some_and(|end| (self.now_ms)() >= end - SCHEDULED_LEAD_MS)
        {
            Some(EndgameTrigger::Scheduled)
        } else if new_lines && wrap_probability(&self.lines) >= WRAP_SUGGEST_AT {
            Some(EndgameTrigger::Semantic)
        } else {
            None
        };
        if let Some(trigger) = trigger {
            self.wrap_suggested = true;
            (self.on_update)(IntelUpdate::WrapSuggested(trigger));
        }
    }

    /// The gap audit, run on the worker right away.
    fn run_audit(&mut self) {
        let query = {
            let text: String = self
                .lines
                .iter()
                .rev()
                .take(20)
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            text.chars().take(1500).collect::<String>()
        };
        let retrieved = if query.is_empty() {
            Vec::new()
        } else {
            self.retriever.retrieve(&query)
        };
        let result = audit(
            self.backend.as_ref(),
            &self.cancel,
            &AuditInput {
                transcript: &self.lines,
                state: &self.state,
                retrieved: &retrieved,
                focus: self.config.focus.as_deref(),
                timeout: self.config.timeout,
            },
        );
        let update = match result {
            Ok(report) => IntelUpdate::Audit {
                report,
                backend: self.backend.name().to_owned(),
            },
            Err(e) => IntelUpdate::Failed(e.to_string()),
        };
        if !self.cancel.is_cancelled() {
            (self.on_update)(update);
        }
    }

    fn pending_chars(&self) -> usize {
        let start = self
            .state
            .analyzed_through
            .map_or(0, |t| (t + 1).max(0) as usize);
        self.lines
            .get(start..)
            .map_or(0, |l| l.iter().map(|x| x.text.chars().count()).sum())
    }

    fn pass(&mut self) {
        self.last_pass = Some(Instant::now());
        let query = retrieval_text(&self.state, &self.lines);
        let retrieved = if query.is_empty() {
            Vec::new()
        } else {
            self.retriever.retrieve(&query)
        };
        let input = AnalyzeInput {
            transcript: &self.lines,
            retrieved: &retrieved,
            focus: self.config.focus.as_deref(),
            endgame: self.endgame.is_some(),
            timeout: self.config.timeout,
        };
        let now = (self.now_ms)();
        let update = match analyze_now(
            self.backend.as_ref(),
            &self.cancel,
            &mut self.state,
            &input,
            now,
        ) {
            Ok(out) => {
                self.failures = 0;
                self.log.extend(out.report.applied.iter().cloned());
                if let Ok(mut shared) = self.snapshot.lock() {
                    *shared = self.state.clone();
                }
                let mut cards = Vec::new();
                for candidate in out.candidates {
                    cards.extend(self.filter.consider(candidate, now));
                }
                for (title, reason) in &out.rejected_candidates {
                    self.filter.reject(title, reason, now);
                }
                self.pending_since = (out.remaining_lines > 0).then(Instant::now);
                IntelUpdate::Pass {
                    applied: out.report.applied.len(),
                    rejected: out.report.rejected.len(),
                    live_items: self.state.live_items().into_iter().cloned().collect(),
                    remaining_lines: out.remaining_lines,
                    backend: out.backend,
                    cards,
                }
            }
            Err(IntelError::NothingNew) => IntelUpdate::NothingNew,
            Err(e) => {
                self.failures += 1;
                IntelUpdate::Failed(e.to_string())
            }
        };
        if !self.cancel.is_cancelled() {
            (self.on_update)(update);
        }
    }
}

/// Rewrites every source ref in `log` through `map`; refs it returns `None` for are kept.
pub fn remap_refs(log: &[AppliedOp], map: impl Fn(&str) -> Option<String>) -> Vec<AppliedOp> {
    let remap = |refs: &[String]| -> Vec<String> {
        refs.iter()
            .map(|r| map(r).unwrap_or_else(|| r.clone()))
            .collect()
    };
    log.iter()
        .map(|applied| {
            let op = match &applied.op {
                ResolvedOp::Add {
                    id,
                    kind,
                    text,
                    status,
                    confidence,
                    source_refs,
                    related_items,
                    owner,
                    due,
                } => ResolvedOp::Add {
                    id: id.clone(),
                    kind: *kind,
                    text: text.clone(),
                    status: *status,
                    confidence: *confidence,
                    source_refs: remap(source_refs),
                    related_items: related_items.clone(),
                    owner: owner.clone(),
                    due: due.clone(),
                },
                ResolvedOp::Update {
                    id,
                    text,
                    confidence,
                    owner,
                    due,
                    add_refs,
                    add_related,
                } => ResolvedOp::Update {
                    id: id.clone(),
                    text: text.clone(),
                    confidence: *confidence,
                    owner: owner.clone(),
                    due: due.clone(),
                    add_refs: remap(add_refs),
                    add_related: add_related.clone(),
                },
                ResolvedOp::SetLifecycle {
                    id,
                    lifecycle,
                    superseded_by,
                    add_refs,
                } => ResolvedOp::SetLifecycle {
                    id: id.clone(),
                    lifecycle: *lifecycle,
                    superseded_by: superseded_by.clone(),
                    add_refs: remap(add_refs),
                },
            };
            AppliedOp {
                seq: applied.seq,
                at_ms: applied.at_ms,
                op,
            }
        })
        .collect()
}

/// Where each line lands in the saved transcript, which orders lines by start time (stably) while
/// the runtime numbers them by arrival. `starts[i]` is the start of the i-th line to arrive, in
/// whatever precision the saver sorts by; the result's `[i]` is its saved index.
pub fn saved_positions<K: Ord>(starts: &[K]) -> Vec<i64> {
    let mut order: Vec<usize> = (0..starts.len()).collect();
    order.sort_by_key(|&i| &starts[i]);
    let mut saved = vec![0; starts.len()];
    for (pos, &arrival) in order.iter().enumerate() {
        saved[arrival] = pos as i64;
    }
    saved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, Lifecycle};
    use serde_json::json;
    use std::sync::Mutex;
    use wisp_reasoning::{ReasoningError, ScriptedBackend};

    fn fast_policy() -> TriggerPolicy {
        TriggerPolicy {
            min_new_chars: 20,
            min_interval: Duration::from_millis(0),
            max_wait: Duration::from_secs(3600),
        }
    }

    fn add_citing_last_line() -> ScriptedBackend {
        ScriptedBackend::with_responder("scripted", |req| {
            // Cite the newest line in the request.
            let last = req
                .context
                .lines()
                .filter_map(|l| l.strip_prefix('[')?.split(']').next())
                .rfind(|id| id.starts_with('T'))
                .unwrap_or("T0")
                .to_owned();
            Ok(json!({"ops": [{
                "op": "add", "id": null, "temp_id": null, "kind": "fact",
                "text": format!("noted {last}"), "epistemic_status": "stated", "confidence": 0.8,
                "lifecycle": null, "superseded_by": null, "owner": null, "due": null,
                "source_refs": [last], "related_items": []
            }], "candidates": []}))
        })
    }

    fn collect() -> (Arc<Mutex<Vec<IntelUpdate>>>, OnUpdate) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (seen, Box::new(move |u| sink.lock().unwrap().push(u)))
    }

    fn wait_for(seen: &Arc<Mutex<Vec<IntelUpdate>>>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().len() < n {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n} update(s)"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_policy_waits_for_enough_text_or_enough_time() {
        let p = TriggerPolicy::default();
        let s = Duration::from_secs;
        assert!(!p.should_run(0, None, s(999)), "nothing new");
        assert!(!p.should_run(100, None, s(5)), "too little, too soon");
        assert!(p.should_run(400, None, s(0)), "enough text, first pass");
        assert!(
            !p.should_run(5000, Some(s(10)), s(30)),
            "inside the interval"
        );
        assert!(p.should_run(5000, Some(s(20)), s(0)), "interval passed");
        assert!(
            p.should_run(10, Some(s(30)), s(60)),
            "small but waited long enough"
        );
    }

    #[test]
    fn failures_back_off_exponentially_up_to_a_cap() {
        let mut w = Worker {
            backend: Arc::new(ScriptedBackend::named("s")),
            retriever: Box::new(NoRetrieval),
            config: RuntimeConfig::default(),
            on_update: Box::new(|_| {}),
            now_ms: Box::new(|| 0),
            cancel: CancelToken::new(),
            state: MeetingState::new(LIVE_MEETING_ID),
            log: Vec::new(),
            lines: Vec::new(),
            last_pass: None,
            pending_since: None,
            failures: 0,
            snapshot: Arc::new(Mutex::new(MeetingState::new(LIVE_MEETING_ID))),
            filter: InterventionFilter::default(),
            endgame: None,
            wrap_suggested: false,
        };
        assert_eq!(w.backoff(), Duration::ZERO);
        w.failures = 1;
        assert_eq!(w.backoff(), Duration::from_secs(40));
        w.failures = 2;
        assert_eq!(w.backoff(), Duration::from_secs(80));
        w.failures = 30;
        assert_eq!(w.backoff(), MAX_BACKOFF);
    }

    #[test]
    fn finals_trigger_passes_and_stop_returns_state_and_log() {
        let backend = Arc::new(add_citing_last_line());
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 7),
        );
        rt.push_final("Them", 0, "Production runs in our Azure tenant.");
        wait_for(&seen, 1);
        rt.push_final("Them", 1000, "   ");
        rt.push_final("You", 2000, "And what about single sign-on?");
        wait_for(&seen, 2);
        assert_eq!(
            rt.snapshot().items.len(),
            2,
            "the snapshot follows each pass"
        );
        let done = rt.stop();

        assert_eq!(done.lines, 2, "the blank line is dropped");
        assert_eq!(done.state.analyzed_through, Some(1));
        assert_eq!(done.log.len(), 2);
        assert_eq!(
            MeetingState::replay(LIVE_MEETING_ID, &done.log)
                .unwrap()
                .items,
            done.state.items,
            "the log rebuilds the items (analysis progress is not an op)"
        );
        let fact = done.state.item("FACT-2").unwrap();
        assert_eq!(fact.source_refs, ["Mlive:T1"]);
        assert_eq!(fact.created_at_ms, 7);
        let IntelUpdate::Pass { live_items, .. } = &seen.lock().unwrap()[1] else {
            panic!("expected a pass");
        };
        assert_eq!(live_items.len(), 2);
    }

    #[test]
    fn below_the_threshold_only_analyze_now_runs_a_pass() {
        let backend = Arc::new(add_citing_last_line());
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: TriggerPolicy {
                    min_new_chars: 10_000,
                    ..fast_policy()
                },
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.analyze_now();
        wait_for(&seen, 1);
        assert_eq!(seen.lock().unwrap()[0], IntelUpdate::NothingNew);

        rt.push_final("Them", 0, "short");
        std::thread::sleep(TICK * 2);
        assert_eq!(
            backend.request_count(),
            0,
            "under the threshold, no automatic pass"
        );
        rt.analyze_now();
        wait_for(&seen, 2);
        assert_eq!(backend.request_count(), 1);
        assert_eq!(rt.stop().state.items.len(), 1);
    }

    #[test]
    fn a_failed_pass_is_reported_and_retried_later() {
        let backend = Arc::new(ScriptedBackend::named("scripted"));
        backend.push_err(ReasoningError::Unavailable("not logged in".into()));
        backend.push_ok(json!({"ops": [], "candidates": []}));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "Enough words to cross the threshold.");
        wait_for(&seen, 2);
        let updates = seen.lock().unwrap().clone();
        assert!(matches!(&updates[0], IntelUpdate::Failed(m) if m.contains("not logged in")));
        assert!(matches!(&updates[1], IntelUpdate::Pass { applied: 0, .. }));
        assert_eq!(rt.stop().state.analyzed_through, Some(0));
    }

    #[test]
    fn stop_cancels_a_pass_in_flight_without_reporting_it() {
        let backend = Arc::new(ScriptedBackend::with_responder("slow", |_| {
            std::thread::sleep(Duration::from_millis(300));
            Err(ReasoningError::Cancelled)
        }));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "Enough words to cross the threshold.");
        let deadline = Instant::now() + Duration::from_secs(5);
        while backend.request_count() == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let done = rt.stop();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(done.state.analyzed_through, None);
        assert_eq!(done.lines, 1);
    }

    #[test]
    fn pushing_never_waits_for_a_slow_pass() {
        let backend = Arc::new(ScriptedBackend::with_responder("slow", |_| {
            std::thread::sleep(Duration::from_millis(400));
            Ok(json!({"ops": [], "candidates": []}))
        }));
        let (_seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "Enough words to cross the threshold.");
        while backend.request_count() == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        for i in 0..1000 {
            rt.push_final("Them", i, "more words while the model thinks");
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(rt.stop().lines, 1001);
    }

    #[test]
    fn the_retriever_gets_the_new_text_and_its_snippets_reach_the_request() {
        struct Fixed(Arc<Mutex<Vec<String>>>);
        impl Retriever for Fixed {
            fn retrieve(&self, text: &str) -> Vec<Snippet> {
                self.0.lock().unwrap().push(text.to_owned());
                vec![Snippet {
                    ref_id: "S3:C0".into(),
                    origin: wisp_library::SnippetOrigin::Source {
                        source_id: 3,
                        chunk_idx: 0,
                        label: "spec.md".into(),
                        line_start: Some(1),
                    },
                    text: "EU only".into(),
                    score: 1.0,
                }]
            }
        }
        let queries = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(ScriptedBackend::with_responder("s", |_| {
            Ok(json!({"ops": [], "candidates": []}))
        }));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(Fixed(queries.clone())),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "We host everything in Azure US East.");
        wait_for(&seen, 1);
        rt.stop();
        assert_eq!(
            queries.lock().unwrap()[0],
            "We host everything in Azure US East."
        );
        let ctx = backend.requests.lock().unwrap()[0].context.clone();
        assert!(ctx.contains("[D3:C0] spec.md, line 1\nEU only"));
    }

    #[test]
    fn candidates_pass_the_filter_into_cards_and_dismissals_reach_the_log() {
        let strong = |title: &str| {
            json!({"kind": "missing_owner", "title": title, "detail": "", "suggested_question": null,
                   "source_refs": ["T0"], "related_items": [], "importance": 0.9, "urgency": 0.9,
                   "confidence": 0.9, "future_work_risk": 0.9})
        };
        let mut weak = strong("They seem interested");
        weak["confidence"] = json!(0.3);
        let backend = Arc::new(ScriptedBackend::named("scripted"));
        backend
            .push_ok(json!({"ops": [], "candidates": [strong("Test dataset has no owner"), weak]}));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: fast_policy(),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 1_000),
        );
        rt.push_final("Them", 0, "We'll send a test dataset at some point.");
        wait_for(&seen, 1);
        let IntelUpdate::Pass { cards, .. } = seen.lock().unwrap()[0].clone() else {
            panic!("expected a pass");
        };
        assert_eq!(cards.len(), 1, "the weak candidate is held back");
        assert_eq!(cards[0].id, "CARD-1");
        assert_eq!(cards[0].candidate.source_refs, ["Mlive:T0"]);
        rt.dismiss("CARD-1");
        let done = rt.stop();
        assert_eq!(done.candidate_log.len(), 3, "two considered, one dismissed");
        assert!(matches!(done.candidate_log[2], LogEntry::Dismissed { .. }));
    }

    #[test]
    fn wrap_up_runs_the_audit_at_once_and_shapes_later_passes() {
        let backend = Arc::new(ScriptedBackend::with_responder("scripted", |req| {
            Ok(match req.task {
                wisp_reasoning::TaskKind::EndgameAudit => json!({"gaps": [
                    {"category": "missing", "text": "Nobody owns deployment.", "source_refs": [], "related_items": []}
                ]}),
                _ => json!({"ops": [], "candidates": []}),
            })
        }));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend.clone(),
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: TriggerPolicy {
                    min_new_chars: 10_000,
                    ..fast_policy()
                },
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "We talked about the migration.");
        rt.wrap_up();
        wait_for(&seen, 1);
        let IntelUpdate::Audit { report, .. } = seen.lock().unwrap()[0].clone() else {
            panic!("expected an audit");
        };
        assert_eq!(report.gaps[0].text, "Nobody owns deployment.");
        rt.analyze_now();
        wait_for(&seen, 2);
        let requests = backend.requests.lock().unwrap().clone();
        assert_eq!(requests[0].task, wisp_reasoning::TaskKind::EndgameAudit);
        assert!(requests[1]
            .context
            .contains("## The meeting is wrapping up"));
        drop(requests);
        rt.stop();
    }

    #[test]
    fn closing_words_suggest_wrapping_up_once() {
        let backend = Arc::new(ScriptedBackend::with_responder("s", |_| {
            Ok(json!({"ops": [], "candidates": []}))
        }));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend,
            Box::new(NoRetrieval),
            RuntimeConfig {
                policy: TriggerPolicy {
                    min_new_chars: 10_000,
                    ..fast_policy()
                },
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 0),
        );
        rt.push_final("Them", 0, "Anything else before we wrap?");
        rt.push_final("You", 1, "No, thanks everyone.");
        wait_for(&seen, 1);
        rt.push_final("Them", 2, "Thanks all, talk soon.");
        std::thread::sleep(TICK * 2);
        let updates = seen.lock().unwrap().clone();
        assert_eq!(
            updates,
            [IntelUpdate::WrapSuggested(EndgameTrigger::Semantic)]
        );
        rt.stop();
    }

    #[test]
    fn a_near_scheduled_end_suggests_wrapping_up() {
        let backend = Arc::new(ScriptedBackend::named("s"));
        let (seen, on_update) = collect();
        let rt = IntelRuntime::spawn(
            backend,
            Box::new(NoRetrieval),
            RuntimeConfig {
                scheduled_end_ms: Some(10 * 60 * 1000),
                ..RuntimeConfig::default()
            },
            on_update,
            Box::new(|| 6 * 60 * 1000),
        );
        wait_for(&seen, 1);
        assert_eq!(
            seen.lock().unwrap()[0],
            IntelUpdate::WrapSuggested(EndgameTrigger::Scheduled)
        );
        rt.stop();
    }

    #[test]
    fn saved_positions_follow_start_order_stably() {
        // Arrival order: system line at 5s, mic line that started at 3s, then 5s again, then 9s.
        assert_eq!(saved_positions(&[5000, 3000, 5000, 9000]), [1, 0, 2, 3]);
        assert!(saved_positions::<i64>(&[]).is_empty());
    }

    #[test]
    fn remapping_rewrites_every_ref_and_keeps_the_rest() {
        let log = vec![
            AppliedOp {
                seq: 0,
                at_ms: 1,
                op: ResolvedOp::Add {
                    id: "COM-1".into(),
                    kind: ItemKind::Commitment,
                    text: "x".into(),
                    status: crate::model::EpistemicStatus::Stated,
                    confidence: 0.9,
                    source_refs: vec!["Mlive:T0".into(), "S3:C0".into()],
                    related_items: vec![],
                    owner: None,
                    due: None,
                },
            },
            AppliedOp {
                seq: 1,
                at_ms: 2,
                op: ResolvedOp::SetLifecycle {
                    id: "COM-1".into(),
                    lifecycle: Lifecycle::Resolved,
                    superseded_by: None,
                    add_refs: vec!["Mlive:T1".into()],
                },
            },
        ];
        let saved = saved_positions(&[5000, 3000]);
        let out = remap_refs(&log, |r| {
            let a: usize = r.strip_prefix("Mlive:T")?.parse().ok()?;
            Some(wisp_library::meeting_ref("m-42", saved[a]))
        });
        let state = MeetingState::replay("m-42", &out).unwrap();
        assert_eq!(
            state.item("COM-1").unwrap().source_refs,
            ["Mm-42:T1", "S3:C0", "Mm-42:T0"]
        );
    }
}

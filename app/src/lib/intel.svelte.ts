import { copyText } from "$lib/clipboard";
// Meeting intelligence on the webview side: the live state as `intel://update` reports it, and the
// Ask conversation. Module-level, so nothing is lost while the panel is closed; reset per session.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ItemKind =
  | "participant"
  | "requirement"
  | "constraint"
  | "fact"
  | "decision"
  | "assumption"
  | "commitment"
  | "open_question"
  | "risk"
  | "topic"
  | "artifact"
  | "conflict"
  | "task_candidate"
  | "objective";

export const KIND_ORDER: ItemKind[] = [
  "conflict",
  "open_question",
  "commitment",
  "decision",
  "requirement",
  "constraint",
  "risk",
  "assumption",
  "fact",
  "task_candidate",
  "objective",
  "participant",
  "artifact",
  "topic",
];

export type StateItem = {
  id: string;
  kind: ItemKind;
  text: string;
  status: "stated" | "inferred" | "suggested";
  confidence: number;
  lifecycle: "active" | "resolved" | "superseded" | "withdrawn" | "uncertain";
  source_refs: string[];
  related_items: string[];
  owner: string | null;
  due: string | null;
};

export type Card = {
  id: string;
  score: number;
  shownAtMs: number;
  candidate: {
    kind: string;
    /** At most six words, for a glance. */
    headline: string;
    title: string;
    detail: string;
    suggestedQuestion: string | null;
    cited: string[];
    confidence: number;
  };
};

export type GapCategory =
  | "missing"
  | "clarify"
  | "commitment_without_owner"
  | "commitment_without_date"
  | "weakened_promise"
  | "owed_by_them"
  | "owed_by_you"
  | "conflict";

export const GAP_ORDER: GapCategory[] = [
  "missing",
  "clarify",
  "commitment_without_owner",
  "commitment_without_date",
  "weakened_promise",
  "owed_by_them",
  "owed_by_you",
  "conflict",
];

export type Gap = { category: GapCategory; headline: string; text: string; cited: string[] };

export type FollowUpClass = "mine" | "theirs" | "open_question" | "not_a_task" | "project_memory";
export const CLASS_ORDER: FollowUpClass[] = ["mine", "theirs", "open_question", "not_a_task", "project_memory"];
export type FollowUp = {
  n: number;
  text: string;
  class: FollowUpClass;
  owner: string | null;
  due: string | null;
  itemId: string | null;
};

export type Project = { id: string; name: string; created_at_ms: number };

/** A hand change to an item: fields left out stay; an empty owner or due clears it. */
export type ItemChange = {
  text?: string;
  owner?: string;
  due?: string;
  lifecycle?: StateItem["lifecycle"];
};
/** An item the user added to a project by hand. */
export type ProjectItem = {
  id: string;
  projectId: string;
  kind: ItemKind;
  text: string;
  owner: string | null;
  due: string | null;
  lifecycle: StateItem["lifecycle"];
};
/** One item of a project's overview: from a meeting's state (`meeting`), or added by hand (`manualId`). */
export type OverviewItem = {
  kind: ItemKind;
  text: string;
  owner: string | null;
  due: string | null;
  lifecycle: StateItem["lifecycle"];
  meeting: { id: string; title: string; when: string; startedAtMs: number; itemId: string } | null;
  manualId: string | null;
};
export type ProjectOverview = {
  commitments: OverviewItem[];
  openQuestions: OverviewItem[];
  decisions: OverviewItem[];
  risks: OverviewItem[];
  done: OverviewItem[];
};
/** Kinds whose items can be marked done. */
export const RESOLVABLE: ItemKind[] = ["commitment", "task_candidate", "open_question", "risk"];
export type ProvenanceRef = { sourceRef: string; label: string; sha256: string };
export type MemoryItem = {
  id: number;
  kind: string;
  text: string;
  status: string;
  confidence: number;
  provenance: ProvenanceRef[];
  expired: boolean[];
};
export type Proposal = {
  kind: string;
  text: string;
  status: "stated" | "inferred";
  confidence: number;
  provenance: ProvenanceRef[];
  accepted: boolean;
};

type IntelUpdate =
  | { kind: "wrapSuggested"; trigger: "scheduled" | "semantic" | "manual" }
  | { kind: "audit"; gaps: Gap[]; rejected: number; markdown: string; backend: string }
  | {
      kind: "pass";
      applied: number;
      rejected: number;
      items: StateItem[];
      remainingLines: number;
      backend: string;
      cards: Card[];
    }
  | { kind: "nothingNew" }
  | { kind: "speakerNames"; suggestions: SpeakerSuggestion[] }
  | { kind: "failed"; message: string };

/** "Speaker 2 is probably Laurie", from the transcript. `speakerId` is 0-based. */
export type SpeakerSuggestion = {
  speaker: string;
  speakerId: number;
  name: string;
  confidence: number;
  evidence: string[];
  quote: string | null;
};

export type Citation = { id: string; label: string; text: string; sourceRef: string | null; itemId: string | null };
export type AskAnswer = {
  /** At most twelve words, shown first. */
  short: string;
  answer: string;
  citations: Citation[];
  unknownCitations: string[];
  grounded: boolean;
  markdown: string;
};
export type AskTurn = { question: string; answer?: AskAnswer; error?: string; pending: boolean };

export const intel = $state({
  items: [] as StateItem[],
  lastPass: null as null | { applied: number; rejected: number; backend: string },
  note: "" as "" | "nothingNew",
  error: "",
  analyzing: false,
  turns: [] as AskTurn[],
  cards: [] as Card[],
  /** Cards shown since the Insights view was last open. */
  unseen: 0,
  tab: "insights" as "insights" | "ask" | "state" | "review" | "summary",
  /** The just-saved meeting a post-call review can run on. */
  savedMeetingId: "",
  /** When the current live meeting started (epoch ms), for exports. */
  startedAt: 0,
  /** The just-saved meeting's summary, once made. */
  summary: "",
  review: null as null | { followups: FollowUp[]; source: "model" | "state"; note: string | null },
  reviewBusy: false,
  reviewError: "",
  reviewUnderstood: [] as { n: number; class: FollowUpClass }[],
  reviewApplied: null as null | number,
  projects: [] as Project[],
  /** The project new meetings are filed under ("" for none). Persists per device. */
  projectId: (() => {
    try {
      return localStorage.getItem("wisp.project") ?? "";
    } catch {
      return "";
    }
  })(),
  /** The project the just-saved meeting was filed under. */
  savedProjectId: "",
  memory: [] as MemoryItem[],
  proposals: null as null | Proposal[],
  learningBusy: false,
  learningError: "",
  learningSaved: null as null | number,
  /** An advisory "looks like you're wrapping up", until acted on or dismissed. */
  wrapSuggested: null as null | "scheduled" | "semantic" | "manual",
  endgame: false,
  auditing: false,
  audit: null as null | { gaps: Gap[]; rejected: number; markdown: string },
  scheduledEnd: "",
  // Whether the user typed `scheduledEnd` (vs. the assumed slot). Only a typed time carries into
  // the next Start, and Stop clears it.
  scheduledEndTyped: false,
  /** Screenshots attached to this meeting. */
  context: [] as ContextShot[],
  contextBusy: false,
  contextError: "",
  /** Live speaker-name suggestions, one per speaker at most. */
  speakerSuggestions: [] as SpeakerSuggestion[],
});

/** Suggestions dismissed this meeting, so one already in flight doesn't come back. */
let dismissedSpeakerNames = new Set<string>();
const speakerKey = (s: { speaker: string; name: string }) => `${s.speaker}\u0000${s.name.toLowerCase()}`;

/** Drops a speaker-name suggestion and tells the runtime not to offer it again this meeting. */
export async function dismissSpeakerSuggestion(s: SpeakerSuggestion) {
  dismissedSpeakerNames.add(speakerKey(s));
  intel.speakerSuggestions = intel.speakerSuggestions.filter((x) => x.speaker !== s.speaker);
  await invoke("intel_dismiss_speaker_name", { speaker: s.speaker, name: s.name }).catch(() => {});
}

/** Drops the suggestion for a speaker the user just named. */
export function clearSpeakerSuggestion(speakerId: number) {
  intel.speakerSuggestions = intel.speakerSuggestions.filter((x) => x.speakerId !== speakerId);
}

export type ContextShot = {
  sourceId: number;
  label: string;
  title: string | null;
  status: "describing" | "described" | "undescribed";
  note: string | null;
};

/** Shown next to Capture context; the backend registers it only during a meeting. */
export const CAPTURE_SHORTCUT = "⌘⌥⇧C";

async function attaching(run: () => Promise<unknown>) {
  intel.contextBusy = true;
  intel.contextError = "";
  try {
    await run();
  } catch (e) {
    intel.contextError = String(e);
  } finally {
    intel.contextBusy = false;
  }
}

/** Capture Context: the system's region/window screenshot. */
export const captureContext = () => attaching(() => invoke("capture_context"));
/** Attach an image file the user picks. */
export const importContext = () => attaching(() => invoke("import_context_image"));
/** Attach a pasted image. */
export const pasteContext = (image: Blob) =>
  attaching(async () => invoke("paste_context_image", new Uint8Array(await image.arrayBuffer())));
export const describeContext = (sourceId: number) => attaching(() => invoke("describe_context", { sourceId }));
export const removeContext = (sourceId: number) => attaching(() => invoke("remove_context", { sourceId }));

let listening: Promise<unknown> | null = null;
let listeningContext: Promise<unknown> | null = null;

export type ExportKind = "summary" | "record" | "transcript" | "packet" | "json";
export const EXPORT_KINDS: ExportKind[] = ["summary", "record", "transcript", "packet", "json"];

/** Which meeting an export is of: `id` null means the live (or just-stopped, unsaved) one. `when`
 *  is the display date; `title` overrides the stored one. */
export type ExportTarget = { id: string | null; title: string | null; when: string };

/** The live (or just-stopped) meeting while recording or when nothing was saved; else the saved one. */
export function liveExportTarget(running: boolean, liveTitle: (when: string) => string): ExportTarget {
  const when = new Date(intel.startedAt || Date.now()).toLocaleString();
  const id = !running && intel.savedMeetingId ? intel.savedMeetingId : null;
  return { id, title: id ? null : liveTitle(when), when };
}

const EXPORT_FILE: Record<ExportKind, string> = {
  summary: "summary",
  record: "record",
  transcript: "transcript",
  packet: "context",
  json: "state",
};

/** Copies a meeting export to the clipboard. */
export async function copyMeetingExport(target: ExportTarget, kind: ExportKind) {
  await copyText(await invoke<string>("intel_export", { ...target, kind }));
}

/** Saves a meeting export to a file the user picks. `false` on cancel. */
export async function saveMeetingExport(target: ExportTarget, kind: ExportKind, startedAt: number): Promise<boolean> {
  const stamp = new Date(startedAt || Date.now()).toISOString().slice(0, 10);
  return invoke<boolean>("intel_export_save", {
    ...target,
    kind,
    defaultName: `meeting-${EXPORT_FILE[kind]}-${stamp}`,
  });
}

/** Summarizes a saved meeting and stores the summary on it, replacing any earlier one. */
export async function summarizeMeeting(id: string, when: string) {
  return invoke<{ markdown: string; fromTranscript: boolean }>("meeting_summarize", { id, when });
}

/** Starts listening for pass results (once per page load). */
export function ensureIntelListener(): Promise<unknown> {
  listeningContext ??= listen<ContextShot[]>("intel://context", (e) => {
    intel.context = e.payload;
  });
  listening ??= listen<IntelUpdate>("intel://update", (e) => {
    const u = e.payload;
    intel.analyzing = false;
    if (u.kind === "pass") {
      intel.items = u.items;
      if (u.cards.length) {
        intel.cards.push(...u.cards);
        intel.unseen += u.cards.length;
      }
      intel.lastPass = { applied: u.applied, rejected: u.rejected, backend: u.backend };
      intel.error = "";
      intel.note = "";
    } else if (u.kind === "nothingNew") {
      intel.note = "nothingNew";
    } else if (u.kind === "speakerNames") {
      intel.speakerSuggestions = u.suggestions.filter((s) => !dismissedSpeakerNames.has(speakerKey(s)));
    } else if (u.kind === "wrapSuggested") {
      if (!intel.endgame) {
        intel.wrapSuggested = u.trigger;
        intel.unseen += 1;
      }
    } else if (u.kind === "audit") {
      intel.auditing = false;
      intel.audit = { gaps: u.gaps, rejected: u.rejected, markdown: u.markdown };
      intel.unseen += 1;
    } else {
      intel.auditing = false;
      intel.error = u.message;
    }
  });
  return listening;
}

/** Clears state and conversation for a new meeting. */
export function resetIntel() {
  intel.items = [];
  intel.lastPass = null;
  intel.note = "";
  intel.error = "";
  intel.analyzing = false;
  intel.turns = [];
  intel.cards = [];
  intel.unseen = 0;
  intel.tab = "insights";
  intel.wrapSuggested = null;
  intel.endgame = false;
  intel.auditing = false;
  intel.audit = null;
  intel.scheduledEnd = "";
  intel.savedMeetingId = "";
  intel.summary = "";
  intel.context = [];
  intel.contextError = "";
  intel.speakerSuggestions = [];
  dismissedSpeakerNames = new Set();
  intel.review = null;
  intel.reviewBusy = false;
  intel.reviewError = "";
  intel.reviewUnderstood = [];
  intel.reviewApplied = null;
  intel.savedProjectId = "";
  intel.proposals = null;
  intel.learningBusy = false;
  intel.learningError = "";
  intel.learningSaved = null;
}

/** Loads the project list; drops a remembered project that no longer exists. */
export async function loadProjects() {
  try {
    intel.projects = await invoke<Project[]>("list_projects");
    if (intel.projectId && !intel.projects.some((p) => p.id === intel.projectId)) selectProject("");
  } catch {
    intel.projects = [];
  }
}

/** Chooses the project new meetings are filed under. */
export function selectProject(id: string) {
  intel.projectId = id;
  try {
    localStorage.setItem("wisp.project", id);
  } catch {
    // per-device convenience only
  }
}

/** Creates a project and, unless `select` is false, selects it for new meetings. Returns an error
 *  message, or "" on success. */
export async function createProject(name: string, select = true): Promise<string> {
  try {
    const p = await invoke<Project>("create_project", { name });
    intel.projects = [...intel.projects, p].sort((a, b) => a.name.localeCompare(b.name));
    if (select) selectProject(p.id);
    return "";
  } catch (e) {
    return String(e);
  }
}

/** Renames a project. Returns an error message, or "" on success. */
export async function renameProject(id: string, name: string): Promise<string> {
  try {
    await invoke<boolean>("rename_project", { id, name });
    await loadProjects();
    return "";
  } catch (e) {
    return String(e);
  }
}

/** Loads the selected project's accepted knowledge. */
export async function loadMemory() {
  if (!intel.projectId) {
    intel.memory = [];
    return;
  }
  try {
    intel.memory = await invoke<MemoryItem[]>("list_project_memory", { projectId: intel.projectId });
  } catch {
    intel.memory = [];
  }
}

export async function deleteMemory(id: number) {
  await invoke("delete_project_memory", { id }).catch(() => {});
  intel.memory = intel.memory.filter((m) => m.id !== id);
}

/** Proposes what the saved meeting's project should remember. */
export async function proposeLearning() {
  intel.learningBusy = true;
  intel.learningError = "";
  intel.learningSaved = null;
  try {
    intel.proposals = await invoke<Proposal[]>("intel_learning_propose", { id: intel.savedMeetingId });
  } catch (e) {
    intel.learningError = String(e);
  } finally {
    intel.learningBusy = false;
  }
}

/** Stores the accepted proposals in the project. */
export async function saveLearning() {
  if (!intel.proposals) return;
  intel.learningBusy = true;
  try {
    intel.learningSaved = await invoke<number>("intel_learning_save", {
      id: intel.savedMeetingId,
      proposals: intel.proposals,
    });
    intel.proposals = null;
    await loadMemory();
  } catch (e) {
    intel.learningError = String(e);
  } finally {
    intel.learningBusy = false;
  }
}

/** Proposes the saved meeting's follow-ups. */
export async function startReview() {
  if (!intel.savedMeetingId) return;
  intel.reviewBusy = true;
  intel.reviewError = "";
  intel.reviewApplied = null;
  try {
    intel.review = await invoke("intel_review_start", { id: intel.savedMeetingId });
  } catch (e) {
    intel.reviewError = String(e);
  } finally {
    intel.reviewBusy = false;
  }
}

/** Applies a plain-words correction to the follow-ups. */
export async function replyReview(reply: string) {
  if (!intel.review || !reply.trim()) return;
  intel.reviewBusy = true;
  intel.reviewError = "";
  try {
    const r = await invoke<{ followups: FollowUp[]; understood: { n: number; class: FollowUpClass }[] }>(
      "intel_review_reply",
      { reply },
    );
    intel.review.followups = r.followups;
    intel.reviewUnderstood = r.understood;
  } catch (e) {
    intel.reviewError = String(e);
  } finally {
    intel.reviewBusy = false;
  }
}

/** Sets one follow-up's class. */
export async function setFollowUpClass(n: number, cls: FollowUpClass) {
  if (!intel.review) return;
  try {
    intel.review.followups = await invoke<FollowUp[]>("intel_review_set", { n, class: cls });
  } catch (e) {
    intel.reviewError = String(e);
  }
}

/** Applies the reviewed follow-ups to the meeting's state. Any marked "Save to project" then go
 *  straight to the project-knowledge step, since applying alone stores nothing for them. */
export async function applyReview() {
  const toProject = intel.review?.followups.some((f) => f.class === "project_memory") ?? false;
  intel.reviewBusy = true;
  let applied = false;
  try {
    intel.reviewApplied = await invoke<number>("intel_review_apply");
    intel.review = null;
    intel.reviewUnderstood = [];
    applied = true;
  } catch (e) {
    intel.reviewError = String(e);
  } finally {
    intel.reviewBusy = false;
  }
  if (applied && toProject && intel.savedProjectId && !intel.proposals) await proposeLearning();
}

/** Wrapping Up: enter endgame and audit what's still open. Recording goes on. */
export async function wrapUp(): Promise<boolean> {
  intel.endgame = true;
  intel.wrapSuggested = null;
  intel.auditing = true;
  intel.tab = "insights";
  try {
    const running = await invoke<boolean>("intel_wrap_up");
    if (!running) intel.auditing = false;
    return running;
  } catch (e) {
    intel.auditing = false;
    intel.error = String(e);
    return false;
  }
}

/** Sets the meeting's scheduled end from a local "HH:MM" today (blank clears it). */
export function setScheduledEnd(hhmm: string) {
  intel.scheduledEnd = hhmm;
  let endMs: number | null = null;
  const m = /^(\d{1,2}):(\d{2})$/.exec(hhmm);
  if (m) {
    const d = new Date();
    d.setHours(Number(m[1]), Number(m[2]), 0, 0);
    // A time well before now means tomorrow (a meeting running past midnight).
    if (d.getTime() < Date.now() - 12 * 60 * 60 * 1000) d.setDate(d.getDate() + 1);
    endMs = d.getTime();
  }
  invoke("intel_set_scheduled_end", { endMs }).catch(() => {});
}

const MEETING_MINUTES_KEY = "wisp.meetingMinutes";

/** The assumed meeting length when no end time is given: 30 or 60 minutes (default 60). */
export function meetingMinutes(): 30 | 60 {
  try {
    return localStorage.getItem(MEETING_MINUTES_KEY) === "30" ? 30 : 60;
  } catch {
    return 60;
  }
}

export function setMeetingMinutes(minutes: 30 | 60) {
  try {
    localStorage.setItem(MEETING_MINUTES_KEY, String(minutes));
  } catch {
    // per-device convenience only
  }
}

/** A guess at when a meeting that started at `startMs` ends, as "HH:MM": the start rounded to the
 *  nearest :00 or :30 (calendar slots; people join a little early or late), plus `minutes`. */
export function defaultMeetingEnd(startMs: number, minutes: number = meetingMinutes()): string {
  const half = 30 * 60 * 1000;
  const d = new Date(Math.round(startMs / half) * half + minutes * 60 * 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** Hides a card and tells the filter, so it holds back repeats. */
export function dismissCard(id: string) {
  intel.cards = intel.cards.filter((c) => c.id !== id);
  invoke("intel_dismiss_card", { id }).catch(() => {});
}

/** Asks the runtime for a pass now. `false` if intelligence isn't running. */
export async function analyzeNow(): Promise<boolean> {
  intel.analyzing = true;
  intel.note = "";
  try {
    const running = await invoke<boolean>("intel_analyze_now");
    if (!running) intel.analyzing = false;
    return running;
  } catch (e) {
    intel.analyzing = false;
    intel.error = String(e);
    return false;
  }
}

/** Asks a question, sending the finished turns as history. */
export async function askQuestion(question: string) {
  const q = question.trim();
  if (!q) return;
  const history = intel.turns
    .filter((t) => t.answer)
    .map((t) => ({ question: t.question, answer: t.answer!.answer }));
  intel.turns.push({ question: q, pending: true });
  const turn = intel.turns[intel.turns.length - 1];
  try {
    turn.answer = await invoke<AskAnswer>("intel_ask", { question: q, history });
  } catch (e) {
    turn.error = String(e);
  } finally {
    turn.pending = false;
  }
}

/** Cancels the question being answered. */
export function cancelAsk() {
  invoke("intel_ask_cancel").catch(() => {});
}

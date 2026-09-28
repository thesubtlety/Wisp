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

export type Gap = { category: GapCategory; text: string; cited: string[] };

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
  | { kind: "failed"; message: string };

export type Citation = { id: string; label: string; text: string; sourceRef: string | null; itemId: string | null };
export type AskAnswer = {
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
  tab: "insights" as "insights" | "ask" | "state" | "review",
  /** The just-saved meeting a post-call review can run on. */
  savedMeetingId: "",
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
});

let listening: Promise<unknown> | null = null;

/** Starts listening for pass results (once per page load). */
export function ensureIntelListener(): Promise<unknown> {
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

/** Creates a project and selects it. Returns an error message, or "" on success. */
export async function createProject(name: string): Promise<string> {
  try {
    const p = await invoke<Project>("create_project", { name });
    intel.projects = [...intel.projects, p];
    selectProject(p.id);
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

/** Applies the reviewed follow-ups to the meeting's state. */
export async function applyReview() {
  intel.reviewBusy = true;
  try {
    intel.reviewApplied = await invoke<number>("intel_review_apply");
    intel.review = null;
    intel.reviewUnderstood = [];
  } catch (e) {
    intel.reviewError = String(e);
  } finally {
    intel.reviewBusy = false;
  }
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
    endMs = d.getTime();
  }
  invoke("intel_set_scheduled_end", { endMs }).catch(() => {});
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

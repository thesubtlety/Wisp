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

type IntelUpdate =
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
    } else {
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

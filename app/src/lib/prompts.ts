// The prompt library's backend surface (app/src-tauri/src/prompts.rs), plus a small Markdown reader
// for showing a prompt's output without injecting HTML.

import { invoke } from "@tauri-apps/api/core";

export type PromptScope = "meeting" | "speaker";

/** A saved prompt. Built-ins can be edited and reset, not deleted. */
export type Prompt = {
  id: string;
  name: string;
  body: string;
  scope: PromptScope;
  builtin: boolean;
  /** A built-in whose text differs from the shipped one. */
  customized: boolean;
  updatedAtMs: number;
};

/** A stored output of a prompt run over a saved meeting. */
export type PromptRun = {
  id: number;
  meetingId: string;
  promptName: string;
  speaker: string | null;
  output: string;
  backend: string;
  atMs: number;
};

/** Variables a prompt body may use, in the order the editor offers them. */
export const PROMPT_VARIABLES = ["speaker", "title", "date", "about_me", "project"] as const;

export const listPrompts = () => invoke<Prompt[]>("list_prompts");
export const createPrompt = (name: string, body: string, scope: PromptScope) =>
  invoke<Prompt>("create_prompt", { name, body, scope });
export const updatePrompt = (id: string, name: string, body: string, scope: PromptScope) =>
  invoke<Prompt>("update_prompt", { id, name, body, scope });
export const deletePrompt = (id: string) => invoke<void>("delete_prompt", { id });
export const resetPrompt = (id: string) => invoke<Prompt>("reset_prompt", { id });
export const listPromptRuns = (meetingId: string) => invoke<PromptRun[]>("list_prompt_runs", { meetingId });
export const deletePromptRun = (id: number) => invoke<void>("delete_prompt_run", { id });
/** Saves `text` as a Markdown file the user picks; `false` when they cancel. */
export const savePromptOutput = (text: string, defaultName: string) =>
  invoke<boolean>("save_prompt_output", { text, defaultName });

/**
 * Runs a prompt. With `meetingId`, over that saved meeting (the backend reads its transcript and
 * stores the run); otherwise over the live `transcript` (not stored). Returns the output text.
 */
export function runPrompt(args: {
  promptId: string;
  speaker?: string;
  meetingId?: string;
  transcript?: string;
  title?: string;
  date?: string;
}): Promise<string> {
  return invoke<string>("run_prompt", {
    promptId: args.promptId,
    speaker: args.speaker ?? null,
    meetingId: args.meetingId ?? null,
    transcript: args.transcript ?? null,
    title: args.title ?? null,
    date: args.date ?? null,
  });
}

/** A run of inline text: plain, **bold**, or `code`. */
export type Inline = { text: string; bold?: boolean; code?: boolean };

/** One displayed block of a Markdown-ish reply. */
export type Block =
  | { kind: "heading"; level: number; parts: Inline[] }
  | { kind: "item"; ordered: string | null; checked: boolean | null; parts: Inline[] }
  | { kind: "table"; head: Inline[][]; rows: Inline[][][] }
  | { kind: "para"; parts: Inline[] }
  | { kind: "gap" };

/** Splits `**bold**` and `` `code` `` out of one line. Unclosed markers stay as text. */
export function parseInline(line: string): Inline[] {
  const parts: Inline[] = [];
  const re = /\*\*([^*]+)\*\*|`([^`]+)`/g;
  let last = 0;
  for (const m of line.matchAll(re)) {
    if (m.index! > last) parts.push({ text: line.slice(last, m.index) });
    if (m[1] !== undefined) parts.push({ text: m[1], bold: true });
    else parts.push({ text: m[2], code: true });
    last = m.index! + m[0].length;
  }
  if (last < line.length) parts.push({ text: line.slice(last) });
  return parts;
}

/** Reads the Markdown a model usually writes: headings, lists, checklists, tables, paragraphs. */
export function parseMarkdown(text: string): Block[] {
  const blocks: Block[] = [];
  for (const raw of text.split("\n")) {
    const trimmed = raw.trim();
    if (trimmed === "") {
      if (blocks.length && blocks[blocks.length - 1].kind !== "gap") blocks.push({ kind: "gap" });
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(trimmed);
    if (heading) {
      blocks.push({ kind: "heading", level: heading[1].length, parts: parseInline(heading[2]) });
      continue;
    }
    if (trimmed.startsWith("|")) {
      const cells = trimmed.replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      if (cells.every((c) => /^:?-{2,}:?$/.test(c))) continue; // the header separator
      const last = blocks[blocks.length - 1];
      if (last?.kind === "table") last.rows.push(cells.map(parseInline));
      else blocks.push({ kind: "table", head: cells.map(parseInline), rows: [] });
      continue;
    }
    const item = /^(?:[-*+]|(\d+)[.)])\s+(?:\[([ xX])\]\s+)?(.*)$/.exec(trimmed);
    if (item) {
      blocks.push({
        kind: "item",
        ordered: item[1] ?? null,
        checked: item[2] === undefined ? null : item[2] !== " ",
        parts: parseInline(item[3]),
      });
      continue;
    }
    blocks.push({ kind: "para", parts: parseInline(trimmed) });
  }
  return blocks;
}

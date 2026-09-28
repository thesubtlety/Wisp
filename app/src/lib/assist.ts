// The AI assist's backend surface: every Tauri command and event the assist uses, in one place, so the
// assist UI calls named functions rather than raw command strings. The backend side lives in
// app/src-tauri/src/assist/ (chat.rs for the chat tasks, realtime.rs for the realtime assist).

import { invoke } from "@tauri-apps/api/core";
import type { ParamSpec, ParamValue } from "$lib/cloud.svelte";

/** A chunk of the in-progress reply, to append to the feed as it streams. */
export const ASSIST_DELTA_EVENT = "assist://delta";
/** The finished reply; it closes one that streamed in via {@link ASSIST_DELTA_EVENT}. */
export const ASSIST_TEXT_EVENT = "assist://text";
/** A realtime-assist failure (bad key/model, server error, dropped socket). */
export const ASSIST_ERROR_EVENT = "assist://error";

/**
 * Run a one-shot LLM task over `transcript` using the chat model of a provider (a custom endpoint —
 * the user's gateway, a local Ollama, or OpenAI). `systemPrompt` steers the task (summary, action
 * items, or a custom prompt). Returns the assistant's reply; throws (with the backend's message) on
 * a missing key or an HTTP error.
 */
export async function runLlmTask(provider: string, model: string, systemPrompt: string, transcript: string, params: Record<string, ParamValue> = {}): Promise<string> {
  return await invoke<string>("run_llm_task", { provider, model, systemPrompt, transcript, params });
}

/**
 * Streaming variant of {@link runLlmTask}: the reply streams back via `assist://delta` events (and
 * closes on `assist://text`), so the caller's feed fills token-by-token instead of waiting for the
 * whole reply. Resolves when the reply completes; throws (with the backend's message) on error.
 */
export async function runAssistStream(provider: string, model: string, systemPrompt: string, transcript: string, params: Record<string, ParamValue> = {}): Promise<void> {
  await invoke("run_assist_stream", { provider, model, systemPrompt, transcript, params });
}

/** The advanced parameter specs the AI assist exposes (temperature / top_p / max reply tokens). The
 *  same generic <ParamsPanel> renders them; empty on error so the panel just shows nothing. Vendor-
 *  agnostic — every assist provider speaks the same OpenAI-compatible chat tuning. */
export async function assistParams(): Promise<ParamSpec[]> {
  try {
    return await invoke<ParamSpec[]>("assist_params");
  } catch {
    return [];
  }
}

/** The advanced parameter specs the **realtime** assist exposes (turn-detection + noise reduction) —
 *  the realtime counterpart of {@link assistParams}, rendered by the same generic <ParamsPanel>. */
export async function assistRealtimeParams(): Promise<ParamSpec[]> {
  try {
    return await invoke<ParamSpec[]>("assist_realtime_params");
  } catch {
    return [];
  }
}

/** Start the realtime assist over the running live session's audio, answering under `instructions`.
 *  Resolves once the WebSocket handshake succeeds; throws (with the backend's message) otherwise. */
export async function startAssistRealtime(
  provider: string,
  model: string,
  instructions: string,
  params: Record<string, ParamValue>,
): Promise<void> {
  await invoke("start_assist_realtime", { provider, model, instructions, params });
}

/** Stop the realtime assist. The live session keeps running, so the assist can be started again. */
export async function stopAssistRealtime(): Promise<void> {
  await invoke("stop_assist_realtime");
}

/** Ask the running realtime assist for a reply now, instead of waiting for its throttle. */
export async function assistHintNow(): Promise<void> {
  await invoke("assist_hint_now");
}

<script lang="ts">
  // A saved meeting's summary: shown as simple Markdown, made (or remade) with one reasoning call
  // over the meeting's state. The result is stored on the meeting and handed back via `onSummary`.
  import { i18n } from "$lib/i18n.svelte";
  import { summarizeMeeting } from "$lib/intel.svelte";
  import Markdown from "$lib/Markdown.svelte";

  let {
    meetingId,
    when,
    summary,
    hasState,
    onSummary,
  }: {
    meetingId: string;
    /** The meeting's date, for the prompt. */
    when: string;
    summary: string;
    /** Whether the meeting has state; without it the summary comes from the transcript's end. */
    hasState: boolean;
    onSummary: (markdown: string, meetingId: string) => void;
  } = $props();

  let busy = $state(false);
  let error = $state("");

  async function run() {
    busy = true;
    error = "";
    try {
      const id = meetingId;
      onSummary((await summarizeMeeting(id, when)).markdown, id);
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="summary-view">
  <div class="head">
    <button class="btn" class:primary={!summary} disabled={busy} onclick={run}>
      {busy ? i18n.t.intel.summarizing : summary ? i18n.t.intel.regenerate : i18n.t.intel.summarize}
    </button>
    {#if !hasState}<span class="note">{i18n.t.intel.summaryFromTranscript}</span>{/if}
  </div>
  {#if error}<p class="error">{i18n.t.intel.summaryFailed(error)}</p>{/if}
  {#if summary}
    <Markdown md={summary} />
  {:else if !busy}
    <p class="note">{i18n.t.intel.summaryEmpty}</p>
  {/if}
</div>

<style>
  .summary-view {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }

  .btn {
    font: inherit;
    font-size: 12px;
    color: var(--text);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 5px 10px;
    cursor: pointer;
  }

  .btn.primary {
    color: var(--accent);
    border-color: var(--accent);
  }

  .btn:hover:not(:disabled) {
    border-color: var(--accent);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .note {
    margin: 0;
    font-size: 12px;
    color: var(--muted);
  }

  .error {
    margin: 0;
    font-size: 12px;
    color: var(--danger, #c0392b);
  }
</style>

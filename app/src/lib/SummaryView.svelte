<script lang="ts">
  // A saved meeting's summary: shown as simple Markdown, made (or remade) with one reasoning call
  // over the meeting's state. The result is stored on the meeting and handed back via `onSummary`.
  import { i18n } from "$lib/i18n.svelte";
  import { summarizeMeeting } from "$lib/intel.svelte";

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

  type Span = { text: string; bold: boolean };
  type Block = { kind: "h" | "p"; spans: Span[] } | { kind: "ul"; items: Span[][] };

  /** `**bold**` runs as spans; everything else is plain text (never HTML). */
  function spans(line: string): Span[] {
    return line
      .split(/\*\*(.+?)\*\*/)
      .map((text, i) => ({ text, bold: i % 2 === 1 }))
      .filter((s) => s.text);
  }

  /** Headings, bullet lists and paragraphs: enough for the summaries Wisp writes. */
  function parse(md: string): Block[] {
    const blocks: Block[] = [];
    for (const raw of md.split("\n")) {
      const line = raw.trim();
      const last = blocks[blocks.length - 1];
      if (!line) continue;
      const h = /^#{1,6}\s+(.*)$/.exec(line);
      const li = /^[-*]\s+(.*)$/.exec(line);
      if (h) blocks.push({ kind: "h", spans: spans(h[1]) });
      else if (li && last?.kind === "ul") last.items.push(spans(li[1]));
      else if (li) blocks.push({ kind: "ul", items: [spans(li[1])] });
      else blocks.push({ kind: "p", spans: spans(line) });
    }
    return blocks;
  }

  const blocks = $derived(parse(summary));
</script>

{#snippet text(parts: Span[])}{#each parts as s, i (i)}{#if s.bold}<strong>{s.text}</strong>{:else}{s.text}{/if}{/each}{/snippet}

<div class="summary-view">
  <div class="head">
    <button class="btn" class:primary={!summary} disabled={busy} onclick={run}>
      {busy ? i18n.t.intel.summarizing : summary ? i18n.t.intel.regenerate : i18n.t.intel.summarize}
    </button>
    {#if !hasState}<span class="note">{i18n.t.intel.summaryFromTranscript}</span>{/if}
  </div>
  {#if error}<p class="error">{i18n.t.intel.summaryFailed(error)}</p>{/if}
  {#if summary}
    <div class="md">
      {#each blocks as b, i (i)}
        {#if b.kind === "h"}
          <h4>{@render text(b.spans)}</h4>
        {:else if b.kind === "ul"}
          <ul>
            {#each b.items as item, j (j)}<li>{@render text(item)}</li>{/each}
          </ul>
        {:else}
          <p>{@render text(b.spans)}</p>
        {/if}
      {/each}
    </div>
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

  .md {
    font-size: 13.5px;
    line-height: 1.6;
    color: var(--text);
    user-select: text;
  }

  .md h4 {
    margin: 12px 0 4px;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }

  .md p {
    margin: 0 0 6px;
  }

  .md ul {
    margin: 0;
    padding-left: 18px;
  }
</style>

<script lang="ts">
  // Ask about one saved meeting: the stored question-and-answer thread (each answer with checked
  // citations) plus a box to ask more. Reads and writes the saved thread via intel_saved_ask /
  // intel_ask_saved, so a past meeting's questions are there when it is reopened. The thread is
  // pruned with the transcript its answers quote, the same as the meeting's prompt runs.
  import { tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import { copyText } from "$lib/clipboard";
  import type { AskAnswer, AskTurn } from "$lib/intel.svelte";

  let { meetingId }: { meetingId: string } = $props();

  type SavedTurn = { question: string; answer: AskAnswer; atMs: number };

  let turns = $state<AskTurn[]>([]);
  let draft = $state("");
  let copiedAt = $state(-1);
  let feedEl = $state<HTMLDivElement>();

  const t = $derived(i18n.t.intel);
  const asking = $derived(turns.some((x) => x.pending));
  const isBrief = (a: AskAnswer) => !!a.short && a.short !== a.answer;

  // Load the stored thread when the meeting changes.
  $effect(() => {
    const want = meetingId;
    turns = [];
    draft = "";
    invoke<SavedTurn[]>("intel_saved_ask", { id: want })
      .then((saved) => {
        if (meetingId !== want) return;
        turns = saved.map((s) => ({ question: s.question, answer: s.answer, pending: false }));
        scrollDown();
      })
      .catch(() => {});
  });

  function scrollDown() {
    void tick().then(() => feedEl?.scrollTo({ top: feedEl.scrollHeight }));
  }

  async function send(text = draft) {
    const q = text.trim();
    if (!q || asking) return;
    draft = "";
    const id = meetingId;
    // Finished turns become the conversation history, the same shape the live Ask sends.
    const history = turns
      .filter((x) => x.answer)
      .map((x) => ({ question: x.question, answer: x.answer!.answer }));
    turns.push({ question: q, pending: true });
    const turn = turns[turns.length - 1];
    scrollDown();
    try {
      turn.answer = await invoke<AskAnswer>("intel_ask_saved", { id, question: q, history });
    } catch (e) {
      turn.error = String(e);
    } finally {
      turn.pending = false;
      scrollDown();
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  function cancel() {
    invoke("intel_ask_cancel").catch(() => {});
  }

  async function copy(i: number, text: string) {
    try {
      await copyText(text);
      copiedAt = i;
      setTimeout(() => copiedAt === i && (copiedAt = -1), 1500);
    } catch {
      // clipboard unavailable; nothing to do
    }
  }
</script>

<div class="ask">
  <div class="feed" bind:this={feedEl}>
    {#if !turns.length}
      <p class="muted">{t.askEmpty}</p>
      <div class="suggest">
        {#each t.suggestions as s (s)}
          <button class="chip" onclick={() => send(s)}>{s}</button>
        {/each}
      </div>
    {/if}
    {#each turns as turn, i (i)}
      <div class="turn">
        <p class="q">{turn.question}</p>
        {#if turn.pending}
          <p class="muted working" aria-live="polite">{t.thinking}</p>
        {:else if turn.error}
          <p class="err">{turn.error}</p>
        {:else if turn.answer}
          {@const brief = isBrief(turn.answer)}
          <div class="a">
            {#if brief}<p class="answer short">{turn.answer.short}</p>{/if}
            {#if !turn.answer.grounded}<p class="warn">{t.notGrounded}</p>{/if}
            <p class="answer" class:detail={brief}>{turn.answer.answer}</p>
            {#if turn.answer.unknownCitations.length}
              <p class="warn">{t.droppedCitations(turn.answer.unknownCitations.length)}</p>
            {/if}
            {#if turn.answer.citations.length}
              <details class="sources">
                <summary>{t.sources} ({turn.answer.citations.length})</summary>
                <ul>
                  {#each turn.answer.citations as c (c.id)}
                    <li title={c.text}>
                      <span class="cid">{c.id}</span>
                      <span class="clabel">{c.label}</span>
                      <span class="ctext">{c.text}</span>
                    </li>
                  {/each}
                </ul>
              </details>
            {/if}
            <button class="btn" onclick={() => copy(i, turn.answer!.markdown)}>
              {copiedAt === i ? t.copied : t.copy}
            </button>
          </div>
        {/if}
      </div>
    {/each}
  </div>
  <div class="composer">
    <textarea rows="2" placeholder={t.askPlaceholder} bind:value={draft} onkeydown={onKey}></textarea>
    {#if asking}
      <button class="btn" onclick={cancel}>{t.cancel}</button>
    {:else}
      <button class="btn primary" disabled={!draft.trim()} onclick={() => send()}>{t.ask}</button>
    {/if}
  </div>
</div>

<style>
  .ask {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .feed {
    display: flex;
    flex-direction: column;
    gap: 12px;
    max-height: 60vh;
    overflow-y: auto;
    min-width: 0;
  }

  .turn {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-width: 0;
  }

  .q {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
  }

  .a {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 9px;
    min-width: 0;
  }

  .answer {
    margin: 0;
    font-size: 13px;
    line-height: 1.55;
    color: var(--text);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    user-select: text;
  }

  .answer.short {
    font-weight: 600;
  }

  .answer.detail {
    color: var(--muted);
  }

  .warn {
    margin: 0;
    font-size: 12px;
    color: var(--warn, #b7791f);
  }

  .sources summary {
    font-size: 12px;
    color: var(--muted);
    cursor: pointer;
  }

  .sources ul {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .sources li {
    display: flex;
    gap: 6px;
    font-size: 12px;
    color: var(--muted);
    min-width: 0;
  }

  .cid {
    flex: none;
    font-family: var(--font-mono);
    color: var(--accent);
  }

  .clabel {
    flex: none;
  }

  .ctext {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .suggest {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    min-width: 0;
  }

  textarea {
    flex: 1;
    font: inherit;
    font-size: 12.5px;
    line-height: 1.45;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 6px 8px;
    resize: vertical;
    min-width: 0;
  }

  .muted {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--muted);
  }

  .err {
    margin: 0;
    font-size: 12.5px;
    color: var(--danger, #c0392b);
    white-space: pre-wrap;
  }

  .working {
    animation: pulse 1.2s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.45;
    }
  }

  .btn {
    font-family: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 3px 10px;
    cursor: pointer;
  }

  .btn:hover:not(:disabled) {
    color: var(--text);
    border-color: var(--accent);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn.primary {
    color: var(--text);
    border-color: var(--accent);
  }

  .chip {
    font: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 10px;
    cursor: pointer;
  }

  .chip:hover {
    color: var(--text);
    border-color: var(--accent);
  }
</style>

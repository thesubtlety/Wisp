<script lang="ts">
  import { copyText } from "$lib/clipboard";
  // The AI activity log: every call Wisp made to a model — on this machine or a remote service —
  // with the full text sent and the reply, newest first. Shown in Settings and from the intelligence
  // panel. The backend caps the list; each entry can hold a whole transcript, so bodies render only
  // when expanded.
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";

  // A meeting to offer under "This meeting" when none is live (the one just saved, say).
  let { meetingId = "" }: { meetingId?: string } = $props();

  type LlmCall = {
    id: number;
    atMs: number;
    meetingId: string | null;
    task: string;
    backend: string;
    model: string | null;
    local: boolean;
    instructions: string;
    context: string;
    images: string[];
    output: string;
    error: string | null;
    elapsedMs: number;
    tokensIn: number;
    tokensOut: number;
    tokensEstimated: boolean;
  };

  // Matches MAX_LISTED in the backend.
  const CAP = 200;

  let calls = $state<LlmCall[]>([]);
  let scope = $state<"all" | "meeting">("all");
  let liveId = $state<string | null>(null);
  let open = $state<number | null>(null);
  let confirmClear = $state(false);
  let copied = $state("");
  let error = $state("");
  let loading = $state(false);

  const current = $derived(liveId ?? (meetingId || null));

  async function load() {
    loading = true;
    try {
      liveId = await invoke<string | null>("ai_activity_live_meeting");
      const filter = scope === "meeting" ? current : null;
      calls = scope === "meeting" && !filter ? [] : await invoke<LlmCall[]>("list_ai_activity", { meetingId: filter });
      error = "";
    } catch (e) {
      error = String(e);
    }
    loading = false;
  }

  async function clearAll() {
    try {
      await invoke<number>("clear_ai_activity");
      confirmClear = false;
      open = null;
      await load();
    } catch (e) {
      error = String(e);
    }
  }

  async function copy(key: string, text: string) {
    try {
      await copyText(text);
      copied = key;
      setTimeout(() => {
        if (copied === key) copied = "";
      }, 1500);
    } catch (e) {
      error = String(e);
    }
  }

  function taskName(task: string): string {
    return (i18n.t.audit.tasks as Record<string, string>)[task] ?? task;
  }

  function when(ms: number): string {
    return new Date(ms).toLocaleString();
  }

  function seconds(ms: number): string {
    return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`;
  }

  // Everything sent, as one copyable text.
  function sent(c: LlmCall): string {
    const parts = [`# ${i18n.t.audit.instructions}\n${c.instructions}`, `# ${i18n.t.audit.context}\n${c.context}`];
    if (c.images.length) parts.push(`# ${i18n.t.audit.images}\n${c.images.join("\n")}`);
    return parts.join("\n\n");
  }

  $effect(() => {
    void scope;
    load();
  });
</script>

<div class="audit">
  <p class="intro">{i18n.t.audit.intro}</p>

  <div class="bar">
    <div class="scope" role="tablist">
      <button role="tab" aria-selected={scope === "all"} class:on={scope === "all"} onclick={() => (scope = "all")}>
        {i18n.t.audit.all}
      </button>
      <button role="tab" aria-selected={scope === "meeting"} class:on={scope === "meeting"} onclick={() => (scope = "meeting")}>
        {i18n.t.audit.thisMeeting}
      </button>
    </div>
    <span class="grow"></span>
    <button class="btn" disabled={loading} onclick={load}>{i18n.t.audit.refresh}</button>
    <button class="btn danger" disabled={!calls.length && scope === "all"} onclick={() => (confirmClear = true)}>
      {i18n.t.audit.clear}
    </button>
  </div>

  {#if confirmClear}
    <div class="confirm">
      <span>{i18n.t.audit.clearConfirm}</span>
      <button class="btn danger" onclick={clearAll}>{i18n.t.audit.clearYes}</button>
      <button class="btn" onclick={() => (confirmClear = false)}>{i18n.t.audit.cancel}</button>
    </div>
  {/if}

  {#if error}<p class="err">{error}</p>{/if}

  {#if scope === "meeting" && !current}
    <p class="empty">{i18n.t.audit.noMeeting}</p>
  {:else if !calls.length && !loading}
    <p class="empty">{i18n.t.audit.empty}</p>
  {:else}
    {#if calls.length >= CAP}<p class="empty">{i18n.t.audit.capped(CAP)}</p>{/if}
    <ul class="calls">
      {#each calls as c (c.id)}
        <li class="call" class:failed={!!c.error}>
          <button class="row" aria-expanded={open === c.id} onclick={() => (open = open === c.id ? null : c.id)}>
            <span class="where" class:local={c.local}>{c.local ? i18n.t.audit.local : i18n.t.audit.remote}</span>
            <span class="what">
              <span class="task">{taskName(c.task)}</span>
              <span class="via">{c.backend} · {c.model ?? i18n.t.audit.defaultModel}</span>
            </span>
            <span class="meta">
              <span>{when(c.atMs)}</span>
              <span>
                {i18n.t.audit.tokens(c.tokensIn, c.tokensOut, c.tokensEstimated)} · {seconds(c.elapsedMs)} ·
                <span class="status">{c.error ? i18n.t.audit.failed : i18n.t.audit.ok}</span>
              </span>
            </span>
          </button>

          {#if open === c.id}
            <div class="detail">
              {#if c.tokensEstimated}<p class="note">{i18n.t.audit.estimatedNote}</p>{/if}
              <div class="block-head">
                <span>{i18n.t.audit.sent}</span>
                <button class="btn" onclick={() => copy(`s${c.id}`, sent(c))}>
                  {copied === `s${c.id}` ? i18n.t.audit.copied : i18n.t.audit.copy}
                </button>
              </div>
              <h5>{i18n.t.audit.instructions}</h5>
              <pre>{c.instructions}</pre>
              <h5>{i18n.t.audit.context}</h5>
              <pre>{c.context}</pre>
              {#if c.images.length}
                <h5>{i18n.t.audit.images}</h5>
                <pre>{c.images.join("\n")}</pre>
              {/if}
              {#if c.error}
                <h5>{i18n.t.audit.error}</h5>
                <pre class="err-text">{c.error}</pre>
              {:else}
                <div class="block-head">
                  <span>{i18n.t.audit.reply}</span>
                  <button class="btn" onclick={() => copy(`r${c.id}`, c.output)}>
                    {copied === `r${c.id}` ? i18n.t.audit.copied : i18n.t.audit.copy}
                  </button>
                </div>
                <pre>{c.output}</pre>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .audit {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .intro,
  .empty,
  .note {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--muted);
  }

  .bar,
  .confirm {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }

  .confirm {
    font-size: 12.5px;
    color: var(--text);
  }

  .grow {
    flex: 1;
  }

  .scope {
    display: inline-flex;
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    overflow: hidden;
  }

  .scope button {
    font: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: none;
    padding: 4px 10px;
    cursor: pointer;
  }

  .scope button.on {
    color: var(--text);
    background: var(--surface-active);
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

  .btn.danger,
  .err,
  .err-text,
  .failed .status {
    color: var(--danger, #c0392b);
  }

  .err {
    margin: 0;
    font-size: 12.5px;
  }

  .calls {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .call {
    border: 1px solid var(--border);
    border-radius: 9px;
    min-width: 0;
  }

  .row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 10px;
    font: inherit;
    text-align: left;
    color: var(--text);
    background: transparent;
    border: none;
    cursor: pointer;
  }

  .row:hover {
    background: var(--surface-active);
    border-radius: 9px;
  }

  .where {
    flex: none;
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.04em;
    padding: 2px 6px;
    border-radius: 5px;
    border: 1px solid var(--accent);
    color: var(--accent);
  }

  .where.local {
    border-color: var(--border-strong);
    color: var(--muted);
  }

  .what,
  .meta {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .what {
    flex: 1;
  }

  .task {
    font-size: 13px;
  }

  .via,
  .meta {
    font-size: 11.5px;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    flex: none;
    align-items: flex-end;
  }

  .detail {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 4px 10px 10px;
    border-top: 1px solid var(--border);
  }

  .block-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-top: 6px;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--text);
  }

  h5 {
    margin: 2px 0 0;
    font-size: 11.5px;
    font-weight: 500;
    color: var(--muted);
  }

  pre {
    margin: 0;
    max-height: 240px;
    overflow: auto;
    padding: 8px;
    border-radius: 7px;
    background: var(--surface);
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 11.5px;
    line-height: 1.45;
    white-space: pre-wrap;
    word-break: break-word;
    color: var(--text);
  }
</style>

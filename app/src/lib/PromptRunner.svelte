<script lang="ts">
  // Runs a saved prompt over one meeting: pick a prompt (and a speaker for a speaker prompt), Run,
  // then copy or save the result. For a saved meeting (`meetingId`) the backend reads the transcript
  // and keeps each run, listed under "Past runs". For the live meeting (`live` + `transcript`) the
  // run is not kept. "Manage prompts" edits the library in a modal.
  import { onMount } from "svelte";
  import Modal from "$lib/Modal.svelte";
  import { copyText } from "$lib/clipboard";
  import { i18n } from "$lib/i18n.svelte";
  import {
    PROMPT_VARIABLES,
    createPrompt,
    deletePrompt,
    deletePromptRun,
    listPromptRuns,
    listPrompts,
    parseMarkdown,
    resetPrompt,
    runPrompt,
    savePromptOutput,
    updatePrompt,
    type Block,
    type Inline,
    type Prompt,
    type PromptRun,
    type PromptScope,
  } from "$lib/prompts";

  let {
    meetingId,
    live = false,
    transcript = "",
    speakers,
    title,
    date,
  }: {
    /** A saved meeting to run over. Its runs are kept. */
    meetingId?: string;
    /** Run over `transcript` (the live meeting). Runs are not kept. */
    live?: boolean;
    /** The live transcript, one "Name: text" line per turn. Used only when `live`. */
    transcript?: string;
    /** Speaker labels or names to offer for a speaker prompt ("You", "Speaker 2", "Alice"). */
    speakers: string[];
    /** Fills {title}; a saved meeting defaults to its own title. */
    title?: string;
    /** Fills {date}; a saved meeting defaults to its start date. */
    date?: string;
  } = $props();

  const t = $derived(i18n.t.prompts);
  const PROMPT_KEY = "wisp.promptId";

  let prompts = $state<Prompt[]>([]);
  let promptId = $state(readStored());
  let speaker = $state("");
  let running = $state(false);
  let result = $state("");
  let error = $state("");
  let flash = $state("");
  let runs = $state<PromptRun[]>([]);
  let openRun = $state<number | null>(null);

  const saved = $derived(!live && !!meetingId);
  const current = $derived(prompts.find((p) => p.id === promptId) ?? prompts[0]);
  const needsSpeaker = $derived(current?.scope === "speaker");
  const hasTranscript = $derived(saved || transcript.trim() !== "");
  const canRun = $derived(
    !!current && !running && hasTranscript && (!needsSpeaker || speaker !== ""),
  );
  const blocks = $derived(parseMarkdown(result));

  function readStored(): string {
    try {
      return localStorage.getItem(PROMPT_KEY) ?? "";
    } catch {
      return "";
    }
  }

  function remember(id: string) {
    promptId = id;
    try {
      localStorage.setItem(PROMPT_KEY, id);
    } catch {
      /* storage unavailable */
    }
  }

  // Keep the chosen speaker one of those offered.
  $effect(() => {
    if (!speakers.includes(speaker)) speaker = speakers[0] ?? "";
  });

  // Reload past runs when the meeting changes.
  $effect(() => {
    const id = meetingId;
    result = "";
    error = "";
    runs = [];
    if (saved && id) void loadRuns(id);
  });

  onMount(() => {
    void loadPrompts();
  });

  async function loadPrompts() {
    try {
      prompts = await listPrompts();
    } catch (e) {
      error = String(e);
    }
  }

  async function loadRuns(id: string) {
    try {
      runs = await listPromptRuns(id);
    } catch (e) {
      error = String(e);
    }
  }

  async function run() {
    if (!current || !canRun) return;
    running = true;
    error = "";
    result = "";
    try {
      result = await runPrompt({
        promptId: current.id,
        speaker: needsSpeaker ? speaker : undefined,
        meetingId: saved ? meetingId : undefined,
        transcript: saved ? undefined : transcript,
        title,
        date,
      });
      if (saved && meetingId) await loadRuns(meetingId);
    } catch (e) {
      error = String(e);
    } finally {
      running = false;
    }
  }

  function note(text: string) {
    flash = text;
    setTimeout(() => {
      if (flash === text) flash = "";
    }, 1500);
  }

  async function copy(text: string) {
    try {
      await copyText(text);
      note(t.copied);
    } catch (e) {
      error = String(e);
    }
  }

  function fileName(name: string, who: string | null): string {
    const base = [title, name, who].filter(Boolean).join(" - ");
    return base.replace(/[\\/:*?"<>|]+/g, "-").slice(0, 120) || "prompt";
  }

  async function save(text: string, name: string, who: string | null) {
    try {
      if (await savePromptOutput(text, fileName(name, who))) note(t.saved);
    } catch (e) {
      error = String(e);
    }
  }

  async function removeRun(id: number) {
    try {
      await deletePromptRun(id);
      runs = runs.filter((r) => r.id !== id);
    } catch (e) {
      error = String(e);
    }
  }

  function when(ms: number): string {
    return new Date(ms).toLocaleString(i18n.locale, { dateStyle: "medium", timeStyle: "short" });
  }

  // ── Manage prompts ──
  let managing = $state(false);
  let editId = $state<string | null>(null); // null: a new prompt
  let editName = $state("");
  let editBody = $state("");
  let editScope = $state<PromptScope>("meeting");
  let editError = $state("");
  let confirmDelete = $state(false);
  let bodyEl = $state<HTMLTextAreaElement | null>(null);
  const editing = $derived(prompts.find((p) => p.id === editId));

  function openManager() {
    managing = true;
    pick(current ?? null);
  }

  function pick(p: Prompt | null) {
    editId = p?.id ?? null;
    editName = p?.name ?? "";
    editBody = p?.body ?? "";
    editScope = p?.scope ?? "meeting";
    editError = "";
    confirmDelete = false;
  }

  function insertVar(name: string) {
    const token = `{${name}}`;
    const el = bodyEl;
    if (!el) {
      editBody += token;
      return;
    }
    const start = el.selectionStart;
    const end = el.selectionEnd;
    editBody = editBody.slice(0, start) + token + editBody.slice(end);
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(start + token.length, start + token.length);
    });
  }

  function replace(p: Prompt) {
    const i = prompts.findIndex((x) => x.id === p.id);
    prompts = i < 0 ? [...prompts, p] : prompts.map((x) => (x.id === p.id ? p : x));
    pick(p);
  }

  async function saveEdit() {
    editError = "";
    try {
      const p = editId
        ? await updatePrompt(editId, editName, editBody, editScope)
        : await createPrompt(editName, editBody, editScope);
      replace(p);
      remember(p.id);
    } catch (e) {
      editError = String(e);
    }
  }

  async function resetEdit() {
    if (!editId) return;
    try {
      replace(await resetPrompt(editId));
    } catch (e) {
      editError = String(e);
    }
  }

  async function deleteEdit() {
    if (!editId) return;
    try {
      await deletePrompt(editId);
      prompts = prompts.filter((p) => p.id !== editId);
      if (promptId === editId) remember(prompts[0]?.id ?? "");
      pick(prompts[0] ?? null);
    } catch (e) {
      editError = String(e);
    }
  }
</script>

{#snippet inline(parts: Inline[])}
  {#each parts as part, i (i)}
    {#if part.bold}<strong>{part.text}</strong>{:else if part.code}<code>{part.text}</code>{:else}{part.text}{/if}
  {/each}
{/snippet}

{#snippet markdown(list: Block[])}
  <div class="md">
    {#each list as b, i (i)}
      {#if b.kind === "heading"}
        <p class="h" class:h1={b.level <= 2}>{@render inline(b.parts)}</p>
      {:else if b.kind === "item"}
        <p class="li">
          <span class="bullet"
            >{b.checked === null ? (b.ordered ? `${b.ordered}.` : "•") : b.checked ? "☑" : "☐"}</span
          >
          <span>{@render inline(b.parts)}</span>
        </p>
      {:else if b.kind === "table"}
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                {#each b.head as cell, c (c)}<th>{@render inline(cell)}</th>{/each}
              </tr>
            </thead>
            <tbody>
              {#each b.rows as row, r (r)}
                <tr>
                  {#each row as cell, c (c)}<td>{@render inline(cell)}</td>{/each}
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {:else if b.kind === "para"}
        <p>{@render inline(b.parts)}</p>
      {:else}
        <div class="gap"></div>
      {/if}
    {/each}
  </div>
{/snippet}

<div class="runner">
  <div class="bar">
    <label class="field">
      <span class="label">{t.prompt}</span>
      <select
        class="input"
        value={current?.id ?? ""}
        onchange={(e) => remember(e.currentTarget.value)}
        disabled={running}
      >
        {#each prompts as p (p.id)}
          <option value={p.id}>{p.name}</option>
        {/each}
      </select>
    </label>

    {#if needsSpeaker}
      <label class="field">
        <span class="label">{t.speaker}</span>
        {#if speakers.length}
          <select class="input" bind:value={speaker} disabled={running}>
            {#each speakers as s (s)}
              <option value={s}>{s}</option>
            {/each}
          </select>
        {:else}
          <span class="muted">{t.noSpeakers}</span>
        {/if}
      </label>
    {/if}

    <button class="btn primary" onclick={run} disabled={!canRun}>
      {running ? t.running : t.run}
    </button>
    <span class="grow"></span>
    <button class="btn" onclick={openManager}>{t.manage}</button>
  </div>

  {#if !hasTranscript}
    <p class="muted">{t.noTranscript}</p>
  {:else if live}
    <p class="muted">{t.liveNote}</p>
  {/if}

  {#if error}<p class="err">{error}</p>{/if}

  {#if running}
    <p class="muted working" aria-live="polite">{t.running}</p>
  {:else if result}
    <section class="result">
      {@render markdown(blocks)}
      <div class="actions">
        <button class="btn" onclick={() => copy(result)}>{t.copy}</button>
        <button
          class="btn"
          onclick={() => save(result, current?.name ?? "prompt", needsSpeaker ? speaker : null)}
          >{t.save}</button
        >
        {#if flash}<span class="muted">{flash}</span>{/if}
      </div>
    </section>
  {/if}

  {#if saved}
    <section class="runs">
      <h3 class="label">{t.pastRuns}</h3>
      {#if runs.length === 0}
        <p class="muted">{t.noRuns}</p>
      {:else}
        <ul>
          {#each runs as r (r.id)}
            <li class="run">
              <button class="run-head" onclick={() => (openRun = openRun === r.id ? null : r.id)}>
                <span class="run-name">
                  {r.promptName}{#if r.speaker}&nbsp;<span class="muted">{t.forSpeaker(r.speaker)}</span>{/if}
                </span>
                <span class="muted small">{when(r.atMs)}{r.backend ? ` · ${r.backend}` : ""}</span>
              </button>
              {#if openRun === r.id}
                <div class="run-body">
                  {@render markdown(parseMarkdown(r.output))}
                  <div class="actions">
                    <button class="btn" onclick={() => copy(r.output)}>{t.copy}</button>
                    <button class="btn" onclick={() => save(r.output, r.promptName, r.speaker)}
                      >{t.save}</button
                    >
                    <button class="btn danger" onclick={() => removeRun(r.id)}>{t.deleteRun}</button>
                  </div>
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}
</div>

<Modal bind:open={managing} title={t.manage}>
  <div class="manage">
    <div class="chips">
      {#each prompts as p (p.id)}
        <button class="chip" class:on={p.id === editId} onclick={() => pick(p)}>
          {p.name}{#if p.customized}<span class="tag">{t.edited}</span>{/if}
        </button>
      {/each}
      <button class="chip" class:on={editId === null} onclick={() => pick(null)}>+ {t.newPrompt}</button>
    </div>

    <label class="field col">
      <span class="label">{t.name}</span>
      <input class="input" bind:value={editName} />
    </label>

    <label class="field col">
      <span class="label">{t.scope}</span>
      <select class="input" bind:value={editScope}>
        <option value="meeting">{t.scopeMeeting}</option>
        <option value="speaker">{t.scopeSpeaker}</option>
      </select>
    </label>

    <label class="field col">
      <span class="label">{t.body}</span>
      <textarea class="input body" rows="8" bind:value={editBody} bind:this={bodyEl}></textarea>
    </label>

    <div class="vars">
      <span class="muted small">{t.variables}</span>
      {#each PROMPT_VARIABLES as v (v)}
        <button class="var" title={t.vars[v]} onclick={() => insertVar(v)}>{`{${v}}`}</button>
      {/each}
    </div>

    {#if editError}<p class="err">{editError}</p>{/if}

    <div class="actions">
      <button class="btn primary" onclick={saveEdit}>{editId ? t.saveChanges : t.add}</button>
      {#if editing?.builtin}
        <span class="muted small">{t.builtin}</span>
        <button class="btn" onclick={resetEdit} disabled={!editing.customized}>{t.reset}</button>
      {:else if editing}
        {#if confirmDelete}
          <span class="small">{t.deleteConfirm(editing.name)}</span>
          <button class="btn danger" onclick={deleteEdit}>{t.delete}</button>
          <button class="btn" onclick={() => (confirmDelete = false)}>{t.cancel}</button>
        {:else}
          <button class="btn danger" onclick={() => (confirmDelete = true)}>{t.delete}</button>
        {/if}
      {/if}
    </div>
  </div>
</Modal>

<style>
  .runner,
  .manage {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .bar,
  .actions,
  .vars,
  .chips {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px 8px;
  }

  .field {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .field.col {
    flex-direction: column;
    align-items: stretch;
    gap: 4px;
  }

  .label {
    margin: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }

  .input {
    font: inherit;
    font-size: 12.5px;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 4px 8px;
    min-width: 0;
    max-width: 100%;
  }

  .input.body {
    resize: vertical;
    line-height: 1.45;
  }

  .grow {
    flex: 1;
  }

  .muted {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--muted);
  }

  .small {
    font-size: 11.5px;
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

  .btn.danger {
    color: var(--danger, #c0392b);
  }

  .result,
  .run-body {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 9px;
    min-width: 0;
  }

  .md {
    font-size: 13px;
    line-height: 1.55;
    color: var(--text);
    user-select: text;
    overflow-wrap: anywhere;
  }

  .md p {
    margin: 0;
  }

  .md .h {
    font-weight: 600;
    margin-top: 4px;
  }

  .md .h1 {
    font-size: 14px;
  }

  .md .li {
    display: flex;
    gap: 6px;
    padding-left: 4px;
  }

  .md .bullet {
    flex: none;
    color: var(--muted);
  }

  .md .gap {
    height: 8px;
  }

  .md code {
    font-family: var(--font-mono);
    font-size: 12px;
    background: var(--surface-active);
    border-radius: 4px;
    padding: 0 3px;
  }

  .table-wrap {
    overflow-x: auto;
    margin: 4px 0;
  }

  .md table {
    border-collapse: collapse;
    font-size: 12.5px;
  }

  .md th,
  .md td {
    border: 1px solid var(--border);
    padding: 4px 8px;
    text-align: left;
    vertical-align: top;
  }

  .runs ul {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .run {
    border: 1px solid var(--border);
    border-radius: 9px;
    min-width: 0;
  }

  .run .run-body {
    border: none;
    border-top: 1px solid var(--border);
    border-radius: 0;
  }

  .run-head {
    width: 100%;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
    padding: 7px 10px;
    font: inherit;
    font-size: 12.5px;
    text-align: left;
    color: var(--text);
    background: transparent;
    border: none;
    border-radius: 9px;
    cursor: pointer;
  }

  .run-head:hover {
    background: var(--surface-active);
  }

  .run-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .chip,
  .var {
    font: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 10px;
    cursor: pointer;
  }

  .chip.on {
    color: var(--text);
    border-color: var(--accent);
    background: var(--surface-active);
  }

  .var {
    font-family: var(--font-mono);
    border-radius: 6px;
    padding: 1px 6px;
  }

  .var:hover,
  .chip:hover {
    color: var(--text);
  }

  .tag {
    margin-left: 6px;
    font-size: 10.5px;
    color: var(--accent);
  }
</style>

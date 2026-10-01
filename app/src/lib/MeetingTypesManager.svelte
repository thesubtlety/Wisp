<script lang="ts">
  // Settings › Meeting types: list, edit, reset (built-ins) and add or delete (the user's own).
  // List fields (checklist, summary sections, suggested prompts) are edited one per line.
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { i18n } from "$lib/i18n.svelte";
  import { intel, loadMeetingTypes, CADENCES, CARD_STYLES, type MeetingType } from "$lib/intel.svelte";

  const t = $derived(i18n.t.meetingTypes);

  /** The type being edited; null is a new one. */
  let editId = $state<string | null>(null);
  let name = $state("");
  let description = $state("");
  let watchFor = $state("");
  let cardStyle = $state<MeetingType["cardStyle"]>("balanced");
  let cadence = $state<MeetingType["cadence"]>("normal");
  let checklist = $state("");
  let sections = $state("");
  let prompts = $state("");
  let error = $state("");
  let flash = $state("");
  let confirmDelete = $state(false);

  const editing = $derived(intel.meetingTypes.find((m) => m.id === editId) ?? null);

  const lines = (text: string) =>
    text
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);

  function pick(m: MeetingType | null) {
    editId = m?.id ?? null;
    name = m?.name ?? "";
    description = m?.description ?? "";
    watchFor = m?.watchFor ?? "";
    cardStyle = m?.cardStyle ?? "balanced";
    cadence = m?.cadence ?? "normal";
    checklist = (m?.wrapChecklist ?? []).join("\n");
    sections = (m?.summarySections ?? []).join("\n");
    prompts = (m?.suggestedPrompts ?? []).join("\n");
    error = "";
    flash = "";
    confirmDelete = false;
  }

  onMount(async () => {
    await loadMeetingTypes();
    pick(intel.meetingTypes[0] ?? null);
  });

  async function save() {
    error = "";
    try {
      const saved = await invoke<MeetingType>("save_meeting_type", {
        meetingType: {
          id: editId ?? "",
          name,
          description,
          watchFor,
          cardStyle,
          cadence,
          wrapChecklist: lines(checklist),
          summarySections: lines(sections),
          suggestedPrompts: lines(prompts),
        },
      });
      await loadMeetingTypes();
      pick(intel.meetingTypes.find((m) => m.id === saved.id) ?? saved);
      flash = i18n.t.prompts.saved;
    } catch (e) {
      error = String(e);
    }
  }

  async function reset() {
    if (!editId) return;
    try {
      const m = await invoke<MeetingType>("reset_meeting_type", { id: editId });
      await loadMeetingTypes();
      pick(m);
    } catch (e) {
      error = String(e);
    }
  }

  async function remove() {
    if (!editId) return;
    try {
      await invoke("delete_meeting_type", { id: editId });
      await loadMeetingTypes();
      pick(intel.meetingTypes[0] ?? null);
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="types">
  <p class="intro">{t.help}</p>

  <div class="chips">
    {#each intel.meetingTypes as m (m.id)}
      <button class="chip" class:on={m.id === editId} onclick={() => pick(m)}>
        {m.name}{#if m.customized}<span class="tag">{t.edited}</span>{/if}
      </button>
    {/each}
    <button class="chip" class:on={editId === null} onclick={() => pick(null)}>+ {t.newType}</button>
  </div>

  <label class="field">
    <span class="label">{t.name}</span>
    <input class="input" bind:value={name} placeholder={t.untitled} />
  </label>

  <label class="field">
    <span class="label">{t.description}</span>
    <input class="input" bind:value={description} />
    <span class="hint">{t.descriptionHelp}</span>
  </label>

  <div class="row">
    <label class="field">
      <span class="label">{t.cadence}</span>
      <select class="input" bind:value={cadence}>
        {#each CADENCES as c (c)}<option value={c}>{t.cadences[c]}</option>{/each}
      </select>
    </label>
    <label class="field">
      <span class="label">{t.cardStyle}</span>
      <select class="input" bind:value={cardStyle}>
        {#each CARD_STYLES as s (s)}<option value={s}>{t.cardStyles[s]}</option>{/each}
      </select>
    </label>
  </div>

  <label class="field">
    <span class="label">{t.watchFor}</span>
    <textarea class="input" rows="6" bind:value={watchFor}></textarea>
    <span class="hint">{t.watchForHelp}</span>
  </label>

  <label class="field">
    <span class="label">{t.wrapChecklist}</span>
    <textarea class="input" rows="4" bind:value={checklist}></textarea>
    <span class="hint">{t.onePerLine}</span>
  </label>

  <label class="field">
    <span class="label">{t.summarySections}</span>
    <textarea class="input" rows="3" bind:value={sections}></textarea>
    <span class="hint">{t.onePerLine}</span>
  </label>

  <label class="field">
    <span class="label">{t.suggestedPrompts}</span>
    <textarea class="input" rows="2" bind:value={prompts}></textarea>
    <span class="hint">{t.onePerLine}</span>
  </label>

  {#if error}<p class="err">{error}</p>{/if}

  <div class="actions">
    <button class="btn primary" disabled={!name.trim()} onclick={save}>{t.save}</button>
    {#if editing?.builtin}
      <span class="hint">{t.builtin}</span>
      <button class="btn" onclick={reset} disabled={!editing.customized}>{t.reset}</button>
    {:else if editing}
      {#if confirmDelete}
        <span class="hint">{t.deleteConfirm(editing.name)}</span>
        <button class="btn danger" onclick={remove}>{t.delete}</button>
        <button class="btn" onclick={() => (confirmDelete = false)}>{t.cancel}</button>
      {:else}
        <button class="btn danger" onclick={() => (confirmDelete = true)}>{t.delete}</button>
      {/if}
    {/if}
    {#if flash}<span class="hint">{flash}</span>{/if}
  </div>
</div>

<style>
  .types {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .intro {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--muted);
  }

  .chips,
  .actions,
  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px 8px;
  }

  .row {
    align-items: flex-start;
    gap: 12px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }

  .label {
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }

  .hint {
    font-size: 11.5px;
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

  textarea.input {
    resize: vertical;
    line-height: 1.45;
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
  }

  .chip.on {
    color: var(--text);
    border-color: var(--accent);
    background: var(--surface-active);
  }

  .tag {
    margin-left: 6px;
    font-size: 10.5px;
    color: var(--accent);
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

  .err {
    margin: 0;
    font-size: 12px;
    color: var(--danger, #c0392b);
  }
</style>

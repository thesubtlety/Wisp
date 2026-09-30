<script lang="ts">
  // A project's overview: open commitments, open questions, recent decisions and active risks across
  // its meetings plus the items the user added by hand, then its recent meetings and what was marked
  // done. Each item shows where it came from (a meeting chip that opens it, or "Added by you") and has
  // quiet hand-edit actions. The selection is the brief's (see `project_overview` in the backend).
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import EditableItem from "$lib/EditableItem.svelte";
  import type { ItemChange, ItemKind, OverviewItem, ProjectOverview } from "$lib/intel.svelte";

  type Meeting = { id: string; title: string; started_at_ms: number };

  let {
    projectId,
    meetings,
    onOpenMeeting,
  }: {
    projectId: string;
    /** The project's meetings, newest first. */
    meetings: Meeting[];
    onOpenMeeting: (id: string) => void;
  } = $props();

  const RECENT_MEETINGS = 5;

  let overview = $state<ProjectOverview | null>(null);
  let error = $state("");
  // Sections start open, except Done.
  let closed = $state<Set<string>>(new Set(["done"]));
  // The section whose "Add" form is open, and its draft.
  let adding = $state<ItemKind | null>(null);
  let draft = $state({ text: "", owner: "", due: "" });
  let addError = $state("");

  // Local UTC offset in minutes east, so dates match the user's calendar.
  const offsetMinutes = () => -new Date().getTimezoneOffset();

  async function load(id: string) {
    try {
      const o = await invoke<ProjectOverview>("project_overview", { projectId: id, offsetMinutes: offsetMinutes() });
      if (projectId === id) {
        overview = o;
        error = "";
      }
    } catch (e) {
      if (projectId === id) error = String(e);
    }
  }

  $effect(() => {
    const id = projectId;
    overview = null;
    adding = null;
    load(id);
  });

  const sections = $derived(
    overview
      ? ([
          { key: "commitment", heading: i18n.t.projects.openCommitments, items: overview.commitments },
          { key: "open_question", heading: i18n.t.projects.openQuestions, items: overview.openQuestions },
          { key: "decision", heading: i18n.t.projects.recentDecisions, items: overview.decisions },
          { key: "risk", heading: i18n.t.projects.activeRisks, items: overview.risks },
        ] as { key: ItemKind; heading: string; items: OverviewItem[] }[])
      : [],
  );

  function toggle(key: string) {
    const next = new Set(closed);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    closed = next;
  }

  async function change(item: OverviewItem, c: ItemChange): Promise<string> {
    try {
      if (item.manualId) await invoke("edit_project_item", { id: item.manualId, change: c });
      else if (item.meeting)
        await invoke("edit_meeting_item", { id: item.meeting.id, itemId: item.meeting.itemId, change: c });
      await load(projectId);
      return "";
    } catch (e) {
      return String(e);
    }
  }

  async function remove(item: OverviewItem): Promise<string> {
    if (!item.manualId) return change(item, { lifecycle: "withdrawn" });
    try {
      await invoke("delete_project_item", { id: item.manualId });
      await load(projectId);
      return "";
    } catch (e) {
      return String(e);
    }
  }

  function startAdd(kind: ItemKind) {
    adding = kind;
    draft = { text: "", owner: "", due: "" };
    addError = "";
    if (closed.has(kind)) toggle(kind);
  }

  async function saveAdd() {
    const kind = adding;
    if (!kind || !draft.text.trim()) return;
    try {
      await invoke("add_project_item", {
        projectId,
        kind,
        text: draft.text,
        owner: draft.owner || null,
        due: draft.due || null,
      });
      adding = null;
      await load(projectId);
    } catch (e) {
      addError = String(e);
    }
  }

  function onAddKey(e: KeyboardEvent) {
    if (e.key === "Escape") adding = null;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      saveAdd();
    }
  }

  function fmtDate(ms: number): string {
    return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  }
</script>

{#snippet item(it: OverviewItem)}
  <div class="item" class:muted={it.lifecycle === "resolved"}>
    <EditableItem item={it} onChange={(c) => change(it, c)} onDelete={() => remove(it)}>
      <p class="itext">{it.text}</p>
      <p class="imeta">
        {#if it.meeting}
          {@const m = it.meeting}
          <button class="src" title={i18n.t.projects.openMeeting} onclick={() => onOpenMeeting(m.id)}
            >{m.title} · {m.when}</button
          >
        {:else}
          <span class="src mine">{i18n.t.projects.addedByYou}</span>
        {/if}
        {#if it.owner}<span>{i18n.t.intel.owner}: {it.owner}</span>{/if}
        {#if it.due}<span>{i18n.t.intel.due}: {it.due}</span>{/if}
        {#if it.lifecycle !== "active"}<span class="lc">{i18n.t.intel.lifecycle[it.lifecycle]}</span>{/if}
      </p>
    </EditableItem>
  </div>
{/snippet}

{#snippet head(key: string, heading: string, count: number, addKind: ItemKind | null)}
  <div class="sh">
    <button class="st" aria-expanded={!closed.has(key)} onclick={() => toggle(key)}>
      <span class="chev" class:open={!closed.has(key)}>›</span>{heading} <span class="n">{count}</span>
    </button>
    {#if addKind}
      <button class="add" onclick={() => startAdd(addKind)}>+ {i18n.t.projects.add}</button>
    {/if}
  </div>
{/snippet}

<div class="overview">
  {#if error}<p class="err">{error}</p>{/if}
  {#if !overview}
    {#if !error}<p class="quiet">{i18n.t.projects.loading}</p>{/if}
  {:else}
    {#each sections as s (s.key)}
      <section class="sec">
        {@render head(s.key, s.heading, s.items.length, s.key)}
        {#if !closed.has(s.key)}
          {#if adding === s.key}
            <div class="addform">
              <!-- svelte-ignore a11y_autofocus -->
              <textarea
                rows="2"
                aria-label={i18n.t.items.text}
                placeholder={i18n.t.projects.addPlaceholder}
                bind:value={draft.text}
                autofocus
                onkeydown={onAddKey}
              ></textarea>
              <div class="fields">
                <input placeholder={i18n.t.items.owner} aria-label={i18n.t.items.owner} bind:value={draft.owner} onkeydown={onAddKey} />
                <input placeholder={i18n.t.items.due} aria-label={i18n.t.items.due} bind:value={draft.due} onkeydown={onAddKey} />
                <button class="b primary" disabled={!draft.text.trim()} onclick={saveAdd}>{i18n.t.projects.add}</button>
                <button class="b" onclick={() => (adding = null)}>{i18n.t.common.cancel}</button>
              </div>
              {#if addError}<p class="err">{addError}</p>{/if}
            </div>
          {/if}
          {#each s.items as it, n (`${it.manualId ?? `${it.meeting?.id}/${it.meeting?.itemId}`}-${n}`)}
            {@render item(it)}
          {:else}
            {#if adding !== s.key}<p class="quiet">{i18n.t.projects.nothing}</p>{/if}
          {/each}
        {/if}
      </section>
    {/each}

    <section class="sec">
      {@render head("meetings", i18n.t.projects.recentMeetings, meetings.length, null)}
      {#if !closed.has("meetings")}
        {#each meetings.slice(0, RECENT_MEETINGS) as m (m.id)}
          <button class="meeting" onclick={() => onOpenMeeting(m.id)}>
            <span class="mt">{m.title}</span><span class="md">{fmtDate(m.started_at_ms)}</span>
          </button>
        {:else}
          <p class="quiet">{i18n.t.projects.noMeetings}</p>
        {/each}
      {/if}
    </section>

    {#if overview.done.length}
      <section class="sec">
        {@render head("done", i18n.t.projects.done, overview.done.length, null)}
        {#if !closed.has("done")}
          {#each overview.done as it, n (`${it.manualId ?? `${it.meeting?.id}/${it.meeting?.itemId}`}-${n}`)}
            {@render item(it)}
          {/each}
        {/if}
      </section>
    {/if}
  {/if}
</div>

<style>
  .overview {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .sh {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 2px;
  }
  .st {
    font: inherit;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    background: none;
    border: 0;
    padding: 0;
    cursor: pointer;
  }
  .chev {
    display: inline-block;
    width: 12px;
    transition: transform 0.12s;
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .n {
    margin-left: 4px;
    font-weight: 400;
  }
  .add {
    font: inherit;
    font-size: 12px;
    color: var(--muted);
    background: none;
    border: 1px solid transparent;
    border-radius: 7px;
    padding: 2px 8px;
    cursor: pointer;
  }
  .add:hover {
    color: var(--text);
    border-color: var(--border);
  }
  .item {
    padding: 6px 0;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
  }
  .item.muted .itext {
    color: var(--muted);
    text-decoration: line-through;
  }
  .itext {
    margin: 0;
    padding-right: 28px;
    font-size: 13px;
    user-select: text;
  }
  .imeta {
    margin: 3px 0 0;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 8px;
    font-size: 11px;
    color: var(--muted);
  }
  .src {
    font: inherit;
    font-size: 10.5px;
    color: var(--accent);
    background: color-mix(in srgb, var(--border) 45%, transparent);
    border: 0;
    border-radius: 4px;
    padding: 1px 6px;
    max-width: 22rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button.src {
    cursor: pointer;
  }
  button.src:hover {
    background: color-mix(in srgb, var(--accent) 16%, transparent);
  }
  .src.mine {
    color: var(--muted);
  }
  .quiet {
    margin: 4px 0;
    font-size: 12.5px;
    color: var(--muted);
  }
  .meeting {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    padding: 6px 0;
    font: inherit;
    font-size: 13px;
    color: var(--text);
    text-align: left;
    background: none;
    border: 0;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
    cursor: pointer;
  }
  .meeting:hover .mt {
    text-decoration: underline;
  }
  .mt {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .md {
    flex: none;
    font-size: 12px;
    color: var(--muted);
  }
  .addform {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 6px 0;
  }
  .addform textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    font-size: 13px;
    resize: vertical;
  }
  .fields {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .fields input {
    flex: 1 1 110px;
    min-width: 0;
    font: inherit;
    font-size: 12.5px;
  }
  .b {
    font: inherit;
    font-size: 12.5px;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 3px 10px;
    cursor: pointer;
  }
  .b.primary {
    font-weight: 600;
  }
  .err {
    margin: 0;
    font-size: 12.5px;
    color: var(--stop);
  }
</style>

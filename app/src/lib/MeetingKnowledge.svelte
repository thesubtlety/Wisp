<script lang="ts">
  // Knowledge › From meetings: the live requirements, constraints, decisions and facts said in the
  // project's meetings that aren't project knowledge yet (the backend's `project_overview` selection,
  // deduplicated like the brief). "Keep" saves one as knowledge, citing the item's evidence in its
  // meeting. Items already kept (matched on text) are hidden; ones kept here stay, marked as kept.
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import { keepAsKnowledge, type KnowledgeCandidate, type MemoryItem, type ProjectOverview } from "$lib/intel.svelte";

  let {
    projectId,
    memory,
    onOpenMeeting,
    onKept,
  }: {
    projectId: string;
    /** The project's knowledge, to hide what is already kept. */
    memory: MemoryItem[];
    onOpenMeeting: (id: string) => void;
    onKept: () => void;
  } = $props();

  let candidates = $state<KnowledgeCandidate[]>([]);
  let error = $state("");
  let loaded = $state(false);
  let open = $state(true);
  let factsOpen = $state(false);
  // Keys of items kept from here, and of ones being saved.
  let kept = $state<Set<string>>(new Set());
  let saving = $state<Set<string>>(new Set());

  const key = (c: KnowledgeCandidate) => `${c.meeting?.id}/${c.meeting?.itemId}`;
  /** Like the backend's dedup key: case, spacing and trailing punctuation don't matter. */
  const norm = (t: string) =>
    t
      .split(/\s+/)
      .filter(Boolean)
      .join(" ")
      .replace(/[.!?。！？]+$/u, "")
      .toLowerCase();

  async function load(id: string) {
    try {
      const o = await invoke<ProjectOverview>("project_overview", {
        projectId: id,
        offsetMinutes: -new Date().getTimezoneOffset(),
      });
      if (projectId === id) {
        candidates = o.fromMeetings;
        error = "";
        loaded = true;
      }
    } catch (e) {
      if (projectId === id) error = String(e);
    }
  }

  $effect(() => {
    const id = projectId;
    candidates = [];
    loaded = false;
    kept = new Set();
    load(id);
  });

  const known = $derived(new Set(memory.map((m) => norm(m.text))));
  const visible = $derived(candidates.filter((c) => kept.has(key(c)) || !known.has(norm(c.text))));
  const groups = $derived(
    [
      { kind: "requirement", heading: i18n.t.projects.groupRequirements },
      { kind: "constraint", heading: i18n.t.projects.groupConstraints },
      { kind: "decision", heading: i18n.t.projects.groupDecisions },
    ]
      .map((g) => ({ ...g, items: visible.filter((c) => c.kind === g.kind) }))
      .filter((g) => g.items.length),
  );
  const facts = $derived(visible.filter((c) => c.kind === "fact"));

  async function keep(c: KnowledgeCandidate) {
    const k = key(c);
    saving = new Set(saving).add(k);
    error = "";
    try {
      await keepAsKnowledge(c);
      kept = new Set(kept).add(k);
      onKept();
    } catch (e) {
      error = String(e);
    } finally {
      const next = new Set(saving);
      next.delete(k);
      saving = next;
    }
  }
</script>

{#snippet row(c: KnowledgeCandidate)}
  <li class="row">
    <span class="text" class:kept={kept.has(key(c))}>{c.text}</span>
    <span class="meta">
      {#if c.meeting}
        {@const m = c.meeting}
        <button class="src" title={i18n.t.projects.openMeeting} onclick={() => onOpenMeeting(m.id)}>{m.title} · {m.when}</button>
      {/if}
      {#if kept.has(key(c))}
        <span class="done">✓ {i18n.t.projects.kept}</span>
      {:else}
        <button class="keep" title={i18n.t.projects.keepTitle} disabled={saving.has(key(c))} onclick={() => keep(c)}
          >{i18n.t.projects.keep}</button
        >
      {/if}
    </span>
  </li>
{/snippet}

<section class="from">
  <button class="st" aria-expanded={open} onclick={() => (open = !open)}>
    <span class="chev" class:open>›</span>{i18n.t.projects.fromMeetings} <span class="n">{visible.length}</span>
  </button>
  {#if open}
    <p class="quiet">{i18n.t.projects.fromMeetingsHelp}</p>
    {#if error}<p class="err">{error}</p>{/if}
    {#if loaded && !visible.length}
      <p class="quiet">{i18n.t.projects.nothingFromMeetings}</p>
    {/if}
    {#each groups as g (g.kind)}
      <h4 class="gh">{g.heading} <span class="n">{g.items.length}</span></h4>
      <ul class="list">
        {#each g.items as c (key(c))}{@render row(c)}{/each}
      </ul>
    {/each}
    {#if facts.length}
      <button class="gh toggle" aria-expanded={factsOpen} onclick={() => (factsOpen = !factsOpen)}>
        <span class="chev" class:open={factsOpen}>›</span>{i18n.t.projects.groupFacts} <span class="n">{facts.length}</span>
      </button>
      {#if factsOpen}
        <ul class="list">
          {#each facts as c (key(c))}{@render row(c)}{/each}
        </ul>
      {/if}
    {/if}
  {/if}
</section>

<style>
  .from {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 10px;
  }
  .st,
  .gh {
    font: inherit;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    background: none;
    border: 0;
    padding: 0;
    text-align: left;
  }
  .st,
  .toggle {
    cursor: pointer;
  }
  .gh {
    margin: 8px 0 0;
    font-size: 11px;
    letter-spacing: 0.03em;
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
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .row {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 6px 0;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
  }
  .text {
    font-size: 13px;
    user-select: text;
  }
  .text.kept {
    color: var(--muted);
  }
  .meta {
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
    cursor: pointer;
  }
  .src:hover {
    background: color-mix(in srgb, var(--accent) 16%, transparent);
  }
  .keep {
    font: inherit;
    font-size: 11.5px;
    color: var(--text);
    background: none;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 1px 8px;
    cursor: pointer;
  }
  .keep:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  .keep:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .done {
    font-size: 11.5px;
    color: var(--accent);
  }
  .quiet {
    margin: 0;
    font-size: 12.5px;
    color: var(--muted);
  }
  .err {
    margin: 0;
    font-size: 12.5px;
    color: var(--stop);
  }
</style>

<script lang="ts">
  // A meeting's structured state, grouped for reading: decisions, commitments, open questions,
  // risks, requirements, conflicts, then facts (collapsed). Superseded and withdrawn items sit
  // behind a toggle. Each item's evidence shows as small chips; a chip jumps to its transcript line
  // when the host can show it (`onRef`).
  import { i18n } from "$lib/i18n.svelte";
  import type { ItemKind, StateItem } from "$lib/intel.svelte";

  let {
    items,
    refLabel,
    onRef,
  }: {
    items: StateItem[];
    /** A short label for an evidence ref ("3:04"), or null when the host can't show that line. */
    refLabel?: (ref: string) => string | null;
    /** Shows the line an evidence ref points at. */
    onRef?: (ref: string) => void;
  } = $props();

  type Section = { key: string; kinds: ItemKind[]; collapsed?: boolean };
  const SECTIONS: Section[] = [
    { key: "decision", kinds: ["decision"] },
    { key: "commitment", kinds: ["commitment", "task_candidate"] },
    { key: "open_question", kinds: ["open_question"] },
    { key: "risk", kinds: ["risk"] },
    { key: "requirement", kinds: ["requirement", "constraint"] },
    { key: "conflict", kinds: ["conflict"] },
    { key: "fact", kinds: ["fact", "assumption"], collapsed: true },
    { key: "other", kinds: ["objective", "participant", "topic", "artifact"], collapsed: true },
  ];

  let showChanged = $state(false);

  const isChanged = (i: StateItem) => i.lifecycle === "superseded" || i.lifecycle === "withdrawn";
  const changed = $derived(items.filter(isChanged));
  const groups = $derived(
    SECTIONS.map((s) => ({ ...s, items: items.filter((i) => !isChanged(i) && s.kinds.includes(i.kind)) })).filter(
      (g) => g.items.length,
    ),
  );

  function heading(s: Section): string {
    const k = i18n.t.intel.kinds;
    if (s.key === "requirement") return `${k.requirement} / ${k.constraint}`;
    if (s.key === "other") return i18n.t.intel.stateOther;
    return k[s.kinds[0]];
  }

  /** The chip text: the host's label, else the ref's short form (`T12`, or "Doc" for a document). */
  function chip(ref: string): { text: string; linked: boolean } {
    const label = refLabel?.(ref) ?? null;
    if (label) return { text: label, linked: !!onRef };
    const line = /:T(\d+)$/.exec(ref);
    if (line) return { text: `T${line[1]}`, linked: false };
    return { text: /^S\d+:C\d+$/.test(ref) ? i18n.t.intel.sourceDoc : ref, linked: false };
  }

  function badge(item: StateItem): string {
    return `${i18n.t.intel.status[item.status]} · ${Math.round(item.confidence * 100)}%`;
  }
</script>

{#snippet row(item: StateItem, primary: ItemKind | null)}
  <div class="item" class:muted={item.lifecycle === "resolved"} class:gone={isChanged(item)}>
    <p class="itext">{item.text}</p>
    <p class="imeta">
      {#if primary && item.kind !== primary}<span class="tag">{i18n.t.intel.kinds[item.kind]}</span>{/if}
      <span class="st {item.status}">{badge(item)}</span>
      {#if item.lifecycle !== "active"}<span class="lc">{i18n.t.intel.lifecycle[item.lifecycle]}</span>{/if}
      {#if item.owner}<span>{i18n.t.intel.owner}: {item.owner}</span>{/if}
      {#if item.due}<span>{i18n.t.intel.due}: {item.due}</span>{/if}
      {#each item.source_refs as ref (ref)}
        {@const c = chip(ref)}
        {#if c.linked}
          <button class="ref" title={i18n.t.intel.refTip} onclick={() => onRef?.(ref)}>{c.text}</button>
        {:else}
          <span class="ref" title={ref}>{c.text}</span>
        {/if}
      {/each}
      <span class="iid">{item.id}</span>
    </p>
  </div>
{/snippet}

<div class="mstate">
  {#each groups as g (g.key)}
    <details class="group" open={!g.collapsed}>
      <summary>{heading(g)} <span class="n">{g.items.length}</span></summary>
      {#each g.items as item (item.id)}
        {@render row(item, g.kinds[0])}
      {/each}
    </details>
  {/each}
  {#if changed.length}
    <button class="toggle" onclick={() => (showChanged = !showChanged)}>
      {showChanged ? i18n.t.intel.hideChanged : i18n.t.intel.showChanged(changed.length)}
    </button>
    {#if showChanged}
      <section class="group">
        <h4>{i18n.t.intel.changedDuring}</h4>
        {#each changed as item (item.id)}
          {@render row(item, null)}
        {/each}
      </section>
    {/if}
  {/if}
</div>

<style>
  .mstate {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .group summary,
  .group h4 {
    margin: 0 0 4px;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    cursor: pointer;
  }

  .n {
    margin-left: 4px;
    font-weight: 400;
  }

  .item {
    padding: 6px 0;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
  }

  .item.muted .itext,
  .item.gone .itext {
    color: var(--muted);
    text-decoration: line-through;
  }

  .itext {
    margin: 0;
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

  .st.stated {
    color: var(--text);
  }

  .st.suggested {
    font-style: italic;
  }

  .tag {
    font-weight: 600;
  }

  .ref {
    font: inherit;
    font-family: ui-monospace, monospace;
    font-size: 10.5px;
    color: var(--muted);
    background: color-mix(in srgb, var(--border) 45%, transparent);
    border: 0;
    border-radius: 4px;
    padding: 1px 5px;
  }

  button.ref {
    color: var(--accent);
    cursor: pointer;
  }

  button.ref:hover {
    background: color-mix(in srgb, var(--accent) 16%, transparent);
  }

  .iid {
    margin-left: auto;
    font-family: ui-monospace, monospace;
  }

  .toggle {
    align-self: flex-start;
    font: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 3px 9px;
    cursor: pointer;
  }

  .toggle:hover {
    border-color: var(--accent);
  }
</style>

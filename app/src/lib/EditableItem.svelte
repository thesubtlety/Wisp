<script lang="ts" module>
  // Which item's menu is open, across every item on the page: opening one closes the others.
  const menu = $state<{ open: symbol | null }>({ open: null });
</script>

<script lang="ts">
  // An item with quiet hand-edit actions: a small ⋯ (shown on hover or focus) opens Done / Reopen,
  // Edit and Delete. Edit swaps the item for a small form (text, owner, due). The host decides what
  // each change does; a meeting's item gets it appended to its state log.
  import type { Snippet } from "svelte";
  import { i18n } from "$lib/i18n.svelte";
  import { RESOLVABLE, type ItemChange, type ItemKind, type StateItem } from "$lib/intel.svelte";

  let {
    item,
    onChange,
    onDelete,
    children,
  }: {
    item: { kind: ItemKind; text: string; owner: string | null; due: string | null; lifecycle: StateItem["lifecycle"] };
    /** Applies a change. Returns an error message, or "" on success. */
    onChange: (change: ItemChange) => Promise<string>;
    /** Deletes the item. Returns an error message, or "" on success. */
    onDelete: () => Promise<string>;
    children: Snippet;
  } = $props();

  const me = Symbol("item");
  const menuOpen = $derived(menu.open === me);
  let confirmDelete = $state(false);
  let editing = $state(false);
  let busy = $state(false);
  let error = $state("");
  let text = $state("");
  let owner = $state("");
  let due = $state("");

  const resolvable = $derived(RESOLVABLE.includes(item.kind));
  const resolved = $derived(item.lifecycle === "resolved");

  function closeMenu() {
    if (menu.open === me) menu.open = null;
    confirmDelete = false;
  }

  function startEdit() {
    closeMenu();
    text = item.text;
    owner = item.owner ?? "";
    due = item.due ?? "";
    error = "";
    editing = true;
  }

  async function run(action: () => Promise<string>) {
    busy = true;
    error = await action();
    busy = false;
    if (!error) {
      closeMenu();
      editing = false;
    }
  }

  function save() {
    const change: ItemChange = {};
    if (text.trim() !== item.text) change.text = text;
    if (owner.trim() !== (item.owner ?? "")) change.owner = owner;
    if (due.trim() !== (item.due ?? "")) change.due = due;
    if (!Object.keys(change).length) {
      editing = false;
      return;
    }
    run(() => onChange(change));
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") editing = false;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      save();
    }
  }
</script>

<svelte:window
  onclick={() => {
    if (menuOpen) closeMenu();
  }}
/>

<div class="ei" class:open={menuOpen}>
  {#if editing}
    <div class="form">
      <!-- svelte-ignore a11y_autofocus -->
      <textarea rows="2" aria-label={i18n.t.items.text} bind:value={text} autofocus onkeydown={onKey}></textarea>
      <div class="fields">
        <input placeholder={i18n.t.items.owner} aria-label={i18n.t.items.owner} bind:value={owner} onkeydown={onKey} />
        <input placeholder={i18n.t.items.due} aria-label={i18n.t.items.due} bind:value={due} onkeydown={onKey} />
        <button class="b primary" disabled={busy || !text.trim()} onclick={save}>{i18n.t.common.save}</button>
        <button class="b" onclick={() => (editing = false)}>{i18n.t.common.cancel}</button>
      </div>
    </div>
  {:else}
    {@render children()}
    <button
      class="dots"
      title={i18n.t.items.actions}
      aria-label={i18n.t.items.actions}
      aria-expanded={menuOpen}
      onclick={(e) => {
        e.stopPropagation();
        if (menuOpen) closeMenu();
        else {
          confirmDelete = false;
          menu.open = me;
        }
      }}>⋯</button
    >
    {#if menuOpen}
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <div class="menu" role="menu" tabindex="-1" onclick={(e) => e.stopPropagation()}>
        {#if confirmDelete}
          <span class="ask">{i18n.t.items.deleteConfirm}</span>
          <button class="mi danger" role="menuitem" disabled={busy} onclick={() => run(onDelete)}>{i18n.t.items.delete}</button>
          <button class="mi" role="menuitem" onclick={() => (confirmDelete = false)}>{i18n.t.common.cancel}</button>
        {:else}
          {#if resolvable}
            <button
              class="mi"
              role="menuitem"
              disabled={busy}
              onclick={() => run(() => onChange({ lifecycle: resolved ? "active" : "resolved" }))}
              >{resolved ? i18n.t.items.reopen : i18n.t.items.done}</button
            >
          {/if}
          <button class="mi" role="menuitem" onclick={startEdit}>{i18n.t.items.edit}</button>
          <button class="mi" role="menuitem" onclick={() => (confirmDelete = true)}>{i18n.t.items.delete}</button>
        {/if}
      </div>
    {/if}
  {/if}
  {#if error}<p class="err">{error}</p>{/if}
</div>

<style>
  .ei {
    position: relative;
  }
  .dots {
    position: absolute;
    top: 2px;
    right: 0;
    width: 24px;
    height: 20px;
    padding: 0;
    font: inherit;
    font-size: 14px;
    line-height: 1;
    color: var(--muted);
    background: var(--surface);
    border: 1px solid transparent;
    border-radius: 6px;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.12s;
  }
  .ei:hover .dots,
  .ei.open .dots,
  .dots:focus-visible {
    opacity: 1;
  }
  .dots:hover {
    border-color: var(--border);
    color: var(--text);
  }
  .menu {
    position: absolute;
    top: 24px;
    right: 0;
    z-index: 5;
    display: flex;
    flex-direction: column;
    min-width: 130px;
    padding: 4px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 9px;
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.12);
  }
  .mi {
    text-align: left;
    font: inherit;
    font-size: 12.5px;
    color: var(--text);
    background: none;
    border: 0;
    border-radius: 6px;
    padding: 5px 9px;
    cursor: pointer;
  }
  .mi:hover {
    background: var(--surface-active);
  }
  .mi.danger {
    color: var(--stop);
  }
  .ask {
    font-size: 12px;
    color: var(--muted);
    padding: 4px 9px 2px;
  }
  .form {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 4px 0;
  }
  .form textarea {
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
    margin: 2px 0 0;
    font-size: 12px;
    color: var(--stop);
  }
</style>

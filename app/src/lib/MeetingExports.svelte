<script lang="ts">
  // One small row of a meeting's exports (summary, record, transcript, AI context packet, JSON),
  // each with Copy and Save. Used by the live panel and the Library meeting page.
  import { i18n } from "$lib/i18n.svelte";
  import {
    EXPORT_KINDS,
    copyMeetingExport,
    saveMeetingExport,
    type ExportKind,
    type ExportTarget,
  } from "$lib/intel.svelte";

  let {
    target,
    startedAt,
    unavailable = [],
  }: {
    target: ExportTarget;
    /** When the meeting started (epoch ms), for the suggested file name. */
    startedAt: number;
    /** Exports with nothing to export yet (no summary, no transcript). */
    unavailable?: ExportKind[];
  } = $props();

  let note = $state("");
  // Which menu is open: Copy or Save, each listing the five exports.
  let open = $state<"copy" | "save" | null>(null);
  let failed = $state(false);

  async function run(kind: ExportKind, how: "copy" | "save") {
    note = "";
    failed = false;
    const name = i18n.t.intel.exportKinds[kind];
    try {
      if (how === "copy") {
        await copyMeetingExport(target, kind);
        note = i18n.t.intel.exportCopied(name);
      } else if (await saveMeetingExport(target, kind, startedAt)) {
        note = i18n.t.intel.exportSaved;
      }
    } catch (e) {
      failed = true;
      note = i18n.t.intel.exportFailed(String(e));
    }
  }
</script>

<div class="exports">
  {#each ["copy", "save"] as const as how (how)}
    <span class="xmenu">
      <button class="xbtn" aria-haspopup="menu" aria-expanded={open === how} onclick={() => (open = open === how ? null : how)}
        >{how === "copy" ? i18n.t.intel.exportCopy : i18n.t.intel.exportSave} <span class="caret">▾</span></button
      >
      {#if open === how}
        <button class="scrim" aria-label={i18n.t.common.close} onclick={() => (open = null)}></button>
        <div class="pop" role="menu">
          {#each EXPORT_KINDS as kind (kind)}
            <button
              role="menuitem"
              disabled={unavailable.includes(kind)}
              onclick={() => {
                open = null;
                run(kind, how);
              }}>{i18n.t.intel.exportKinds[kind]}</button
            >
          {/each}
        </div>
      {/if}
    </span>
  {/each}
  {#if note}<span class="xnote" class:failed>{note}</span>{/if}
</div>

<style>
  .exports {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
  }

  .xmenu {
    position: relative;
  }

  .caret {
    opacity: 0.6;
    font-size: 10px;
  }

  .scrim {
    position: fixed;
    inset: 0;
    z-index: 20;
    background: transparent;
    border: none;
    padding: 0;
    cursor: default;
  }

  .pop {
    position: absolute;
    left: 0;
    top: calc(100% + 4px);
    z-index: 21;
    min-width: 11rem;
    padding: 4px;
    background: var(--surface, var(--bg));
    border: 1px solid var(--border-strong, var(--border));
    border-radius: 8px;
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.12);
  }

  .pop button {
    display: block;
    width: 100%;
    text-align: left;
    border: none;
    padding: 5px 9px;
  }

  .pop button:hover:not(:disabled) {
    background: var(--bg);
  }

  button {
    font: inherit;
    font-size: 11.5px;
    color: var(--text);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 2px 7px;
    cursor: pointer;
  }

  button:hover:not(:disabled) {
    border-color: var(--accent);
  }

  button:disabled {
    cursor: default;
  }

  .xnote {
    font-size: 12px;
    color: var(--muted);
  }

  .xnote.failed {
    color: var(--danger, #c0392b);
  }
</style>

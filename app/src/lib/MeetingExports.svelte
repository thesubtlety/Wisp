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
  {#each EXPORT_KINDS as kind (kind)}
    {@const off = unavailable.includes(kind)}
    <span class="xg" class:off>
      <span class="xl">{i18n.t.intel.exportKinds[kind]}</span>
      <button disabled={off} title={i18n.t.intel.exportCopy} onclick={() => run(kind, "copy")}
        >{i18n.t.intel.exportCopy}</button
      >
      <button disabled={off} title={i18n.t.intel.exportSave} onclick={() => run(kind, "save")}
        >{i18n.t.intel.exportSave}</button
      >
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

  .xg {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    font-size: 12px;
  }

  .xg.off {
    opacity: 0.5;
  }

  .xl {
    color: var(--muted);
    margin-right: 2px;
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

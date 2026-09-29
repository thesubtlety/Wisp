<script lang="ts">
  // A screenshot's thumbnail; a click opens it full size. The image bytes come from the backend
  // once and live in a blob URL that is revoked when the thumbnail goes away.
  import { onDestroy } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";

  let { sourceId, title }: { sourceId: number; title: string } = $props();

  let url = $state("");
  let failed = $state(false);
  let open = $state(false);
  let gone = false;

  /** The image type from its first bytes, so the blob renders without guessing. */
  function mime(b: Uint8Array): string {
    if (b[0] === 0x89 && b[1] === 0x50) return "image/png";
    if (b[0] === 0xff && b[1] === 0xd8) return "image/jpeg";
    if (b[0] === 0x47 && b[1] === 0x49) return "image/gif";
    if (b[8] === 0x57 && b[9] === 0x45) return "image/webp";
    return "application/octet-stream";
  }

  async function load(id: number) {
    try {
      const bytes = new Uint8Array(await invoke<ArrayBuffer>("context_image", { sourceId: id }));
      if (gone) return;
      url = URL.createObjectURL(new Blob([bytes], { type: mime(bytes) }));
    } catch {
      failed = true;
    }
  }

  $effect(() => {
    load(sourceId);
  });

  onDestroy(() => {
    gone = true;
    if (url) URL.revokeObjectURL(url);
  });
</script>

<button
  class="thumb"
  class:failed
  disabled={!url}
  title={failed ? i18n.t.intel.imageMissing : i18n.t.intel.enlarge}
  aria-label={i18n.t.intel.enlarge}
  onclick={() => (open = true)}
>
  {#if url}
    <img src={url} alt={title} />
  {:else if failed}
    <span>?</span>
  {/if}
</button>

<Modal bind:open {title} wide>
  {#if url}
    <img class="full" src={url} alt={title} />
  {/if}
</Modal>

<style>
  .thumb {
    flex: none;
    width: 56px;
    height: 40px;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--surface-active);
    overflow: hidden;
    cursor: zoom-in;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--muted);
    font-size: 12px;
  }

  .thumb:disabled {
    cursor: default;
  }

  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }

  .full {
    display: block;
    max-width: 100%;
    max-height: 72vh;
    margin: 0 auto;
    object-fit: contain;
  }
</style>

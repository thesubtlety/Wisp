<script module lang="ts">
  const ASSIST_MIN = 320;
  const TRANSCRIPT_MIN = 360;

  /** The panel width to start this page load with: the last one dragged to (persisted), at least the minimum. */
  export function savedAssistWidth(): number {
    return Math.max(ASSIST_MIN, Number(localStorage.getItem("wisp.assistWidth")) || 440);
  }
</script>

<script lang="ts">
  // The AI assist's docked panel, shared by Live and File: it sits right of the transcript with a
  // title, a close button, and the assist as its body. The page binds `width`, so both modes share one
  // width; `container` is the body the panel docks in, which bounds how wide a drag can make it.
  import type { Snippet } from "svelte";
  import { i18n } from "$lib/i18n.svelte";

  let {
    title,
    width = $bindable(),
    container,
    onclose,
    children,
  }: {
    title: string;
    width: number;
    container: HTMLElement | null;
    onclose: () => void;
    children: Snippet;
  } = $props();

  // Drag the splitter to resize the assist panel: pulling left widens it. Clamped so neither side
  // gets too thin; the chosen width persists. The whole pane grows with the window (.app widens).
  function startResize(e: MouseEvent) {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width;
    const maxW = container ? Math.max(ASSIST_MIN, container.clientWidth - TRANSCRIPT_MIN) : 9999;
    const onMove = (ev: MouseEvent) => {
      width = Math.min(maxW, Math.max(ASSIST_MIN, startW - (ev.clientX - startX)));
    };
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      localStorage.setItem("wisp.assistWidth", String(Math.round(width)));
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }
</script>

<aside class="assist-panel" style:width="{width}px">
  <button class="assist-resize" aria-label={i18n.t.transcript.resizeAssist} onmousedown={startResize}></button>
  <div class="assist-head">
    <span class="assist-title">{title}</span>
    <button class="assist-x" aria-label={i18n.t.common.close} onclick={onclose}>×</button>
  </div>
  <div class="assist-body">{@render children()}</div>
</aside>

<style>
  .assist-panel {
    position: relative;
    flex: none;
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-left: 1px solid var(--border);
    background: var(--surface);
  }

  /* Drag strip on the panel's left edge — pull left to widen, right to narrow. */
  .assist-resize {
    position: absolute;
    top: 0;
    bottom: 0;
    left: -3px;
    width: 7px;
    padding: 0;
    border: none;
    background: transparent;
    cursor: col-resize;
    z-index: 5;
  }

  .assist-resize:hover {
    background: color-mix(in srgb, var(--accent) 22%, transparent);
  }

  .assist-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
  }

  .assist-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
  }

  .assist-x {
    font-size: 18px;
    line-height: 1;
    color: var(--muted);
    background: transparent;
    border: none;
    cursor: pointer;
    padding: 0 4px;
  }

  .assist-x:hover {
    color: var(--text);
  }

  .assist-body {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
</style>

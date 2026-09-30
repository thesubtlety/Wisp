<script lang="ts">
  // The Library — a browsable, searchable archive of finished notes. Reads from the SQLite-backed
  // store via the note commands; search uses the backend's full-text index (with a short-CJK
  // substring fallback). List ⇄ meeting page in one view; deletes go through a confirm modal.
  // Projects have their own view; here each meeting just shows its project as a chip.
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";
  import MeetingPage from "$lib/MeetingPage.svelte";
  import { intel, loadProjects } from "$lib/intel.svelte";

  type NoteSummary = {
    id: string;
    title: string;
    started_at_ms: number;
    duration_ms: number;
    language: string | null;
    engine: string | null;
    preview: string;
    project_id: string | null;
  };
  type SearchHit = {
    meeting_id: string;
    title: string;
    started_at_ms: number;
    snippet: string;
    score: number;
  };

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let hits = $state<SearchHit[] | null>(null); // null = not searching; [] = searched with no results
  let openId = $state<string | null>(null);
  let error = $state("");
  let pendingDelete = $state<string | null>(null);
  let confirmOpen = $state(false);
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  let searching = $state(false); // a search invoke is in flight
  let searchMode = $state("fulltext"); // the active search logic (mirrors Settings → Notes search)

  function projectName(id: string | null): string {
    return (id && intel.projects.find((p) => p.id === id)?.name) || "";
  }

  async function loadList() {
    try {
      notes = await invoke<NoteSummary[]>("list_library_notes");
      searchMode = await invoke<string>("search_mode");
      error = "";
    } catch (e) {
      error = String(e);
    }
  }

  /** After a change on the meeting page: the list (and any search) may show it differently. */
  async function refresh() {
    await loadList();
    if (query.trim()) onSearchInput();
  }

  onMount(async () => {
    loadList();
    await loadProjects();
  });

  // Debounced so each keystroke doesn't hit the database.
  function onSearchInput() {
    clearTimeout(searchTimer);
    const q = query.trim();
    if (!q) {
      hits = null;
      searching = false;
      return;
    }
    searching = true;
    searchTimer = setTimeout(async () => {
      try {
        // Re-read the mode so the result badges match the logic the backend actually used.
        searchMode = await invoke<string>("search_mode");
        hits = await invoke<SearchHit[]>("search_library", { query: q, limit: 50 });
        error = "";
      } catch (e) {
        error = String(e);
      }
      searching = false;
    }, 200);
  }

  function openNote(id: string) {
    openId = id;
  }

  function askDelete(id: string) {
    pendingDelete = id;
    confirmOpen = true;
  }

  async function doDelete() {
    const id = pendingDelete;
    confirmOpen = false;
    pendingDelete = null;
    if (!id) return;
    try {
      await invoke<boolean>("delete_library_note", { id });
      if (openId === id) openId = null;
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  function fmtDate(ms: number): string {
    return new Date(ms).toLocaleString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function fmtDuration(ms: number): string {
    const total = Math.round(ms / 1000);
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${m}:${s.toString().padStart(2, "0")}`;
  }

  function modeLabel(m: string): string {
    if (m === "semantic") return i18n.t.settings.modeSemantic;
    if (m === "hybrid") return i18n.t.settings.modeHybrid;
    return i18n.t.settings.modeFulltext;
  }

  // The FTS snippet wraps matches in «…»; escape the (user-content) text, then render those as <mark>.
  function renderSnippet(s: string): string {
    const escaped = s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
    return escaped.replaceAll("«", "<mark>").replaceAll("»", "</mark>");
  }
</script>

<section class="library">
  {#if openId}
    <MeetingPage
      id={openId}
      onBack={() => (openId = null)}
      onChanged={refresh}
      onDeleted={() => refresh()}
    />
  {:else}
    <header class="lib-head">
      <h2 class="lib-h2">{i18n.t.library.title}</h2>
      <input
        class="search"
        type="search"
        placeholder={i18n.t.library.searchPlaceholder}
        bind:value={query}
        oninput={onSearchInput}
      />
    </header>

    {#if error}
      <div class="err">{error}</div>
    {/if}

    {#if hits !== null || searching}
      <div class="search-bar">
        <span class="searchby">{i18n.t.library.searchBy}</span>
        <span class="mode-chip {searchMode}">{modeLabel(searchMode)}</span>
        <span class="mode-hint">· {i18n.t.library.searchModeHint}</span>
        {#if searching}
          <span class="searching"><span class="spin"></span>{i18n.t.library.searching}</span>
        {/if}
      </div>
    {/if}

    {#if hits !== null}
      {#if hits.length === 0}
        <div class="empty">{searching ? i18n.t.library.searching : i18n.t.library.noResults}</div>
      {:else}
        <ul class="cards" class:loading={searching}>
          {#each hits as hit (hit.meeting_id)}
            <li>
              <button class="card hit-card" onclick={() => openNote(hit.meeting_id)}>
                <div class="card-top">
                  <span class="card-title">{hit.title}</span>
                  <span class="card-date">{fmtDate(hit.started_at_ms)}</span>
                </div>
                <!-- eslint-disable-next-line svelte/no-at-html-tags -->
                <div class="snippet">{@html renderSnippet(hit.snippet)}</div>
                <div class="match">
                  {#if searchMode === "semantic"}
                    <span class="match-badge sem">{Math.min(100, Math.round(hit.score * 100))}% · {i18n.t.library.matchSemantic}</span>
                  {:else if searchMode === "hybrid"}
                    <span class="match-badge hyb">{i18n.t.library.matchHybrid}</span>
                  {:else}
                    <span class="match-badge kw">{i18n.t.library.matchKeyword}</span>
                  {/if}
                </div>
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    {:else if searching}
      <div class="empty">{i18n.t.library.searching}</div>
    {:else if notes.length === 0}
      <div class="empty">{i18n.t.library.empty}</div>
    {:else}
      <ul class="cards">
        {#each notes as m (m.id)}
          <li class="row">
            <button class="card card-main" onclick={() => openNote(m.id)}>
              <div class="card-top">
                <span class="card-title">{m.title}</span>
                <span class="card-date">{fmtDate(m.started_at_ms)}</span>
              </div>
              <div class="card-sub">
                {fmtDuration(m.duration_ms)}{m.engine ? ` · ${m.engine}` : ""}
                {#if projectName(m.project_id)}<span class="chip">{projectName(m.project_id)}</span>{/if}
              </div>
              {#if m.preview}<div class="preview">{m.preview}</div>{/if}
            </button>
            <div class="row-actions">
              <button
                class="action"
                onclick={() => askDelete(m.id)}
                title={i18n.t.library.delete}
                aria-label={i18n.t.library.delete}
              >
                <svg
                  width="15"
                  height="15"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.6"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12" />
                </svg>
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}

  <Modal bind:open={confirmOpen} title={i18n.t.library.deleteTitle}>
    <p class="confirm-text">{i18n.t.library.deleteConfirm}</p>
    <div class="confirm-actions">
      <button class="btn" onclick={() => (confirmOpen = false)}>{i18n.t.library.cancel}</button>
      <button class="btn danger" onclick={doDelete}>{i18n.t.library.delete}</button>
    </div>
  </Modal>




</section>

<style>
  .chip {
    margin-left: 8px;
    padding: 1px 7px;
    border-radius: 999px;
    border: 1px solid currentColor;
    opacity: 0.7;
    font-size: 0.75rem;
  }
  .library {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 24px clamp(16px, 4vw, 48px);
    overflow-y: auto;
  }

  .lib-head {
    display: flex;
    align-items: center;
    gap: 14px;
    flex: none;
  }

  .lib-h2 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
    color: var(--text);
  }

  .search {
    margin-left: auto;
    width: min(360px, 50%);
    padding: 9px 13px;
    font: inherit;
    font-size: 14px;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    outline: none;
  }
  .search:focus {
    border-color: var(--accent);
  }

  .cards {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  /* A row is just a positioning context; the card is the whole box, the actions overlay its right. */
  .row {
    position: relative;
  }

  .card {
    width: 100%;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    text-align: left;
    padding: 14px 16px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    cursor: pointer;
    font: inherit;
    color: var(--text);
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .card:hover {
    border-color: var(--border-strong);
    background: var(--surface-active);
  }

  /* Reserve the right gutter so a long title never slides under the hover actions. */
  .card-main {
    padding-right: 48px;
  }

  .card-top {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }
  .card-title {
    font-size: 14.5px;
    font-weight: 600;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .card-date {
    flex: none;
    font-size: 12px;
    color: var(--muted);
  }
  .card-sub {
    font-size: 12px;
    color: var(--muted);
  }
  .preview,
  .snippet {
    font-size: 13px;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .snippet :global(mark) {
    background: color-mix(in srgb, var(--accent) 26%, transparent);
    color: var(--text);
    border-radius: 3px;
    padding: 0 1px;
  }

  /* Minimal actions column tucked into the card's right edge — only revealed on hover/focus. */
  .row-actions {
    position: absolute;
    top: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    padding-right: 9px;
    opacity: 0;
    transition: opacity 0.12s;
    pointer-events: none;
  }
  .row:hover .row-actions,
  .row:focus-within .row-actions {
    opacity: 1;
    pointer-events: auto;
  }

  .action {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
    transition:
      background 0.12s,
      color 0.12s;
  }
  .action:hover {
    background: color-mix(in srgb, var(--stop) 14%, transparent);
    color: var(--stop);
  }

  .empty {
    margin: 32px auto;
    max-width: 360px;
    text-align: center;
    font-size: 14px;
    color: var(--muted);
  }
  .err {
    font-size: 13px;
    color: var(--stop);
  }

  .confirm-text {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--text);
  }
  .confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .btn {
    padding: 8px 15px;
    font: inherit;
    font-size: 13px;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 9px;
    cursor: pointer;
  }
  .btn:hover {
    border-color: var(--border-strong);
  }
  .btn.danger {
    color: #fff;
    background: var(--stop);
    border-color: var(--stop);
  }
  .btn.danger:hover {
    filter: brightness(1.05);
  }

  /* ── Search logic transparency ── */
  .search-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
    font-size: 12px;
    color: var(--muted);
  }
  .searchby {
    color: var(--muted);
  }
  .mode-chip {
    font-weight: 600;
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    border-radius: 999px;
    padding: 2px 9px;
  }
  .mode-chip.fulltext {
    color: var(--live);
    background: color-mix(in srgb, var(--live) 14%, transparent);
  }
  .mode-hint {
    color: var(--muted);
  }
  .searching {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
  }
  .spin {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: lib-spin 0.7s linear infinite;
  }
  @keyframes lib-spin {
    to {
      transform: rotate(360deg);
    }
  }
  .cards.loading {
    opacity: 0.55;
    transition: opacity 0.15s;
  }

  /* Per-result "how it matched" badge. */
  .match {
    margin-top: 2px;
  }
  .match-badge {
    font-family: var(--font-mono);
    font-size: 10.5px;
    letter-spacing: 0.02em;
    padding: 1px 7px;
    border-radius: 999px;
    color: var(--muted);
    background: var(--bg);
    border: 1px solid var(--border);
  }
  .match-badge.sem {
    color: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 45%, transparent);
  }
  .match-badge.kw {
    color: var(--live);
    border-color: color-mix(in srgb, var(--live) 45%, transparent);
  }
</style>

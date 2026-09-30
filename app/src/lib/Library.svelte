<script lang="ts">
  // The Library — a browsable, searchable archive of finished notes. Reads from the SQLite-backed
  // store via the note commands; search uses the backend's full-text index (with a short-CJK
  // substring fallback). List ⇄ detail in one view; deletes go through a confirm modal.
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";
  import ShotThumb from "$lib/ShotThumb.svelte";
  import { intel, loadProjects, createProject, renameProject, type MemoryItem } from "$lib/intel.svelte";
  import { copyText } from "$lib/clipboard";

  // "New meeting in this project": the page switches to Live with the project selected.
  // Hidden while a session runs: switching projects then would refile the running meeting.
  let { onNewMeeting, sessionRunning = false }: { onNewMeeting?: (projectId: string) => void; sessionRunning?: boolean } =
    $props();

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
  type Note = {
    id: string;
    title: string;
    started_at_ms: number;
    duration_ms: number;
    language: string | null;
    engine: string | null;
    summary: string | null;
    segment_count: number;
    project_id: string | null;
  };
  type Segment = {
    idx: number;
    start_ms: number;
    end_ms: number;
    speaker: number | null;
    source: string;
    text: string;
  };
  // `speakerNames` maps a diarized speaker id (0-based) to the name the user gave it.
  type Detail = { meeting: Note; segments: Segment[]; speakerNames: Record<number, string> };

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let hits = $state<SearchHit[] | null>(null); // null = not searching; [] = searched with no results
  let detail = $state<Detail | null>(null);
  let error = $state("");
  let pendingDelete = $state<string | null>(null);
  let confirmOpen = $state(false);
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  let searching = $state(false); // a search invoke is in flight
  let searchMode = $state("fulltext"); // the active search logic (mirrors Settings → Notes search)

  // Which meetings the list shows: "" all, NO_PROJECT the unfiled ones, else one project's.
  const NO_PROJECT = "__none";
  const FILTER_KEY = "wisp.libraryProject";
  let projectFilter = $state(readFilter());
  const shown = $derived(
    projectFilter === ""
      ? notes
      : notes.filter((n) => (projectFilter === NO_PROJECT ? !n.project_id : n.project_id === projectFilter)),
  );
  const shownIds = $derived(new Set(shown.map((n) => n.id)));
  const shownHits = $derived((hits ?? []).filter((h) => projectFilter === "" || shownIds.has(h.meeting_id)));
  const filterProject = $derived(intel.projects.find((p) => p.id === projectFilter));
  let memory = $state<MemoryItem[]>([]);
  type ProjectShot = {
    sourceId: number;
    label: string;
    title: string | null;
    snippet: string | null;
    addedAtMs: number;
    expiresAtMs: number | null;
  };
  let shots = $state<ProjectShot[]>([]);
  let pendingShot = $state<ProjectShot | null>(null);
  let shotConfirmOpen = $state(false);
  let renamingProject = $state(false);
  let projectDraft = $state("");
  let projectError = $state("");
  // The selected project's instructions: what matters to the user there, given to its meetings.
  let instructions = $state("");
  let editingInstructions = $state(false);
  let instructionsDraft = $state("");

  // Detail-view editing: the title, and which project the meeting is filed under.
  let editingTitle = $state(false);
  let titleDraft = $state("");
  let newProjectOpen = $state(false);
  let newProjectName = $state("");
  let moveNote = $state("");
  // After a move: knowledge learned only from this meeting can follow it to the new project.
  type KnowledgeMove = { id: string; from: string; to: string; count: number; shared: number };
  let knowledgeMove = $state<KnowledgeMove | null>(null);
  let knowledgeOpen = $state(false);

  // The project brief: rendered by the backend, previewed read-only, copied or saved as .md.
  let briefOpen = $state(false);
  let briefText = $state("");
  let briefError = $state("");
  let briefCopied = $state(false);

  // Speaker names: click a speaker to rename it everywhere in the meeting, or merge it into another
  // (diarization sometimes splits one person in two). Colours match the Live feed.
  const SPEAKER_COLORS = ["#c96442", "#3f7e6b", "#6a5acd", "#b58a2e", "#9c4d6b", "#4a7aa8"];
  const speakerColor = (n: number) => SPEAKER_COLORS[n % SPEAKER_COLORS.length];
  const speakerIds = $derived(
    detail
      ? [...new Set(detail.segments.map((s) => s.speaker).filter((n): n is number => n !== null))].sort(
          (a, b) => a - b,
        )
      : [],
  );
  let editingSpeaker = $state<number | null>(null);
  let speakerDraft = $state("");
  let pendingMerge = $state<{ from: number; into: number } | null>(null);
  let mergeOpen = $state(false);

  function diarizedLabel(n: number): string {
    return detail?.speakerNames[n] || i18n.t.common.speaker(n + 1);
  }

  function startSpeakerEdit(n: number) {
    editingSpeaker = n;
    speakerDraft = detail?.speakerNames[n] ?? "";
  }

  async function saveSpeakerName() {
    if (!detail || editingSpeaker === null) return;
    const speaker = editingSpeaker;
    editingSpeaker = null;
    const name = speakerDraft.trim();
    if ((detail.speakerNames[speaker] ?? "") === name) return;
    try {
      await invoke("set_library_speaker_name", { id: detail.meeting.id, speaker, name });
      const next = { ...detail.speakerNames };
      if (name) next[speaker] = name;
      else delete next[speaker];
      detail.speakerNames = next;
    } catch (e) {
      error = String(e);
    }
  }

  function askMerge(into: string) {
    if (editingSpeaker === null || into === "") return;
    pendingMerge = { from: editingSpeaker, into: Number(into) };
    mergeOpen = true;
  }

  async function doMerge() {
    const merge = pendingMerge;
    mergeOpen = false;
    pendingMerge = null;
    if (!merge || !detail) return;
    try {
      await invoke<number>("merge_library_speaker", { id: detail.meeting.id, from: merge.from, into: merge.into });
      await openNote(detail.meeting.id);
    } catch (e) {
      error = String(e);
    }
  }

  function readFilter(): string {
    try {
      return localStorage.getItem(FILTER_KEY) ?? "";
    } catch {
      return "";
    }
  }

  function setFilter(v: string) {
    projectFilter = v;
    renamingProject = false;
    editingInstructions = false;
    projectError = "";
    try {
      localStorage.setItem(FILTER_KEY, v);
    } catch {
      // per-device convenience only
    }
  }

  // The selected project's knowledge, shown above its meetings.
  function loadProjectMemory(id: string) {
    invoke<MemoryItem[]>("list_project_memory", { projectId: id })
      .then((m) => {
        if (projectFilter === id) memory = m;
      })
      .catch(() => {});
  }

  $effect(() => {
    const id = filterProject?.id;
    memory = [];
    if (id) loadProjectMemory(id);
  });

  // Local UTC offset in minutes east, so the brief's dates match the user's calendar.
  const offsetMinutes = () => -new Date().getTimezoneOffset();

  async function openBrief() {
    const id = filterProject?.id;
    if (!id) return;
    briefText = "";
    briefError = "";
    briefCopied = false;
    briefOpen = true;
    try {
      const text = await invoke<string>("project_brief_markdown", { projectId: id, offsetMinutes: offsetMinutes() });
      if (projectFilter === id) briefText = text;
    } catch (e) {
      briefError = String(e);
    }
  }

  async function copyBrief() {
    await copyText(briefText);
    briefCopied = true;
    setTimeout(() => (briefCopied = false), 1500);
  }

  async function saveBrief() {
    const project = filterProject;
    if (!project) return;
    const stamp = new Date().toISOString().slice(0, 10);
    const safe = project.name.replace(/[\\/:*?"<>|]+/g, "-").trim() || "project";
    try {
      await invoke<boolean>("project_brief_save", {
        projectId: project.id,
        offsetMinutes: offsetMinutes(),
        defaultName: `${safe}-brief-${stamp}`,
      });
    } catch (e) {
      briefError = String(e);
    }
  }

  $effect(() => {
    const id = filterProject?.id;
    instructions = "";
    if (!id) return;
    invoke<string>("get_project_instructions", { id })
      .then((text) => {
        if (projectFilter === id) instructions = text;
      })
      .catch(() => {});
  });

  async function saveInstructions() {
    const id = filterProject?.id;
    if (!id) return;
    try {
      await invoke("set_project_instructions", { id, instructions: instructionsDraft });
      instructions = instructionsDraft.trim();
      editingInstructions = false;
      projectError = "";
    } catch (e) {
      projectError = String(e);
    }
  }

  // The selected project's screenshots.
  async function loadShots(id: string) {
    try {
      const list = await invoke<ProjectShot[]>("list_project_screenshots", { projectId: id });
      if (projectFilter === id) shots = list;
    } catch {
      // the section just stays empty
    }
  }

  $effect(() => {
    const id = filterProject?.id;
    shots = [];
    if (id) loadShots(id);
  });

  function askDeleteShot(shot: ProjectShot) {
    pendingShot = shot;
    shotConfirmOpen = true;
  }

  async function doDeleteShot() {
    const shot = pendingShot;
    shotConfirmOpen = false;
    pendingShot = null;
    if (!shot) return;
    try {
      await invoke("remove_context", { sourceId: shot.sourceId });
      shots = shots.filter((s) => s.sourceId !== shot.sourceId);
    } catch (e) {
      error = String(e);
    }
  }

  function projectName(id: string | null): string {
    return (id && intel.projects.find((p) => p.id === id)?.name) || "";
  }

  async function saveProjectName() {
    if (!filterProject) return;
    const name = projectDraft.trim();
    if (!name || name === filterProject.name) {
      renamingProject = false;
      return;
    }
    projectError = await renameProject(filterProject.id, name);
    if (!projectError) renamingProject = false;
  }

  function startTitleEdit() {
    if (!detail) return;
    titleDraft = detail.meeting.title;
    editingTitle = true;
  }

  async function saveTitle() {
    if (!detail || !editingTitle) return;
    editingTitle = false;
    const title = titleDraft.trim();
    if (!title || title === detail.meeting.title) return;
    try {
      await invoke<boolean>("rename_note", { id: detail.meeting.id, title });
      detail.meeting.title = title;
      await loadList();
      if (query.trim()) onSearchInput();
    } catch (e) {
      error = String(e);
    }
  }

  async function moveTo(projectId: string, select?: HTMLSelectElement) {
    if (!detail) return;
    if (projectId === "__new") {
      newProjectOpen = true;
      return;
    }
    const from = detail.meeting.project_id;
    const id = detail.meeting.id;
    try {
      await invoke<boolean>("set_note_project", { id, projectId: projectId || null });
      detail.meeting.project_id = projectId || null;
      moveNote = "";
      await loadList();
    } catch (e) {
      error = String(e);
      if (select) select.value = from ?? "";
      return;
    }
    if (!from || from === projectId) return;
    // Knowledge needs a project, so moving out of all projects leaves it where it was learned.
    if (!projectId) {
      moveNote = i18n.t.library.movedKeepsKnowledge(projectName(from));
      return;
    }
    try {
      const k = await invoke<{ exclusive: number; shared: number }>("note_knowledge", { id, projectId: from });
      if (k.exclusive > 0) {
        knowledgeMove = { id, from, to: projectId, count: k.exclusive, shared: k.shared };
        knowledgeOpen = true;
      } else if (k.shared > 0) {
        moveNote = i18n.t.library.sharedKnowledgeStays(k.shared, projectName(from));
      }
    } catch {
      moveNote = i18n.t.library.movedKeepsKnowledge(projectName(from));
    }
  }

  async function answerKnowledgeMove(move: boolean) {
    const m = knowledgeMove;
    knowledgeOpen = false;
    knowledgeMove = null;
    if (!m) return;
    const stays = m.shared > 0 ? " " + i18n.t.library.sharedKnowledgeStays(m.shared, projectName(m.from)) : "";
    if (!move) {
      moveNote = i18n.t.library.movedKeepsKnowledge(projectName(m.from));
      return;
    }
    try {
      const n = await invoke<number>("move_note_knowledge", { id: m.id, fromProject: m.from, toProject: m.to });
      moveNote = i18n.t.library.knowledgeMoved(n, projectName(m.to)) + stays;
      if (filterProject && (filterProject.id === m.from || filterProject.id === m.to)) loadProjectMemory(filterProject.id);
    } catch (e) {
      error = String(e);
    }
  }

  async function createAndMove() {
    const name = newProjectName.trim();
    if (!name) return;
    const err = await createProject(name, false);
    if (err) {
      error = err;
      return;
    }
    newProjectOpen = false;
    newProjectName = "";
    const created = intel.projects.find((p) => p.name === name);
    if (created) await moveTo(created.id);
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

  onMount(async () => {
    loadList();
    await loadProjects();
    // A remembered filter whose project was deleted falls back to all meetings.
    if (projectFilter && projectFilter !== NO_PROJECT && !filterProject) setFilter("");
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

  async function openNote(id: string) {
    editingTitle = false;
    editingSpeaker = null;
    newProjectOpen = false;
    moveNote = "";
    error = "";
    try {
      detail = await invoke<Detail | null>("get_library_note", { id });
      error = "";
    } catch (e) {
      error = String(e);
    }
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
      if (detail?.meeting.id === id) detail = null;
      await loadList();
      if (query.trim()) onSearchInput();
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

  // Wall-clock time of a segment (note start + offset), shown on hover of its timecode.
  function fmtClock(ms: number): string {
    return new Date(ms).toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  }

  function modeLabel(m: string): string {
    if (m === "semantic") return i18n.t.settings.modeSemantic;
    if (m === "hybrid") return i18n.t.settings.modeHybrid;
    return i18n.t.settings.modeFulltext;
  }

  function speakerLabel(source: string): string {
    if (source === "mic") return i18n.t.library.you;
    if (source === "system") return i18n.t.library.them;
    return "";
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
  {#if detail}
    <header class="detail-head">
      <div class="detail-bar">
        <button class="back" onclick={() => (detail = null)}>← {i18n.t.library.back}</button>
        <button class="del-btn" onclick={() => askDelete(detail!.meeting.id)}>
          {i18n.t.library.delete}
        </button>
      </div>
      <div class="lib-titles">
        {#if editingTitle}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="title-input"
            aria-label={i18n.t.library.renameMeeting}
            bind:value={titleDraft}
            autofocus
            onkeydown={(e) => {
              if (e.key === "Enter") saveTitle();
              if (e.key === "Escape") editingTitle = false;
            }}
            onblur={saveTitle}
          />
        {:else}
          <button class="title-btn" title={i18n.t.library.renameMeeting} onclick={startTitleEdit}>
            <h2 class="lib-h2">{detail.meeting.title}</h2>
          </button>
        {/if}
        <span class="lib-meta">
          {fmtDate(detail.meeting.started_at_ms)} · {fmtDuration(detail.meeting.duration_ms)}{detail
            .meeting.engine
            ? ` · ${detail.meeting.engine}`
            : ""}
        </span>
        <span class="project-row">
          <span class="project-label">{i18n.t.library.project}</span>
          {#if newProjectOpen}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              placeholder={i18n.t.library.projectName}
              bind:value={newProjectName}
              autofocus
              onkeydown={(e) => {
                if (e.key === "Enter") createAndMove();
                if (e.key === "Escape") newProjectOpen = false;
              }}
            />
            <button class="btn" disabled={!newProjectName.trim()} onclick={createAndMove}>{i18n.t.library.create}</button>
            <button class="btn" onclick={() => (newProjectOpen = false)}>{i18n.t.library.cancel}</button>
          {:else}
            <select
              aria-label={i18n.t.library.project}
              value={detail.meeting.project_id ?? ""}
              onchange={(e) => moveTo(e.currentTarget.value, e.currentTarget)}
            >
              <option value="">{i18n.t.library.noProject}</option>
              {#each intel.projects as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
              <option value="__new">{i18n.t.library.newProject}</option>
            </select>
          {/if}
        </span>
        {#if speakerIds.length}
          <span class="project-row">
            <span class="project-label">{i18n.t.library.speakers}</span>
            {#each speakerIds as n (n)}
              <button
                class="spk-chip"
                class:active={editingSpeaker === n}
                style="--spk: {speakerColor(n)}"
                title={i18n.t.library.speakerTip}
                onclick={() => startSpeakerEdit(n)}>{diarizedLabel(n)}</button
              >
            {/each}
          </span>
          {#if editingSpeaker !== null}
            {@const editing = editingSpeaker}
            <span class="project-row">
              <!-- svelte-ignore a11y_autofocus -->
              <input
                aria-label={i18n.t.library.speakerName}
                placeholder={i18n.t.common.speaker(editing + 1)}
                bind:value={speakerDraft}
                autofocus
                onkeydown={(e) => {
                  if (e.key === "Enter") saveSpeakerName();
                  if (e.key === "Escape") editingSpeaker = null;
                }}
              />
              <button class="btn" onclick={saveSpeakerName}>{i18n.t.library.save}</button>
              {#if speakerIds.length > 1}
                <select
                  aria-label={i18n.t.library.mergeInto}
                  value=""
                  onchange={(e) => {
                    askMerge(e.currentTarget.value);
                    e.currentTarget.value = "";
                  }}
                >
                  <option value="">{i18n.t.library.mergeInto}</option>
                  {#each speakerIds.filter((n) => n !== editing) as n (n)}
                    <option value={String(n)}>{diarizedLabel(n)}</option>
                  {/each}
                </select>
              {/if}
              <button class="btn" onclick={() => (editingSpeaker = null)}>{i18n.t.library.cancel}</button>
            </span>
            <span class="move-note">{i18n.t.library.speakerNameHint}</span>
          {/if}
        {/if}
        {#if moveNote}<span class="move-note">{moveNote}</span>{/if}
        {#if error}<div class="err">{error}</div>{/if}
      </div>
    </header>

    {#if detail.meeting.summary}
      <div class="summary">{detail.meeting.summary}</div>
    {/if}

    <div class="transcript">
      {#each detail.segments as seg (seg.idx)}
        <p class="seg">
          <span class="ts" title={fmtClock(detail!.meeting.started_at_ms + seg.start_ms)}>{fmtDuration(seg.start_ms)}</span>
          <span class="seg-body">
            {#if speakerLabel(seg.source)}<span class="spk" class:them={seg.source === "system"}
                >{speakerLabel(seg.source)}</span
              >{/if}{#if seg.speaker !== null}{@const n = seg.speaker}<button
                class="spk dia"
                style="--spk: {speakerColor(n)}"
                title={i18n.t.library.speakerTip}
                onclick={() => startSpeakerEdit(n)}>{diarizedLabel(n)}</button
              >{/if}<span class="txt">{seg.text}</span>
          </span>
        </p>
      {/each}
    </div>
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
      <select
        class="project-filter"
        aria-label={i18n.t.library.project}
        value={projectFilter}
        onchange={(e) => setFilter(e.currentTarget.value)}
      >
        <option value="">{i18n.t.library.allMeetings}</option>
        <option value={NO_PROJECT}>{i18n.t.library.noProject}</option>
        {#each intel.projects as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
      </select>
    </header>

    {#if filterProject}
      <div class="project-bar">
        {#if renamingProject}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            aria-label={i18n.t.library.renameProject}
            bind:value={projectDraft}
            autofocus
            onkeydown={(e) => {
              if (e.key === "Enter") saveProjectName();
              if (e.key === "Escape") renamingProject = false;
            }}
          />
          <button class="btn" onclick={saveProjectName}>{i18n.t.library.save}</button>
          <button class="btn" onclick={() => ((renamingProject = false), (projectError = ""))}>{i18n.t.library.cancel}</button>
        {:else}
          <span class="project-name">{filterProject.name}</span>
          <span class="lib-meta">{i18n.t.library.meetingCount(shown.length)}</span>
          <button class="btn" onclick={() => ((projectDraft = filterProject!.name), (renamingProject = true))}
            >{i18n.t.library.renameProject}</button
          >
          <button
            class="btn"
            title={i18n.t.library.instructionsHelp}
            onclick={() => ((instructionsDraft = instructions), (editingInstructions = true))}>{i18n.t.library.instructions}</button
          >
          <button class="btn" title={i18n.t.library.projectBriefHelp} onclick={openBrief}
            >{i18n.t.library.projectBrief}</button
          >
          {#if onNewMeeting && !sessionRunning}
            <button class="btn primary" onclick={() => onNewMeeting(filterProject!.id)}>{i18n.t.library.newMeetingInProject}</button>
          {/if}
        {/if}
        {#if projectError}<span class="err">{projectError}</span>{/if}
      </div>
      {#if editingInstructions}
        <div class="instructions">
          <label for="project-instructions" class="project-label">{i18n.t.library.instructionsHelp}</label>
          <!-- svelte-ignore a11y_autofocus -->
          <textarea
            id="project-instructions"
            rows="5"
            bind:value={instructionsDraft}
            placeholder={i18n.t.library.instructionsPlaceholder}
            autofocus
            onkeydown={(e) => {
              if (e.key === "Escape") editingInstructions = false;
            }}
          ></textarea>
          <div class="project-row">
            <button class="btn primary" onclick={saveInstructions}>{i18n.t.library.save}</button>
            <button class="btn" onclick={() => (editingInstructions = false)}>{i18n.t.library.cancel}</button>
          </div>
        </div>
      {:else if instructions}
        <p class="instructions-preview" title={i18n.t.library.instructionsHelp}>
          <span class="project-label">{i18n.t.library.instructions}:</span> {instructions}
        </p>
      {/if}
      {#if memory.length}
        <details class="knowledge">
          <summary>{i18n.t.library.projectKnowledge} ({memory.length})</summary>
          <ul>
            {#each memory as m (m.id)}<li><span class="k-kind">{m.kind}</span> {m.text}</li>{/each}
          </ul>
        </details>
      {/if}
      {#if shots.length}
        <details class="knowledge shots">
          <summary>{i18n.t.library.screenshots} ({shots.length})</summary>
          <ul>
            {#each shots as shot (shot.sourceId)}
              <li>
                <ShotThumb sourceId={shot.sourceId} title={shot.title ?? shot.label} />
                <div class="shot-body">
                  <span class="shot-title" title={shot.snippet ?? ""}>{shot.title ?? shot.label}</span>
                  {#if shot.title}<span class="k-kind">{shot.label}</span>{/if}
                  <span class="k-kind"
                    >{shot.expiresAtMs
                      ? i18n.t.library.shotExpires(new Date(shot.expiresAtMs).toLocaleDateString())
                      : i18n.t.library.shotKept}</span
                  >
                </div>
                <button class="btn" aria-label={i18n.t.library.deleteShotTitle} onclick={() => askDeleteShot(shot)}
                  >×</button
                >
              </li>
            {/each}
          </ul>
        </details>
      {/if}
    {/if}

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
      {#if shownHits.length === 0}
        <div class="empty">{searching ? i18n.t.library.searching : i18n.t.library.noResults}</div>
      {:else}
        <ul class="cards" class:loading={searching}>
          {#each shownHits as hit (hit.meeting_id)}
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
    {:else if shown.length === 0}
      <div class="empty">{projectFilter ? i18n.t.library.emptyProject : i18n.t.library.empty}</div>
    {:else}
      <ul class="cards">
        {#each shown as m (m.id)}
          <li class="row">
            <button class="card card-main" onclick={() => openNote(m.id)}>
              <div class="card-top">
                <span class="card-title">{m.title}</span>
                <span class="card-date">{fmtDate(m.started_at_ms)}</span>
              </div>
              <div class="card-sub">
                {fmtDuration(m.duration_ms)}{m.engine ? ` · ${m.engine}` : ""}
                {#if projectFilter === "" && projectName(m.project_id)}<span class="chip">{projectName(m.project_id)}</span>{/if}
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

  <Modal bind:open={mergeOpen} title={i18n.t.library.mergeTitle}>
    {#if pendingMerge}
      <p class="confirm-text">
        {i18n.t.library.mergeConfirm(diarizedLabel(pendingMerge.from), diarizedLabel(pendingMerge.into))}
      </p>
    {/if}
    <div class="confirm-actions">
      <button class="btn" onclick={() => (mergeOpen = false)}>{i18n.t.library.cancel}</button>
      <button class="btn danger" onclick={doMerge}>{i18n.t.library.merge}</button>
    </div>
  </Modal>

  <Modal bind:open={knowledgeOpen} title={i18n.t.library.moveKnowledgeTitle}>
    {#if knowledgeMove}
      <p class="confirm-text">
        {i18n.t.library.moveKnowledgeConfirm(knowledgeMove.count, projectName(knowledgeMove.to))}
        {#if knowledgeMove.shared > 0}
          {i18n.t.library.sharedKnowledgeStays(knowledgeMove.shared, projectName(knowledgeMove.from))}
        {/if}
      </p>
    {/if}
    <div class="confirm-actions">
      <button class="btn" onclick={() => answerKnowledgeMove(false)}>{i18n.t.library.keepKnowledge}</button>
      <button class="btn primary" onclick={() => answerKnowledgeMove(true)}>{i18n.t.library.moveKnowledge}</button>
    </div>
  </Modal>

  <Modal bind:open={briefOpen} wide title={i18n.t.library.projectBrief}>
    {#if briefError}
      <div class="err">{briefError}</div>
    {:else if !briefText}
      <p class="confirm-text">{i18n.t.library.briefLoading}</p>
    {:else}
      <pre class="brief" aria-label={i18n.t.library.projectBrief}>{briefText}</pre>
    {/if}
    <div class="confirm-actions">
      <button class="btn" onclick={() => (briefOpen = false)}>{i18n.t.library.close}</button>
      <button class="btn" disabled={!briefText} onclick={copyBrief}
        >{briefCopied ? i18n.t.library.copied : i18n.t.library.copy}</button
      >
      <button class="btn primary" disabled={!briefText} onclick={saveBrief}>{i18n.t.library.saveMd}</button>
    </div>
  </Modal>

  <Modal bind:open={shotConfirmOpen} title={i18n.t.library.deleteShotTitle}>
    <p class="confirm-text">{i18n.t.library.deleteShotConfirm}</p>
    <div class="confirm-actions">
      <button class="btn" onclick={() => (shotConfirmOpen = false)}>{i18n.t.library.cancel}</button>
      <button class="btn danger" onclick={doDeleteShot}>{i18n.t.library.delete}</button>
    </div>
  </Modal>
</section>

<style>
  .title-btn {
    all: unset;
    cursor: text;
    border-radius: 6px;
  }
  .title-btn:hover .lib-h2,
  .title-btn:focus-visible .lib-h2 {
    text-decoration: underline dotted;
    text-underline-offset: 4px;
  }
  .title-input {
    font: inherit;
    font-size: 1.25rem;
    font-weight: 600;
    width: 100%;
  }
  .project-row,
  .project-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    font-size: 0.85rem;
  }
  .project-label,
  .move-note,
  .k-kind {
    opacity: 0.65;
    font-size: 0.8rem;
  }
  .project-name {
    font-weight: 600;
  }
  .instructions {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 0.85rem;
  }
  .instructions textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    resize: vertical;
  }
  .instructions-preview {
    margin: 0;
    font-size: 0.85rem;
    white-space: pre-line;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .project-filter {
    flex: none;
    max-width: 14rem;
  }
  .btn.primary {
    font-weight: 600;
  }
  .chip {
    margin-left: 8px;
    padding: 1px 7px;
    border-radius: 999px;
    border: 1px solid currentColor;
    opacity: 0.7;
    font-size: 0.75rem;
  }
  .brief {
    max-height: 60vh;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 0.85rem;
    line-height: 1.45;
    padding: 10px 12px;
    border-radius: 8px;
    background: var(--surface);
    margin: 0 0 12px;
  }
  .knowledge {
    font-size: 0.85rem;
  }
  .knowledge ul {
    margin: 6px 0 0;
    padding-left: 18px;
  }
  .shots ul {
    list-style: none;
    padding-left: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .shots li {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .shot-body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .shot-title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  /* Detail header stacks: a top action bar (back ↔ delete), then the title block full-width below,
     so a long title is never squeezed between the two buttons. */
  .detail-head {
    display: flex;
    flex-direction: column;
    gap: 12px;
    flex: none;
  }
  .detail-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  .back {
    flex: none;
    padding: 7px 11px;
    font: inherit;
    font-size: 13px;
    color: var(--muted);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 9px;
    cursor: pointer;
  }
  .back:hover {
    color: var(--text);
    border-color: var(--border-strong);
  }
  .lib-titles {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .lib-meta {
    font-size: 12px;
    color: var(--muted);
  }
  .del-btn {
    flex: none;
    padding: 7px 13px;
    font: inherit;
    font-size: 13px;
    color: var(--stop);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 9px;
    cursor: pointer;
  }
  .del-btn:hover {
    border-color: var(--stop);
  }

  .summary {
    padding: 13px 15px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    font-size: 13.5px;
    line-height: 1.6;
    color: var(--text);
    white-space: pre-wrap;
  }

  .transcript {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .seg {
    margin: 0;
    display: flex;
    gap: 12px;
    font-size: 14px;
    line-height: 1.6;
    color: var(--text);
  }
  .ts {
    flex: none;
    margin-top: 1px;
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
    cursor: default;
  }
  .seg-body {
    min-width: 0;
  }
  .spk {
    display: inline-block;
    margin-right: 8px;
    font-size: 11px;
    font-weight: 600;
    color: var(--live);
  }
  .spk.them {
    color: var(--accent);
  }
  /* A diarized speaker: a button that opens the rename / merge row in the header. */
  .spk.dia,
  .spk-chip {
    padding: 0;
    font: inherit;
    font-weight: 600;
    color: var(--spk);
    background: none;
    border: 0;
    cursor: pointer;
  }
  .spk.dia {
    font-size: 11px;
  }
  .spk.dia:hover,
  .spk-chip:hover,
  .spk-chip.active {
    text-decoration: underline;
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

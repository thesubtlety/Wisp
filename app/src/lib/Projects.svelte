<script lang="ts">
  // The Projects view: the project list on the left ("No project" at the bottom for unfiled
  // meetings), the selected project's home on the right with tabs for its overview, meetings,
  // knowledge, brief and settings. A meeting opens in place (the same page as in the Library).
  // The selected project and tab are remembered on this device.
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";
  import ShotThumb from "$lib/ShotThumb.svelte";
  import MeetingPage from "$lib/MeetingPage.svelte";
  import ProjectOverview from "$lib/ProjectOverview.svelte";
  import { intel, loadProjects, createProject, renameProject, type MemoryItem } from "$lib/intel.svelte";
  import { copyText } from "$lib/clipboard";

  // "New meeting in this project": the page switches to Live with the project selected. Hidden
  // while a session runs: switching projects then would refile the running meeting.
  let { onNewMeeting, sessionRunning = false }: { onNewMeeting?: (projectId: string) => void; sessionRunning?: boolean } =
    $props();

  type NoteSummary = {
    id: string;
    title: string;
    started_at_ms: number;
    duration_ms: number;
    engine: string | null;
    preview: string;
    project_id: string | null;
  };
  type ProjectShot = {
    sourceId: number;
    label: string;
    title: string | null;
    snippet: string | null;
    addedAtMs: number;
    expiresAtMs: number | null;
  };
  type Tab = "overview" | "meetings" | "knowledge" | "brief" | "settings";
  const TABS: Tab[] = ["overview", "meetings", "knowledge", "brief", "settings"];

  const NO_PROJECT = "__none";
  const SELECTED_KEY = "wisp.projectsSelected";
  const TAB_KEY = "wisp.projectsTab";

  function read(key: string): string {
    try {
      return localStorage.getItem(key) ?? "";
    } catch {
      return "";
    }
  }

  function write(key: string, value: string) {
    try {
      localStorage.setItem(key, value);
    } catch {
      // per-device convenience only
    }
  }

  let notes = $state<NoteSummary[]>([]);
  let selected = $state(read(SELECTED_KEY));
  let tab = $state<Tab>((TABS as string[]).includes(read(TAB_KEY)) ? (read(TAB_KEY) as Tab) : "overview");
  let openMeeting = $state<string | null>(null);
  let error = $state("");

  // Creating a project from the list.
  let creating = $state(false);
  let newName = $state("");

  const project = $derived(intel.projects.find((p) => p.id === selected));
  const unfiled = $derived(notes.filter((n) => !n.project_id));
  const projectNotes = $derived(
    notes.filter((n) => n.project_id === selected).sort((a, b) => b.started_at_ms - a.started_at_ms),
  );
  const counts = $derived.by(() => {
    const c = new Map<string, number>();
    for (const n of notes) if (n.project_id) c.set(n.project_id, (c.get(n.project_id) ?? 0) + 1);
    return c;
  });

  // The selected project's details.
  let memory = $state<MemoryItem[]>([]);
  let shots = $state<ProjectShot[]>([]);
  let instructions = $state("");
  let instructionsDraft = $state("");
  let editingInstructions = $state(false);
  let renaming = $state(false);
  let nameDraft = $state("");
  let settingsError = $state("");
  let menuOpen = $state(false);
  let deleteOpen = $state(false);
  let pendingShot = $state<ProjectShot | null>(null);
  let shotConfirmOpen = $state(false);

  // Knowledge editing.
  let editingMemory = $state<number | null>(null);
  let memoryDraft = $state("");
  let confirmMemory = $state<number | null>(null);

  // The brief preview.
  let briefText = $state("");
  let briefError = $state("");
  let briefCopied = $state(false);

  function select(id: string) {
    selected = id;
    openMeeting = null;
    renaming = false;
    editingInstructions = false;
    settingsError = "";
    write(SELECTED_KEY, id);
  }

  function setTab(t: Tab) {
    tab = t;
    openMeeting = null;
    write(TAB_KEY, t);
  }

  async function loadNotes() {
    try {
      notes = await invoke<NoteSummary[]>("list_library_notes");
      error = "";
    } catch (e) {
      error = String(e);
    }
  }

  onMount(async () => {
    loadNotes();
    await loadProjects();
    // A remembered project that was deleted falls back to nothing selected.
    if (selected && selected !== NO_PROJECT && !project) select("");
  });

  function loadMemory(id: string) {
    invoke<MemoryItem[]>("list_project_memory", { projectId: id })
      .then((m) => {
        if (selected === id) memory = m;
      })
      .catch(() => {});
  }

  async function loadShots(id: string) {
    try {
      const list = await invoke<ProjectShot[]>("list_project_screenshots", { projectId: id });
      if (selected === id) shots = list;
    } catch {
      // the section just stays empty
    }
  }

  $effect(() => {
    const id = project?.id;
    memory = [];
    shots = [];
    instructions = "";
    if (!id) return;
    loadMemory(id);
    loadShots(id);
    invoke<string>("get_project_instructions", { id })
      .then((text) => {
        if (selected === id) instructions = text;
      })
      .catch(() => {});
  });

  // Local UTC offset in minutes east, so the brief's dates match the user's calendar.
  const offsetMinutes = () => -new Date().getTimezoneOffset();

  async function loadBrief(id: string) {
    briefText = "";
    briefError = "";
    briefCopied = false;
    try {
      const text = await invoke<string>("project_brief_markdown", { projectId: id, offsetMinutes: offsetMinutes() });
      if (selected === id) briefText = text;
    } catch (e) {
      briefError = String(e);
    }
  }

  $effect(() => {
    const id = project?.id;
    if (id && tab === "brief" && !openMeeting) loadBrief(id);
  });

  // The Settings tab's name field starts as the current name.
  $effect(() => {
    if (tab === "settings" && project) nameDraft = project.name;
  });

  async function copyBrief() {
    await copyText(briefText);
    briefCopied = true;
    setTimeout(() => (briefCopied = false), 1500);
  }

  async function saveBrief() {
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

  async function create() {
    const name = newName.trim();
    if (!name) return;
    const err = await createProject(name, false);
    if (err) {
      error = err;
      return;
    }
    creating = false;
    newName = "";
    error = "";
    const made = intel.projects.find((p) => p.name === name);
    if (made) select(made.id);
  }

  async function saveName() {
    if (!project) return;
    const name = nameDraft.trim();
    if (!name || name === project.name) {
      renaming = false;
      return;
    }
    settingsError = await renameProject(project.id, name);
    if (!settingsError) renaming = false;
  }

  async function saveInstructions() {
    const id = project?.id;
    if (!id) return;
    try {
      await invoke("set_project_instructions", { id, instructions: instructionsDraft });
      instructions = instructionsDraft.trim();
      editingInstructions = false;
      settingsError = "";
    } catch (e) {
      settingsError = String(e);
    }
  }

  async function deleteProject() {
    deleteOpen = false;
    const id = project?.id;
    if (!id) return;
    try {
      await invoke("delete_project_completely", { projectId: id });
      select("");
      await loadProjects();
      await loadNotes();
    } catch (e) {
      error = String(e);
    }
  }

  async function saveMemory(id: number) {
    const text = memoryDraft.trim();
    const entry = memory.find((m) => m.id === id);
    editingMemory = null;
    if (!entry || !text || text === entry.text) return;
    try {
      await invoke("update_project_memory", { id, text });
      entry.text = text;
    } catch (e) {
      error = String(e);
    }
  }

  async function deleteMemory(id: number) {
    confirmMemory = null;
    try {
      await invoke("delete_project_memory", { id });
      memory = memory.filter((m) => m.id !== id);
    } catch (e) {
      error = String(e);
    }
  }

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

  function fmtDate(ms: number): string {
    return new Date(ms).toLocaleString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function fmtDay(ms: number): string {
    return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  }

  function fmtDuration(ms: number): string {
    const total = Math.round(ms / 1000);
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${m}:${s.toString().padStart(2, "0")}`;
  }

  function tabLabel(t: Tab): string {
    const p = i18n.t.projects;
    return { overview: p.tabOverview, meetings: p.tabMeetings, knowledge: p.tabKnowledge, brief: p.tabBrief, settings: p.tabSettings }[t];
  }
</script>

<svelte:window
  onclick={() => {
    if (menuOpen) menuOpen = false;
  }}
/>

{#snippet meetingList(list: NoteSummary[], empty: string)}
  {#if list.length === 0}
    <p class="quiet">{empty}</p>
  {:else}
    <ul class="cards">
      {#each list as m (m.id)}
        <li>
          <button class="card" onclick={() => (openMeeting = m.id)}>
            <div class="card-top">
              <span class="card-title">{m.title}</span>
              <span class="card-date">{fmtDate(m.started_at_ms)}</span>
            </div>
            <div class="card-sub">{fmtDuration(m.duration_ms)}{m.engine ? ` · ${m.engine}` : ""}</div>
            {#if m.preview}<div class="preview">{m.preview}</div>{/if}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
{/snippet}

<section class="projects">
  <nav class="plist" aria-label={i18n.t.projects.title}>
    <div class="plist-head">
      <h2 class="h2">{i18n.t.projects.title}</h2>
      <button class="icon-btn" title={i18n.t.projects.newProject} aria-label={i18n.t.projects.newProject} onclick={() => (creating = true)}
        >+</button
      >
    </div>
    {#if creating}
      <div class="create">
        <!-- svelte-ignore a11y_autofocus -->
        <input
          placeholder={i18n.t.projects.projectName}
          aria-label={i18n.t.projects.projectName}
          bind:value={newName}
          autofocus
          onkeydown={(e) => {
            if (e.key === "Enter") create();
            if (e.key === "Escape") creating = false;
          }}
        />
      </div>
    {/if}
    {#each intel.projects as p (p.id)}
      <button class="pitem" class:active={selected === p.id} onclick={() => select(p.id)}>
        <span class="pname">{p.name}</span><span class="pcount">{counts.get(p.id) ?? 0}</span>
      </button>
    {:else}
      {#if !creating}<p class="quiet small">{i18n.t.projects.empty}</p>{/if}
    {/each}
    <div class="plist-spacer"></div>
    <button class="pitem none" class:active={selected === NO_PROJECT} onclick={() => select(NO_PROJECT)}>
      <span class="pname">{i18n.t.projects.noProject}</span><span class="pcount">{unfiled.length}</span>
    </button>
    {#if error}<p class="err small">{error}</p>{/if}
  </nav>

  <div class="phome">
    {#if openMeeting}
      <MeetingPage
        id={openMeeting}
        onBack={() => (openMeeting = null)}
        onChanged={() => {
          loadNotes();
          if (project) loadMemory(project.id);
        }}
        onDeleted={() => loadNotes()}
      />
    {:else if selected === NO_PROJECT}
      <header class="head">
        <h2 class="h2">{i18n.t.projects.noProject}</h2>
        <span class="meta">{i18n.t.projects.noProjectHelp}</span>
      </header>
      {@render meetingList(unfiled, i18n.t.library.empty)}
    {:else if project}
      <header class="head">
        <div class="head-row">
          {#if renaming}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="name-input"
              aria-label={i18n.t.library.renameProject}
              bind:value={nameDraft}
              autofocus
              onkeydown={(e) => {
                if (e.key === "Enter") saveName();
                if (e.key === "Escape") renaming = false;
              }}
              onblur={saveName}
            />
          {:else}
            <button class="title-btn" title={i18n.t.library.renameProject} onclick={() => ((nameDraft = project.name), (renaming = true))}>
              <h2 class="h2">{project.name}</h2>
            </button>
          {/if}
          <div class="head-actions">
            {#if onNewMeeting && !sessionRunning}
              <button class="btn primary" onclick={() => onNewMeeting(project.id)}>{i18n.t.library.newMeetingInProject}</button>
            {/if}
            <div class="menu-wrap">
              <button
                class="btn"
                title={i18n.t.projects.more}
                aria-label={i18n.t.projects.more}
                aria-expanded={menuOpen}
                onclick={(e) => {
                  e.stopPropagation();
                  menuOpen = !menuOpen;
                }}>⋯</button
              >
              {#if menuOpen}
                <div class="menu" role="menu">
                  <button class="mi danger" role="menuitem" onclick={() => ((menuOpen = false), (deleteOpen = true))}
                    >{i18n.t.projects.deleteProject}</button
                  >
                </div>
              {/if}
            </div>
          </div>
        </div>
        <span class="meta">
          {i18n.t.projects.meetingCount(projectNotes.length)}{#if projectNotes.length}
            · {i18n.t.projects.lastMeeting(fmtDay(projectNotes[0].started_at_ms))}{/if}
          · {i18n.t.projects.knowledgeCount(memory.length)}
        </span>
        {#if settingsError}<span class="err">{settingsError}</span>{/if}
      </header>

      <div class="tabs" role="tablist">
        {#each TABS as t (t)}
          <button role="tab" aria-selected={tab === t} class:on={tab === t} onclick={() => setTab(t)}>{tabLabel(t)}</button>
        {/each}
      </div>

      {#if tab === "overview"}
        <ProjectOverview projectId={project.id} meetings={projectNotes} onOpenMeeting={(id) => (openMeeting = id)} />
      {:else if tab === "meetings"}
        {@render meetingList(projectNotes, i18n.t.projects.noMeetings)}
      {:else if tab === "knowledge"}
        {#if memory.length === 0}
          <p class="quiet">{i18n.t.projects.noKnowledge}</p>
        {:else}
          <ul class="klist">
            {#each memory as m (m.id)}
              <li class="krow">
                {#if editingMemory === m.id}
                  <!-- svelte-ignore a11y_autofocus -->
                  <textarea
                    rows="2"
                    aria-label={i18n.t.items.text}
                    bind:value={memoryDraft}
                    autofocus
                    onkeydown={(e) => {
                      if (e.key === "Escape") editingMemory = null;
                      if (e.key === "Enter" && !e.shiftKey) {
                        e.preventDefault();
                        saveMemory(m.id);
                      }
                    }}
                  ></textarea>
                  <div class="row-btns">
                    <button class="btn small primary" onclick={() => saveMemory(m.id)}>{i18n.t.common.save}</button>
                    <button class="btn small" onclick={() => (editingMemory = null)}>{i18n.t.common.cancel}</button>
                  </div>
                {:else}
                  <span class="k-kind">{m.kind}</span>
                  <span class="k-text">{m.text}</span>
                  {#if confirmMemory === m.id}
                    <span class="row-btns">
                      <span class="quiet small">{i18n.t.projects.deleteKnowledgeConfirm}</span>
                      <button class="btn small danger" onclick={() => deleteMemory(m.id)}>{i18n.t.items.delete}</button>
                      <button class="btn small" onclick={() => (confirmMemory = null)}>{i18n.t.common.cancel}</button>
                    </span>
                  {:else}
                    <span class="row-btns hover">
                      <button class="btn small" onclick={() => ((memoryDraft = m.text), (editingMemory = m.id))}>{i18n.t.items.edit}</button>
                      <button class="btn small" onclick={() => (confirmMemory = m.id)}>{i18n.t.items.delete}</button>
                    </span>
                  {/if}
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
        {#if shots.length}
          <h3 class="h3">{i18n.t.library.screenshots} <span class="quiet">{shots.length}</span></h3>
          <ul class="shots">
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
                <button class="btn" aria-label={i18n.t.library.deleteShotTitle} onclick={() => askDeleteShot(shot)}>×</button>
              </li>
            {/each}
          </ul>
        {/if}
      {:else if tab === "brief"}
        <p class="quiet">{i18n.t.library.projectBriefHelp}</p>
        <div class="row-btns">
          <button class="btn" disabled={!briefText} onclick={copyBrief}>{briefCopied ? i18n.t.library.copied : i18n.t.library.copy}</button>
          <button class="btn primary" disabled={!briefText} onclick={saveBrief}>{i18n.t.library.saveMd}</button>
        </div>
        {#if briefError}
          <p class="err">{briefError}</p>
        {:else if !briefText}
          <p class="quiet">{i18n.t.library.briefLoading}</p>
        {:else}
          <pre class="brief" aria-label={i18n.t.library.projectBrief}>{briefText}</pre>
        {/if}
      {:else}
        <div class="settings">
          <label class="label" for="project-name">{i18n.t.projects.name}</label>
          <div class="row-btns">
            <input id="project-name" bind:value={nameDraft} />
            <button class="btn" disabled={!nameDraft.trim() || nameDraft.trim() === project.name} onclick={saveName}
              >{i18n.t.library.renameProject}</button
            >
          </div>

          <label class="label" for="project-instructions">{i18n.t.library.instructions}</label>
          <p class="quiet small">{i18n.t.library.instructionsHelp}</p>
          {#if editingInstructions}
            <textarea
              id="project-instructions"
              rows="6"
              bind:value={instructionsDraft}
              placeholder={i18n.t.library.instructionsPlaceholder}
              onkeydown={(e) => {
                if (e.key === "Escape") editingInstructions = false;
              }}
            ></textarea>
            <div class="row-btns">
              <button class="btn primary" onclick={saveInstructions}>{i18n.t.library.save}</button>
              <button class="btn" onclick={() => (editingInstructions = false)}>{i18n.t.library.cancel}</button>
            </div>
          {:else}
            {#if instructions}<p class="instructions">{instructions}</p>{/if}
            <div class="row-btns">
              <button class="btn" onclick={() => ((instructionsDraft = instructions), (editingInstructions = true))}>{i18n.t.items.edit}</button>
            </div>
          {/if}

          <div class="danger-zone">
            <button class="btn danger-outline" onclick={() => (deleteOpen = true)}>{i18n.t.projects.deleteProject}</button>
          </div>
        </div>
      {/if}
    {:else}
      <p class="quiet center">{intel.projects.length ? i18n.t.projects.pick : i18n.t.projects.empty}</p>
    {/if}
  </div>

  <Modal bind:open={deleteOpen} title={i18n.t.projects.deleteProjectTitle}>
    <p class="confirm-text">{i18n.t.projects.deleteProjectConfirm(project?.name ?? "")}</p>
    <div class="confirm-actions">
      <button class="btn" onclick={() => (deleteOpen = false)}>{i18n.t.library.cancel}</button>
      <button class="btn danger" onclick={deleteProject}>{i18n.t.settings.delete}</button>
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
  .projects {
    flex: 1;
    min-width: 0;
    display: flex;
    min-height: 0;
  }
  .plist {
    flex: none;
    width: 220px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 24px 10px 16px 16px;
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }
  .plist-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }
  .plist-spacer {
    flex: 1;
    min-height: 12px;
  }
  .icon-btn {
    width: 26px;
    height: 26px;
    font: inherit;
    font-size: 16px;
    line-height: 1;
    color: var(--muted);
    background: none;
    border: 1px solid var(--border);
    border-radius: 7px;
    cursor: pointer;
  }
  .icon-btn:hover {
    color: var(--text);
    border-color: var(--border-strong);
  }
  .create input {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    font-size: 13px;
    margin-bottom: 4px;
  }
  .pitem {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    width: 100%;
    padding: 6px 9px;
    font: inherit;
    font-size: 13.5px;
    color: var(--text);
    text-align: left;
    background: none;
    border: 0;
    border-radius: 8px;
    cursor: pointer;
  }
  .pitem:hover {
    background: var(--surface);
  }
  .pitem.active {
    background: var(--surface-active);
    font-weight: 600;
  }
  .pitem.none {
    color: var(--muted);
  }
  .pname {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pcount {
    flex: none;
    font-size: 11.5px;
    font-weight: 400;
    color: var(--muted);
  }
  .phome {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 24px clamp(16px, 4vw, 48px);
    overflow-y: auto;
  }
  .head {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .head-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  .head-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: none;
  }
  .h2 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
    color: var(--text);
  }
  .h3 {
    margin: 10px 0 0;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }
  .title-btn {
    all: unset;
    cursor: text;
    min-width: 0;
  }
  .title-btn:hover .h2,
  .title-btn:focus-visible .h2 {
    text-decoration: underline dotted;
    text-underline-offset: 4px;
  }
  .name-input {
    font: inherit;
    font-size: 17px;
    font-weight: 600;
    flex: 1;
    min-width: 0;
  }
  .meta {
    font-size: 12px;
    color: var(--muted);
  }
  .menu-wrap {
    position: relative;
  }
  .menu {
    position: absolute;
    top: 38px;
    right: 0;
    z-index: 5;
    min-width: 170px;
    padding: 4px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 9px;
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.12);
  }
  .mi {
    width: 100%;
    text-align: left;
    font: inherit;
    font-size: 13px;
    background: none;
    border: 0;
    border-radius: 6px;
    padding: 6px 10px;
    cursor: pointer;
  }
  .mi:hover {
    background: var(--surface-active);
  }
  .mi.danger {
    color: var(--stop);
  }
  .tabs {
    display: flex;
    gap: 4px;
    border-bottom: 1px solid var(--border);
    flex: none;
  }
  .tabs button {
    font: inherit;
    font-size: 13px;
    color: var(--muted);
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 6px 10px;
    cursor: pointer;
  }
  .tabs button.on {
    color: var(--text);
    border-bottom-color: var(--accent);
  }
  .quiet {
    margin: 0;
    font-size: 13px;
    color: var(--muted);
  }
  .small {
    font-size: 12px;
  }
  .center {
    margin: 40px auto;
    text-align: center;
  }
  .err {
    margin: 0;
    font-size: 12.5px;
    color: var(--stop);
  }
  .cards {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .card {
    width: 100%;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    text-align: left;
    padding: 12px 14px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    cursor: pointer;
    font: inherit;
    color: var(--text);
  }
  .card:hover {
    border-color: var(--border-strong);
    background: var(--surface-active);
  }
  .card-top {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }
  .card-title {
    font-size: 14px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .card-date,
  .card-sub {
    flex: none;
    font-size: 12px;
    color: var(--muted);
  }
  .preview {
    font-size: 13px;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .klist {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .krow {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 6px 8px;
    padding: 7px 0;
    font-size: 13px;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
  }
  .krow textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    font-size: 13px;
    resize: vertical;
  }
  .k-kind {
    font-size: 11px;
    color: var(--muted);
  }
  .k-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .row-btns {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
  .row-btns.hover {
    opacity: 0;
    transition: opacity 0.12s;
  }
  .krow:hover .row-btns.hover,
  .krow:focus-within .row-btns.hover {
    opacity: 1;
  }
  .shots {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .shots li {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 13px;
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
  .brief {
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 0.85rem;
    line-height: 1.45;
    padding: 10px 12px;
    border-radius: 8px;
    background: var(--surface);
    border: 1px solid var(--border);
    margin: 0;
  }
  .settings {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-width: 640px;
  }
  .settings input {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-size: 13px;
  }
  .settings textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    font-size: 13px;
    resize: vertical;
  }
  .label {
    margin-top: 8px;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }
  .instructions {
    margin: 0;
    font-size: 13px;
    white-space: pre-line;
  }
  .danger-zone {
    margin-top: 20px;
    padding-top: 14px;
    border-top: 1px solid var(--border);
  }
  .btn {
    padding: 7px 13px;
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
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .btn.small {
    padding: 3px 9px;
    font-size: 12px;
    border-radius: 7px;
  }
  .btn.primary {
    font-weight: 600;
  }
  .btn.danger {
    color: #fff;
    background: var(--stop);
    border-color: var(--stop);
  }
  .btn.danger-outline {
    color: var(--stop);
  }
  .btn.danger-outline:hover {
    border-color: var(--stop);
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
</style>

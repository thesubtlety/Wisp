<script lang="ts">
  // One saved meeting: its title, project, speakers, exports, and tabs for the transcript, the
  // structured state (with hand edits), the summary and prompts. Shared by the Library and the
  // Projects view; the host shows it for `id` and hides it on `onBack`.
  import { tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";
  import { intel, createProject, loadMeetingTypes, type ItemChange, type StateItem } from "$lib/intel.svelte";
  import MeetingState from "$lib/MeetingState.svelte";
  import MeetingExports from "$lib/MeetingExports.svelte";
  import SummaryView from "$lib/SummaryView.svelte";
  import PromptRunner from "$lib/PromptRunner.svelte";
  import AskThread from "$lib/AskThread.svelte";
  import ReviewPanel from "$lib/ReviewPanel.svelte";
  import AiActivity from "$lib/AiActivity.svelte";

  let {
    id,
    onBack,
    onChanged,
    onDeleted,
  }: {
    id: string;
    onBack: () => void;
    /** The meeting's title, project or state changed; the host may reload its lists. */
    onChanged?: () => void;
    /** The meeting was deleted. */
    onDeleted?: (id: string) => void;
  } = $props();

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
  // The saved type's suggested prompts come from the type list.
  $effect(() => {
    if (!intel.meetingTypes.length) loadMeetingTypes();
  });
  type Detail = {
    meeting: Note;
    segments: Segment[];
    speakerNames: Record<number, string>;
    /** The meeting type it ran as, if recorded. */
    meetingType: { id: string; name: string } | null;
  };

  let detail = $state<Detail | null>(null);
  let error = $state("");
  let confirmOpen = $state(false);

  // Editing: the title, and which project the meeting is filed under.
  let editingTitle = $state(false);
  let titleDraft = $state("");
  let newProjectOpen = $state(false);
  let newProjectName = $state("");
  let moveNote = $state("");
  // After a move: knowledge learned only from this meeting can follow it to the new project.
  type KnowledgeMove = { id: string; from: string; to: string; count: number; shared: number };
  let knowledgeMove = $state<KnowledgeMove | null>(null);
  let knowledgeOpen = $state(false);

  // The tabs: its transcript, its structured state, its summary and prompts. A state item's
  // evidence chip jumps to (and highlights) its transcript line.
  let detailTab = $state<"transcript" | "state" | "ask" | "review" | "summary" | "prompts">("transcript");
  let activityOpen = $state(false);
  // Who spoke, as the prompt library names them ("You", a given name, "Speaker 2", "Them").
  const meetingSpeakers = $derived.by(() => {
    const seen = new Set<string>();
    for (const seg of detail?.segments ?? []) {
      const named = seg.speaker !== null ? detail?.speakerNames[seg.speaker] : undefined;
      if (seg.source === "mic") seen.add(named || i18n.t.library.you);
      else seen.add(seg.speaker !== null ? diarizedLabel(seg.speaker) : i18n.t.library.them);
    }
    return [...seen];
  });
  let stateItems = $state<StateItem[]>([]);
  let highlightIdx = $state<number | null>(null);
  // Bumped after a speaker rename to remount the self-loading tabs (Ask, Prompts) so they show the
  // relabeled text without reopening the meeting.
  let reloadKey = $state(0);

  $effect(() => {
    const want = id;
    detailTab = "transcript";
    highlightIdx = null;
    stateItems = [];
    openNote(want);
    invoke<StateItem[]>("intel_saved_state", { id: want })
      .then((items) => {
        if (id === want) stateItems = items;
      })
      .catch(() => {});
  });

  /** Applies a hand change to one of this meeting's items (appended to its state log). */
  async function editItem(item: StateItem, change: ItemChange): Promise<string> {
    const meetingId = id;
    try {
      const items = await invoke<StateItem[]>("edit_meeting_item", { id: meetingId, itemId: item.id, change });
      if (id === meetingId) stateItems = items;
      onChanged?.();
      return "";
    } catch (e) {
      return String(e);
    }
  }

  /** A transcript line's time, for an evidence ref into this meeting that still resolves. */
  function refLabel(ref: string): string | null {
    const m = /^M(.+):T(\d+)$/.exec(ref);
    if (!detail || !m || m[1] !== detail.meeting.id) return null;
    const seg = detail.segments.find((s) => s.idx === Number(m[2]));
    return seg ? fmtDuration(seg.start_ms) : null;
  }

  async function showRef(ref: string) {
    const m = /:T(\d+)$/.exec(ref);
    if (!m) return;
    highlightIdx = Number(m[1]);
    detailTab = "transcript";
    await tick();
    document.getElementById(`seg-${highlightIdx}`)?.scrollIntoView({ behavior: "smooth", block: "center" });
  }

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
      // The backend rewrote the old label in the stored summary, state, ask thread and prompt runs;
      // reload them so the open tabs reflect it.
      await refreshArtifacts();
    } catch (e) {
      error = String(e);
    }
  }

  // Reload a meeting's derived artifacts in place (after a speaker rename). Leaves the active tab as
  // it is; the transcript already relabeled itself from `speakerNames`.
  async function refreshArtifacts() {
    const want = detail?.meeting.id;
    if (!want) return;
    await openNote(want);
    try {
      const items = await invoke<StateItem[]>("intel_saved_state", { id: want });
      if (detail?.meeting.id === want) stateItems = items;
    } catch {
      // keep the items already shown
    }
    reloadKey++;
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

  function projectName(pid: string | null): string {
    return (pid && intel.projects.find((p) => p.id === pid)?.name) || "";
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
      onChanged?.();
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
    const meetingId = detail.meeting.id;
    try {
      await invoke<boolean>("set_note_project", { id: meetingId, projectId: projectId || null });
      detail.meeting.project_id = projectId || null;
      moveNote = "";
      onChanged?.();
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
      const k = await invoke<{ exclusive: number; shared: number }>("note_knowledge", { id: meetingId, projectId: from });
      if (k.exclusive > 0) {
        knowledgeMove = { id: meetingId, from, to: projectId, count: k.exclusive, shared: k.shared };
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
      onChanged?.();
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

  async function openNote(noteId: string) {
    editingTitle = false;
    editingSpeaker = null;
    newProjectOpen = false;
    moveNote = "";
    error = "";
    try {
      const got = await invoke<Detail | null>("get_library_note", { id: noteId });
      if (id === noteId) detail = got;
      error = "";
    } catch (e) {
      error = String(e);
    }
  }

  function askDelete() {
    confirmOpen = true;
  }

  async function doDelete() {
    confirmOpen = false;
    const noteId = detail?.meeting.id;
    if (!noteId) return;
    try {
      await invoke<boolean>("delete_library_note", { id: noteId });
      onDeleted?.(noteId);
      onBack();
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

  function speakerLabel(source: string): string {
    if (source === "mic") return i18n.t.library.you;
    if (source === "system") return i18n.t.library.them;
    return "";
  }
</script>

<section class="meeting-page">
  {#if detail}
  <header class="detail-head">
    <div class="detail-bar">
      <button class="back" onclick={onBack}>← {i18n.t.library.back}</button>
      <button class="del-btn" onclick={askDelete}>
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
        {#if detail.meetingType}<span class="type-chip" title={i18n.t.meetingTypes.label}
            >{detail.meetingType.name}</span
          >{/if}
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

  <div class="meeting-exports">
    <MeetingExports
      target={{ id: detail.meeting.id, title: null, when: fmtDate(detail.meeting.started_at_ms) }}
      startedAt={detail.meeting.started_at_ms}
      unavailable={[
        ...(detail.meeting.summary ? [] : (["summary"] as const)),
        ...(detail.segments.length ? [] : (["transcript"] as const)),
      ]}
    />
  </div>

  <div class="detail-tabs" role="tablist">
    {#each [["transcript", i18n.t.library.tabTranscript], ["state", i18n.t.library.tabState], ["ask", i18n.t.intel.tabAsk], ["review", i18n.t.intel.tabReview], ["summary", i18n.t.library.tabSummary], ["prompts", i18n.t.prompts.tab]] as const as [tab, label] (tab)}
      <button role="tab" aria-selected={detailTab === tab} class:on={detailTab === tab} onclick={() => (detailTab = tab)}
        >{label}{#if tab === "state" && stateItems.length}<span class="tab-count">{stateItems.length}</span>{/if}</button
      >
    {/each}
    <span class="grow"></span>
    <button class="activity" onclick={() => (activityOpen = true)}>{i18n.t.audit.title}</button>
  </div>

  {#if detailTab === "state"}
    {#if stateItems.length}
      <MeetingState items={stateItems} {refLabel} onRef={showRef} onEdit={editItem} />
    {:else}
      <p class="move-note">{i18n.t.library.stateNone}</p>
    {/if}
  {:else if detailTab === "ask"}
    {#key reloadKey}
      <AskThread meetingId={detail.meeting.id} />
    {/key}
  {:else if detailTab === "review"}
    <ReviewPanel
      meetingId={detail.meeting.id}
      projectId={detail.meeting.project_id ?? ""}
      projectName={projectName(detail.meeting.project_id)}
    />
  {:else if detailTab === "prompts"}
    {#key reloadKey}
      <PromptRunner
        meetingId={detail.meeting.id}
        suggested={intel.meetingTypes.find((t) => t.id === detail?.meetingType?.id)?.suggestedPrompts ?? []}
        speakers={meetingSpeakers}
        title={detail.meeting.title}
        date={fmtDate(detail.meeting.started_at_ms)}
      />
    {/key}
  {:else if detailTab === "summary"}
    <SummaryView
      meetingId={detail.meeting.id}
      when={fmtDate(detail.meeting.started_at_ms)}
      summary={detail.meeting.summary ?? ""}
      hasState={stateItems.length > 0}
      onSummary={(md, id) => {
        // Only the meeting it was made for: the user may have opened another meanwhile.
        if (detail && detail.meeting.id === id) detail.meeting.summary = md;
      }}
    />
  {:else}
    <div class="transcript">
      {#each detail.segments as seg (seg.idx)}
        <p class="seg" id="seg-{seg.idx}" class:hl={highlightIdx === seg.idx}>
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
  {/if}

  <Modal bind:open={activityOpen} title={i18n.t.audit.title}>
    <AiActivity meetingId={detail.meeting.id} />
  </Modal>
  {:else if error}
    <div class="err">{error}</div>
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
</section>

<style>
  .meeting-page {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
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

  .project-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    font-size: 0.85rem;
  }

  .project-label,
  .move-note {
    opacity: 0.65;
    font-size: 0.8rem;
  }

  .btn.primary {
    font-weight: 600;
  }

  .lib-h2 {
    text-decoration: underline dotted;
    text-underline-offset: 4px;
  }

  .err {
    font-size: 13px;
    color: var(--stop);
  }

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

  .detail-tabs {
    display: flex;
    align-items: center;
    gap: 4px;
    border-bottom: 1px solid var(--border);
  }

  .detail-tabs .grow {
    flex: 1;
  }

  .detail-tabs button {
    font: inherit;
    font-size: 13px;
    color: var(--muted);
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 6px 10px;
    cursor: pointer;
  }

  .detail-tabs button.on {
    color: var(--text);
    border-bottom-color: var(--accent);
  }

  .tab-count {
    margin-left: 6px;
    font-size: 11px;
    color: var(--muted);
  }

  .seg.hl {
    border-radius: 6px;
    background: color-mix(in srgb, var(--accent) 14%, transparent);
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

  .spk.dia,
  .type-chip {
    display: inline-block;
    margin-left: 6px;
    font-size: 11px;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--muted);
    vertical-align: 1px;
  }

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
</style>

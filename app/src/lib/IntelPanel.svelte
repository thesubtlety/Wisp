<script lang="ts">
  // The meeting intelligence panel's body: Ask (questions answered with cited evidence, each answer
  // copyable) and State (the structured meeting state, with Analyze now). Lives inside AssistPanel.
  import { i18n } from "$lib/i18n.svelte";
  import Modal from "$lib/Modal.svelte";
  import AiActivity from "$lib/AiActivity.svelte";
  import {
    intel,
    askQuestion,
    cancelAsk,
    analyzeNow,
    dismissCard,
    wrapUp,
    setScheduledEnd,
    startReview,
    replyReview,
    setFollowUpClass,
    applyReview,
    proposeLearning,
    saveLearning,
    loadMemory,
    deleteMemory,
    copyExport,
    saveExport,
    importContext,
    describeContext,
    removeContext,
    type ExportKind,
    CLASS_ORDER,
    KIND_ORDER,
    GAP_ORDER,
    type StateItem,
  } from "$lib/intel.svelte";

  let { running }: { running: boolean } = $props();

  // Opening Insights marks what's new there seen.
  $effect(() => {
    if (intel.tab === "insights" && intel.unseen) intel.unseen = 0;
  });
  const gapGroups = $derived(
    intel.audit
      ? GAP_ORDER.map((category) => ({
          category,
          gaps: intel.audit!.gaps.filter((g) => g.category === category),
        })).filter((g) => g.gaps.length)
      : [],
  );
  let draft = $state("");
  let reviewDraft = $state("");
  const canReview = $derived(!running && !!intel.savedMeetingId);
  const projectName = $derived(intel.projects.find((p) => p.id === intel.savedProjectId)?.name ?? "");
  // The State view shows the selected project's knowledge.
  $effect(() => {
    if (intel.tab === "state" && intel.projectId) loadMemory();
  });

  function reviewMarkdown(): string {
    return (intel.review?.followups ?? [])
      .map((f) => `${f.n}. [${i18n.t.intel.classes[f.class]}] ${f.text}`)
      .join("\n");
  }
  let activityOpen = $state(false);
  let copiedAt = $state(-1);
  let notRunning = $state(false);
  let feedEl = $state<HTMLDivElement>();

  const asking = $derived(intel.turns.some((t) => t.pending));
  const groups = $derived(
    KIND_ORDER.map((kind) => ({ kind, items: intel.items.filter((i) => i.kind === kind) })).filter(
      (g) => g.items.length,
    ),
  );

  async function send(text = draft) {
    if (!text.trim() || asking) return;
    draft = "";
    const done = askQuestion(text);
    queueMicrotask(() => feedEl?.scrollTo({ top: feedEl.scrollHeight }));
    await done;
    feedEl?.scrollTo({ top: feedEl.scrollHeight });
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  async function copy(i: number, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      copiedAt = i;
      setTimeout(() => copiedAt === i && (copiedAt = -1), 1500);
    } catch {
      // clipboard unavailable; nothing to do
    }
  }

  const liveTitle = (when: string) => i18n.t.library.newNoteTitle(when);
  const doCopy = (kind: ExportKind) => copyExport(kind, running, liveTitle);
  const doSave = (kind: ExportKind) => saveExport(kind, running, liveTitle);

  async function analyze() {
    notRunning = !(await analyzeNow());
  }

  function badge(item: StateItem): string {
    return `${i18n.t.intel.status[item.status]} · ${Math.round(item.confidence * 100)}%`;
  }
</script>

<div class="intel">
  <div class="tabs" role="tablist">
    <button
      role="tab"
      aria-selected={intel.tab === "insights"}
      class:on={intel.tab === "insights"}
      onclick={() => (intel.tab = "insights")}
    >
      {i18n.t.intel.tabInsights}{#if intel.cards.length}<span class="count">{intel.cards.length}</span>{/if}
    </button>
    <button role="tab" aria-selected={intel.tab === "ask"} class:on={intel.tab === "ask"} onclick={() => (intel.tab = "ask")}>
      {i18n.t.intel.tabAsk}
    </button>
    <button role="tab" aria-selected={intel.tab === "state"} class:on={intel.tab === "state"} onclick={() => (intel.tab = "state")}>
      {i18n.t.intel.tabState}{#if intel.items.length}<span class="count">{intel.items.length}</span>{/if}
    </button>
    {#if canReview}
      <button role="tab" aria-selected={intel.tab === "review"} class:on={intel.tab === "review"} onclick={() => (intel.tab = "review")}>
        {i18n.t.intel.tabReview}
      </button>
    {/if}
    <button class="activity" onclick={() => (activityOpen = true)}>{i18n.t.audit.title}</button>
  </div>

  <Modal bind:open={activityOpen} title={i18n.t.audit.title}>
    <AiActivity meetingId={intel.savedMeetingId ?? ""} />
  </Modal>

  {#if intel.tab === "insights"}
    <div class="feed">
      <label class="ends">
        {i18n.t.intel.endsAt}
        <input
          type="time"
          value={intel.scheduledEnd}
          onchange={(e) => setScheduledEnd(e.currentTarget.value)}
        />
      </label>
      {#if intel.wrapSuggested && !intel.endgame}
        <div class="wrap-banner">
          <p>{i18n.t.intel.wrapSuggested[intel.wrapSuggested === "scheduled" ? "scheduled" : "semantic"]}</p>
          <div class="cactions">
            <button class="btn primary" onclick={wrapUp}>{i18n.t.intel.reviewGaps}</button>
            <button class="copy" onclick={() => (intel.wrapSuggested = null)}>{i18n.t.intel.notYet}</button>
          </div>
        </div>
      {/if}
      {#if intel.auditing}
        <p class="hint">{i18n.t.intel.auditing}</p>
      {:else if intel.audit}
        <section class="audit">
          <h4>{i18n.t.intel.beforeYouWrap}</h4>
          {#if !gapGroups.length}<p class="hint">{i18n.t.intel.nothingOutstanding}</p>{/if}
          {#each gapGroups as g (g.category)}
            <p class="gcat">{i18n.t.intel.gaps[g.category]}</p>
            {#each g.gaps as gap, j (j)}
              <p class="gap">
                {gap.text}{#each gap.cited as id (id)}<span class="cid gref">{id}</span>{/each}
              </p>
            {/each}
          {/each}
          <div class="cactions">
            <button class="copy" onclick={() => copy(20_000, intel.audit!.markdown)}>
              {copiedAt === 20_000 ? i18n.t.intel.copied : i18n.t.intel.copyAll}
            </button>
          </div>
        </section>
      {/if}
      {#if !intel.cards.length && !intel.audit && !intel.auditing}
        <p class="hint">{i18n.t.intel.insightsEmpty}</p>
      {/if}
      {#each [...intel.cards].reverse() as card (card.id)}
        <div class="card">
          <p class="ctitle">{card.candidate.title}</p>
          {#if card.candidate.detail}<p class="cdetail">{card.candidate.detail}</p>{/if}
          {#if card.candidate.suggestedQuestion}
            <p class="cq">“{card.candidate.suggestedQuestion}”</p>
          {/if}
          <p class="imeta">
            <span>{Math.round(card.candidate.confidence * 100)}%</span>
            {#each card.candidate.cited as id (id)}<span class="cid">{id}</span>{/each}
          </p>
          <div class="cactions">
            {#if card.candidate.suggestedQuestion}
              <button class="copy" onclick={() => copy(10_000 + intel.cards.indexOf(card), card.candidate.suggestedQuestion!)}>
                {copiedAt === 10_000 + intel.cards.indexOf(card) ? i18n.t.intel.copied : i18n.t.intel.copyQuestion}
              </button>
            {/if}
            <button class="copy" onclick={() => dismissCard(card.id)}>{i18n.t.intel.dismiss}</button>
          </div>
        </div>
      {/each}
    </div>
  {:else if intel.tab === "review" && canReview}
    <div class="feed">
      {#if intel.reviewApplied !== null}
        <p class="hint">{i18n.t.intel.reviewApplied(intel.reviewApplied)}</p>
      {/if}
      {#if !intel.review}
        <p class="hint">{i18n.t.intel.reviewIntro}</p>
        <div class="cactions">
          <button class="btn primary" disabled={intel.reviewBusy} onclick={startReview}>
            {intel.reviewBusy ? i18n.t.intel.findingFollowUps : i18n.t.intel.reviewStart}
          </button>
        </div>
      {:else}
        {#if intel.review.source === "state"}
          <p class="warn">{i18n.t.intel.reviewFromState}</p>
        {/if}
        {#if !intel.review.followups.length}<p class="hint">{i18n.t.intel.noFollowUps}</p>{/if}
        <ol class="followups">
          {#each intel.review.followups as f (f.n)}
            <li>
              <p class="ftext"><span class="fn">{f.n}.</span> {f.text}</p>
              <div class="classes">
                {#each CLASS_ORDER as c (c)}
                  <button class:on={f.class === c} onclick={() => setFollowUpClass(f.n, c)}>
                    {i18n.t.intel.classes[c]}
                  </button>
                {/each}
              </div>
              {#if f.owner || f.due}
                <p class="imeta">
                  {#if f.owner}<span>{i18n.t.intel.owner}: {f.owner}</span>{/if}
                  {#if f.due}<span>{i18n.t.intel.due}: {f.due}</span>{/if}
                </p>
              {/if}
            </li>
          {/each}
        </ol>
        {#if intel.reviewUnderstood.length}
          <p class="hint">
            {i18n.t.intel.understood}: {intel.reviewUnderstood
              .map((e) => `${e.n} → ${i18n.t.intel.classes[e.class]}`)
              .join(", ")}
          </p>
        {/if}
        <div class="cactions">
          <button class="btn primary" disabled={intel.reviewBusy} onclick={applyReview}>{i18n.t.intel.applyReview}</button>
          <button class="copy" onclick={() => copy(30_000, reviewMarkdown())}>
            {copiedAt === 30_000 ? i18n.t.intel.copied : i18n.t.intel.copy}
          </button>
        </div>
      {/if}
      {#if intel.reviewError}<p class="error">{intel.reviewError}</p>{/if}

      {#if intel.savedProjectId}
        <section class="learn">
          <h4>{i18n.t.intel.projectKnowledge} · {projectName}</h4>
          {#if intel.learningSaved !== null}
            <p class="hint">{i18n.t.intel.learningSaved(intel.learningSaved)}</p>
          {/if}
          {#if !intel.proposals}
            <p class="hint">{i18n.t.intel.learningIntro}</p>
            <div class="cactions">
              <button class="btn" disabled={intel.learningBusy} onclick={proposeLearning}>
                {intel.learningBusy ? i18n.t.intel.findingKnowledge : i18n.t.intel.proposeKnowledge}
              </button>
            </div>
          {:else}
            {#if !intel.proposals.length}<p class="hint">{i18n.t.intel.noKnowledge}</p>{/if}
            {#each intel.proposals as p, i (i)}
              <div class="proposal">
                <input type="checkbox" bind:checked={p.accepted} aria-label={i18n.t.intel.accept} />
                <div class="pbody">
                  <input class="ptext" bind:value={p.text} />
                  <p class="imeta">
                    <span class="st {p.status}">{i18n.t.intel.status[p.status]} · {Math.round(p.confidence * 100)}%</span>
                    <span>{p.kind}</span>
                    {#each p.provenance as ref (ref.sourceRef)}<span>{ref.label}</span>{/each}
                  </p>
                </div>
              </div>
            {/each}
            <div class="cactions">
              <button class="btn primary" disabled={intel.learningBusy} onclick={saveLearning}>
                {i18n.t.intel.saveKnowledge}
              </button>
            </div>
          {/if}
          {#if intel.learningError}<p class="error">{intel.learningError}</p>{/if}
        </section>
      {/if}
    </div>
    {#if intel.review}
      <div class="composer">
        <textarea
          rows="2"
          placeholder={i18n.t.intel.reviewPlaceholder}
          bind:value={reviewDraft}
          onkeydown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              replyReview(reviewDraft).then(() => (reviewDraft = ""));
            }
          }}
        ></textarea>
        <button
          class="btn primary"
          disabled={!reviewDraft.trim() || intel.reviewBusy}
          onclick={() => replyReview(reviewDraft).then(() => (reviewDraft = ""))}>{i18n.t.intel.send}</button
        >
      </div>
    {/if}
  {:else if intel.tab === "ask"}
    <div class="feed" bind:this={feedEl}>
      {#if !intel.turns.length}
        <p class="hint">{i18n.t.intel.askEmpty}</p>
        <div class="suggest">
          {#each i18n.t.intel.suggestions as s (s)}
            <button onclick={() => send(s)}>{s}</button>
          {/each}
        </div>
      {/if}
      {#each intel.turns as turn, i (i)}
        <div class="turn">
          <p class="q">{turn.question}</p>
          {#if turn.pending}
            <p class="thinking">{i18n.t.intel.thinking}</p>
          {:else if turn.error}
            <p class="error">{turn.error}</p>
          {:else if turn.answer}
            <div class="a">
              <p class="answer">{turn.answer.answer}</p>
              {#if !turn.answer.grounded}<p class="warn">{i18n.t.intel.notGrounded}</p>{/if}
              {#if turn.answer.unknownCitations.length}
                <p class="warn">{i18n.t.intel.droppedCitations(turn.answer.unknownCitations.length)}</p>
              {/if}
              {#if turn.answer.citations.length}
                <details class="sources">
                  <summary>{i18n.t.intel.sources} ({turn.answer.citations.length})</summary>
                  <ul>
                    {#each turn.answer.citations as c (c.id)}
                      <li title={c.text}>
                        <span class="cid">{c.id}</span>
                        <span class="clabel">{c.label}</span>
                        <span class="ctext">{c.text}</span>
                      </li>
                    {/each}
                  </ul>
                </details>
              {/if}
              <button class="copy" onclick={() => copy(i, turn.answer!.markdown)}>
                {copiedAt === i ? i18n.t.intel.copied : i18n.t.intel.copy}
              </button>
            </div>
          {/if}
        </div>
      {/each}
    </div>
    <div class="composer">
      <textarea
        rows="2"
        placeholder={i18n.t.intel.askPlaceholder}
        bind:value={draft}
        onkeydown={onKey}
      ></textarea>
      {#if asking}
        <button class="btn" onclick={cancelAsk}>{i18n.t.intel.cancel}</button>
      {:else}
        <button class="btn primary" disabled={!draft.trim()} onclick={() => send()}>{i18n.t.intel.ask}</button>
      {/if}
    </div>
  {:else}
    <div class="state-head">
      <span class="status">
        {#if notRunning}
          {i18n.t.intel.notRunning}
        {:else if intel.error}
          <span class="error">{i18n.t.intel.failed(intel.error)}</span>
        {:else if intel.note === "nothingNew"}
          {i18n.t.intel.nothingNew}
        {:else if intel.lastPass}
          {i18n.t.intel.lastPass(intel.lastPass.applied, intel.lastPass.rejected)}
        {/if}
      </span>
      <button class="btn" disabled={intel.analyzing || !running} onclick={analyze}>
        {intel.analyzing ? i18n.t.intel.analyzing : i18n.t.intel.analyzeNow}
      </button>
    </div>
    {#if groups.length}
      <div class="export-row">
        <button class="btn" onclick={() => doCopy("packet")}>{i18n.t.intel.copyPacket}</button>
        <button class="btn" onclick={() => doSave("record")}>{i18n.t.intel.exportRecord}</button>
        <button class="btn" onclick={() => doSave("json")}>{i18n.t.intel.exportJson}</button>
        {#if intel.exportNote === "copied"}
          <span class="status">{i18n.t.intel.packetCopied}</span>
        {:else if intel.exportNote === "saved"}
          <span class="status">{i18n.t.intel.exportSaved}</span>
        {:else if intel.exportNote.startsWith("error:")}
          <span class="status error">{i18n.t.intel.exportFailed(intel.exportNote.slice(6))}</span>
        {/if}
      </div>
    {/if}
    <div class="feed">
      {#if intel.context.length || intel.contextError || (running && intel.projectId)}
        <section class="group shots">
          <h4>
            {i18n.t.intel.screenshots}
            <button class="linkish" disabled={intel.contextBusy} onclick={() => importContext()}
              >{i18n.t.intel.importImage}</button
            >
          </h4>
          {#if intel.contextError}
            <p class="hint error">{i18n.t.intel.contextFailed(intel.contextError)}</p>
          {:else if !intel.context.length}
            <p class="hint">{i18n.t.intel.screenshotsHint}</p>
          {/if}
          {#each intel.context as shot (shot.sourceId)}
            <div class="item">
              <p class="itext">{shot.title ?? i18n.t.intel.screenshot}</p>
              <p class="imeta">
                <span>{shot.label}</span>
                {#if shot.status === "describing"}
                  <span>{i18n.t.intel.describing}</span>
                {:else if shot.status === "undescribed"}
                  <span class="lc" title={shot.note ?? ""}>{i18n.t.intel.undescribed}</span>
                  <button class="linkish" onclick={() => describeContext(shot.sourceId)}
                    >{i18n.t.intel.describeAgain}</button
                  >
                {/if}
                <button class="linkish" aria-label={i18n.t.intel.removeShot} onclick={() => removeContext(shot.sourceId)}
                  >×</button
                >
              </p>
            </div>
          {/each}
        </section>
      {/if}
      {#if !groups.length}
        <p class="hint">{i18n.t.intel.stateEmpty}</p>
      {/if}
      {#if intel.projectId && intel.memory.length}
        <section class="group">
          <h4>{i18n.t.intel.projectKnowledge}</h4>
          {#each intel.memory as m (m.id)}
            <div class="item">
              <p class="itext">{m.text}</p>
              <p class="imeta">
                <span class="st {m.status}">{i18n.t.intel.status[m.status as "stated" | "inferred"] ?? m.status}</span>
                <span>{m.kind}</span>
                {#each m.provenance as ref, j (j)}
                  <span title={m.expired[j] ? i18n.t.intel.sourceExpired : ""}
                    >{m.expired[j] ? i18n.t.intel.derivedFrom : ""}{ref.label}{m.expired[j] ? " ⌛" : ""}</span
                  >
                {/each}
                <button class="linkish" aria-label={i18n.t.intel.forget} onclick={() => deleteMemory(m.id)}>×</button>
              </p>
            </div>
          {/each}
        </section>
      {/if}
      {#each groups as g (g.kind)}
        <section class="group">
          <h4>{i18n.t.intel.kinds[g.kind]}</h4>
          {#each g.items as item (item.id)}
            <div class="item" class:muted={item.lifecycle === "resolved"}>
              <p class="itext">{item.text}</p>
              <p class="imeta">
                <span class="st {item.status}">{badge(item)}</span>
                {#if item.lifecycle !== "active"}<span class="lc">{i18n.t.intel.lifecycle[item.lifecycle]}</span>{/if}
                {#if item.owner}<span>{i18n.t.intel.owner}: {item.owner}</span>{/if}
                {#if item.due}<span>{i18n.t.intel.due}: {item.due}</span>{/if}
                <span class="iid">{item.id}</span>
              </p>
            </div>
          {/each}
        </section>
      {/each}
    </div>
  {/if}
</div>

<style>
  .intel {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }

  .tabs {
    display: flex;
    gap: 4px;
    padding: 8px 12px 0;
    border-bottom: 1px solid var(--border);
  }

  .tabs button {
    font: inherit;
    font-size: 12.5px;
    color: var(--muted);
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 6px 10px;
    cursor: pointer;
  }

  .tabs button.activity {
    margin-left: auto;
  }

  .tabs button.on {
    color: var(--text);
    border-bottom-color: var(--accent);
  }

  .count {
    margin-left: 6px;
    font-size: 11px;
    color: var(--muted);
  }

  .feed {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .hint {
    font-size: 12.5px;
    color: var(--muted);
    margin: 0;
  }

  .suggest {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .suggest button,
  .btn,
  .copy {
    font: inherit;
    font-size: 12px;
    color: var(--text);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 5px 10px;
    cursor: pointer;
  }

  .suggest button:hover,
  .btn:hover:not(:disabled),
  .copy:hover {
    border-color: var(--accent);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn.primary {
    color: var(--accent);
    border-color: var(--accent);
  }

  .turn {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .q {
    align-self: flex-end;
    max-width: 90%;
    margin: 0;
    font-size: 13px;
    padding: 7px 10px;
    border-radius: 10px;
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    white-space: pre-wrap;
  }

  .a {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }

  .answer {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    white-space: pre-wrap;
    user-select: text;
  }

  .thinking,
  .warn {
    margin: 0;
    font-size: 12px;
    color: var(--muted);
  }

  .error {
    margin: 0;
    font-size: 12px;
    color: var(--danger, #c0392b);
  }

  .sources {
    font-size: 12px;
    width: 100%;
  }

  .sources summary {
    cursor: pointer;
    color: var(--muted);
  }

  .sources ul {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .sources li {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: 8px;
  }

  .cid {
    font-family: ui-monospace, monospace;
    color: var(--accent);
  }

  .clabel {
    color: var(--muted);
  }

  .ctext {
    grid-column: 2;
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
  }

  .ends {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    color: var(--muted);
  }

  .ends input {
    font: inherit;
    font-size: 12px;
    color: var(--text);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 2px 6px;
  }

  .wrap-banner {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px 12px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }

  .wrap-banner p {
    margin: 0;
    font-size: 12.5px;
  }

  .audit {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 10px 12px;
    border: 1px solid var(--accent);
    border-radius: 8px;
  }

  .audit h4 {
    margin: 0 0 4px;
    font-size: 11.5px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--accent);
  }

  .gcat {
    margin: 6px 0 0;
    font-size: 11.5px;
    font-weight: 600;
    color: var(--muted);
  }

  .gap {
    margin: 0;
    font-size: 13px;
    user-select: text;
  }

  .gref {
    margin-left: 6px;
    font-size: 11px;
  }

  .card {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-left: 3px solid var(--accent);
    border-radius: 8px;
  }

  .ctitle {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    user-select: text;
  }

  .cdetail,
  .cq {
    margin: 0;
    font-size: 12.5px;
    user-select: text;
  }

  .cq {
    color: var(--muted);
  }

  .cactions {
    display: flex;
    gap: 6px;
  }

  .followups {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .ftext {
    margin: 0;
    font-size: 13px;
    user-select: text;
  }

  .fn {
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .classes {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 5px;
  }

  .classes button {
    font: inherit;
    font-size: 11px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 8px;
    cursor: pointer;
  }

  .classes button.on {
    color: var(--accent);
    border-color: var(--accent);
  }

  .learn {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 6px;
    padding-top: 10px;
    border-top: 1px solid var(--border);
  }

  .learn h4 {
    margin: 0;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }

  .proposal {
    display: flex;
    gap: 8px;
    align-items: flex-start;
  }

  .proposal input[type="checkbox"] {
    margin-top: 6px;
  }

  .pbody {
    flex: 1;
    min-width: 0;
  }

  .ptext {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    font-size: 13px;
    color: var(--text);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 3px 5px;
  }

  .ptext:hover,
  .ptext:focus {
    border-color: var(--border);
  }

  .linkish {
    margin-left: auto;
    font: inherit;
    color: var(--muted);
    background: transparent;
    border: none;
    cursor: pointer;
  }

  .composer {
    display: flex;
    gap: 8px;
    align-items: flex-end;
    padding: 10px 12px;
    border-top: 1px solid var(--border);
  }

  .composer textarea {
    flex: 1;
    font: inherit;
    font-size: 13px;
    resize: none;
    color: var(--text);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 9px;
  }

  .export-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    padding: 8px 14px;
    border-bottom: 1px solid var(--border);
  }

  .state-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
  }

  .status {
    font-size: 12px;
    color: var(--muted);
  }

  .group h4 {
    margin: 0 0 6px;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }

  .item {
    padding: 6px 0;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 60%, transparent);
  }

  .item.muted .itext {
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
    gap: 8px;
    font-size: 11px;
    color: var(--muted);
  }

  .st.stated {
    color: var(--text);
  }

  .st.suggested {
    font-style: italic;
  }

  .iid {
    margin-left: auto;
    font-family: ui-monospace, monospace;
  }
</style>

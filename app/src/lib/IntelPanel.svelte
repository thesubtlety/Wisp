<script lang="ts">
  // The meeting intelligence panel's body: Ask (questions answered with cited evidence, each answer
  // copyable) and State (the structured meeting state, with Analyze now). Lives inside AssistPanel.
  import { i18n } from "$lib/i18n.svelte";
  import {
    intel,
    askQuestion,
    cancelAsk,
    analyzeNow,
    dismissCard,
    wrapUp,
    setScheduledEnd,
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
  </div>

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
    <div class="feed">
      {#if !groups.length}
        <p class="hint">{i18n.t.intel.stateEmpty}</p>
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

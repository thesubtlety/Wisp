<script lang="ts">
  // Review a saved meeting after the fact: find its follow-ups, correct them in plain words, class
  // each one, then apply them to the meeting's state. When the meeting has a project, propose what
  // the project should remember and save the accepted items. Calls the same intel_review_* /
  // intel_learning_* commands the live post-call review uses, keyed by this meeting's id. The
  // backend holds one review session at a time, which is fine: the live pane and this view are
  // never shown together.
  import { invoke } from "@tauri-apps/api/core";
  import { i18n } from "$lib/i18n.svelte";
  import { copyText } from "$lib/clipboard";
  import { CLASS_ORDER, type FollowUp, type FollowUpClass, type Proposal } from "$lib/intel.svelte";

  let {
    meetingId,
    projectId = "",
    projectName = "",
  }: { meetingId: string; projectId?: string; projectName?: string } = $props();

  type Review = { followups: FollowUp[]; source: "model" | "state"; note: string | null };

  let review = $state<Review | null>(null);
  let reviewBusy = $state(false);
  let reviewError = $state("");
  let understood = $state<{ n: number; class: FollowUpClass }[]>([]);
  let applied = $state<number | null>(null);
  let draft = $state("");

  let proposals = $state<Proposal[] | null>(null);
  let learningBusy = $state(false);
  let learningError = $state("");
  let learningSaved = $state<number | null>(null);

  let copied = $state(false);

  const t = $derived(i18n.t.intel);

  // A new meeting starts a fresh review.
  $effect(() => {
    void meetingId;
    review = null;
    reviewBusy = false;
    reviewError = "";
    understood = [];
    applied = null;
    draft = "";
    proposals = null;
    learningBusy = false;
    learningError = "";
    learningSaved = null;
  });

  function reviewMarkdown(): string {
    return (review?.followups ?? [])
      .map((f) => `${f.n}. [${t.classes[f.class]}] ${f.text}`)
      .join("\n");
  }

  async function start() {
    reviewBusy = true;
    reviewError = "";
    applied = null;
    try {
      review = await invoke<Review>("intel_review_start", { id: meetingId });
    } catch (e) {
      reviewError = String(e);
    } finally {
      reviewBusy = false;
    }
  }

  async function reply(text: string) {
    if (!review || !text.trim()) return;
    reviewBusy = true;
    reviewError = "";
    try {
      const r = await invoke<{ followups: FollowUp[]; understood: { n: number; class: FollowUpClass }[] }>(
        "intel_review_reply",
        { reply: text },
      );
      review.followups = r.followups;
      understood = r.understood;
      draft = "";
    } catch (e) {
      reviewError = String(e);
    } finally {
      reviewBusy = false;
    }
  }

  async function setClass(n: number, cls: FollowUpClass) {
    if (!review) return;
    try {
      review.followups = await invoke<FollowUp[]>("intel_review_set", { n, class: cls });
    } catch (e) {
      reviewError = String(e);
    }
  }

  async function apply() {
    const toProject = review?.followups.some((f) => f.class === "project_memory") ?? false;
    reviewBusy = true;
    let ok = false;
    try {
      applied = await invoke<number>("intel_review_apply");
      review = null;
      understood = [];
      ok = true;
    } catch (e) {
      reviewError = String(e);
    } finally {
      reviewBusy = false;
    }
    if (ok && toProject && projectId && !proposals) await propose();
  }

  async function propose() {
    learningBusy = true;
    learningError = "";
    learningSaved = null;
    try {
      proposals = await invoke<Proposal[]>("intel_learning_propose", { id: meetingId });
    } catch (e) {
      learningError = String(e);
    } finally {
      learningBusy = false;
    }
  }

  async function save() {
    if (!proposals) return;
    learningBusy = true;
    try {
      learningSaved = await invoke<number>("intel_learning_save", { id: meetingId, proposals });
      proposals = null;
    } catch (e) {
      learningError = String(e);
    } finally {
      learningBusy = false;
    }
  }

  async function copy() {
    try {
      await copyText(reviewMarkdown());
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      // clipboard unavailable; nothing to do
    }
  }
</script>

<div class="review">
  <div class="feed">
    {#if applied !== null}
      <p class="muted">{t.reviewApplied(applied)}</p>
    {/if}
    {#if !review}
      <p class="muted">{t.reviewIntro}</p>
      <div class="cactions">
        <button class="btn primary" disabled={reviewBusy} onclick={start}>
          {reviewBusy ? t.findingFollowUps : t.reviewStart}
        </button>
      </div>
    {:else}
      {#if review.source === "state"}
        <p class="warn">{t.reviewFromState}</p>
      {/if}
      {#if !review.followups.length}<p class="muted">{t.noFollowUps}</p>{:else}<p class="muted">{t.reviewHelp}</p>{/if}
      <ol class="followups">
        {#each review.followups as f (f.n)}
          <li>
            <p class="ftext"><span class="fn">{f.n}.</span> {f.text}</p>
            <div class="classes">
              {#each CLASS_ORDER as c (c)}
                <button class="chip" class:on={f.class === c} title={t.classHelp[c]} onclick={() => setClass(f.n, c)}>
                  {t.classes[c]}
                </button>
              {/each}
            </div>
            {#if f.owner || f.due}
              <p class="imeta">
                {#if f.owner}<span>{t.owner}: {f.owner}</span>{/if}
                {#if f.due}<span>{t.due}: {f.due}</span>{/if}
              </p>
            {/if}
          </li>
        {/each}
      </ol>
      {#if understood.length}
        <p class="muted">
          {t.understood}: {understood.map((e) => `${e.n} → ${t.classes[e.class]}`).join(", ")}
        </p>
      {/if}
      <div class="cactions">
        <button class="btn primary" disabled={reviewBusy} onclick={apply}>{t.applyReview}</button>
        <button class="btn" onclick={copy}>{copied ? t.copied : t.copy}</button>
      </div>
    {/if}
    {#if reviewError}<p class="err">{reviewError}</p>{/if}

    {#if projectId}
      <section class="learn">
        <h4>{t.projectKnowledge}{projectName ? ` · ${projectName}` : ""}</h4>
        {#if learningSaved !== null}
          <p class="muted">{t.learningSaved(learningSaved)}</p>
        {/if}
        {#if !proposals}
          <p class="muted">{t.learningIntro}</p>
          <div class="cactions">
            <button class="btn" disabled={learningBusy} onclick={propose}>
              {learningBusy ? t.findingKnowledge : t.proposeKnowledge}
            </button>
          </div>
        {:else}
          {#if !proposals.length}<p class="muted">{t.noKnowledge}</p>{/if}
          {#each proposals as p, i (i)}
            <div class="proposal">
              <input type="checkbox" bind:checked={p.accepted} aria-label={t.accept} />
              <div class="pbody">
                <input class="ptext" bind:value={p.text} />
                <p class="imeta">
                  <span class="st {p.status}">{t.status[p.status]} · {Math.round(p.confidence * 100)}%</span>
                  <span>{p.kind}</span>
                  {#each p.provenance as ref (ref.sourceRef)}<span>{ref.label}</span>{/each}
                </p>
              </div>
            </div>
          {/each}
          <div class="cactions">
            <button class="btn primary" disabled={learningBusy} onclick={save}>{t.saveKnowledge}</button>
          </div>
        {/if}
        {#if learningError}<p class="err">{learningError}</p>{/if}
      </section>
    {/if}
  </div>
  {#if review}
    <div class="composer">
      <textarea
        rows="2"
        placeholder={t.reviewPlaceholder}
        bind:value={draft}
        onkeydown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            void reply(draft);
          }
        }}
      ></textarea>
      <button class="btn primary" disabled={!draft.trim() || reviewBusy} onclick={() => reply(draft)}>{t.send}</button>
    </div>
  {/if}
</div>

<style>
  .review {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .feed {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .followups {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .followups li {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 9px;
    min-width: 0;
  }

  .ftext {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .fn {
    color: var(--muted);
  }

  .classes {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }

  .imeta {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 10px;
    margin: 2px 0 0;
    font-size: 11.5px;
    color: var(--muted);
  }

  .learn {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding-top: 6px;
    border-top: 1px solid var(--border);
  }

  .learn h4 {
    margin: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }

  .proposal {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    min-width: 0;
  }

  .pbody {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-width: 0;
  }

  .ptext {
    font: inherit;
    font-size: 12.5px;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 4px 8px;
    min-width: 0;
  }

  .st.inferred {
    opacity: 0.8;
  }

  .cactions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 8px;
  }

  .composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    min-width: 0;
  }

  textarea {
    flex: 1;
    font: inherit;
    font-size: 12.5px;
    line-height: 1.45;
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 6px 8px;
    resize: vertical;
    min-width: 0;
  }

  .muted {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--muted);
  }

  .warn {
    margin: 0;
    font-size: 12px;
    color: var(--warn, #b7791f);
  }

  .err {
    margin: 0;
    font-size: 12.5px;
    color: var(--danger, #c0392b);
    white-space: pre-wrap;
  }

  .btn {
    font-family: inherit;
    font-size: 12px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border-strong);
    border-radius: 7px;
    padding: 3px 10px;
    cursor: pointer;
  }

  .btn:hover:not(:disabled) {
    color: var(--text);
    border-color: var(--accent);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn.primary {
    color: var(--text);
    border-color: var(--accent);
  }

  .chip {
    font: inherit;
    font-size: 11.5px;
    color: var(--muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 9px;
    cursor: pointer;
  }

  .chip.on {
    color: var(--text);
    border-color: var(--accent);
    background: var(--surface-active);
  }

  .chip:hover {
    color: var(--text);
  }
</style>

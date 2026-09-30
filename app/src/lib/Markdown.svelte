<script lang="ts">
  // A small, safe Markdown renderer for the text Wisp writes (summaries, the project brief):
  // headings, paragraphs, bullet and numbered lists (one nested level), quotes, rules, and inline
  // **bold**, _italic_, `code` and [links](url) shown as their text. Everything is rendered as text
  // nodes; nothing is ever inserted as HTML.
  let { md }: { md: string } = $props();

  type Span = { text: string; bold: boolean; italic: boolean; code: boolean };
  type List = { ordered: boolean; start: number; items: Item[] };
  type Item = { text: string; sub: List | null };
  type Block =
    | { kind: "h"; level: 1 | 2 | 3; text: string }
    | { kind: "p"; text: string }
    | { kind: "quote"; paras: string[] }
    | { kind: "hr" }
    | ({ kind: "list" } & List);

  const WORD = /[\p{L}\p{N}]/u;

  /** Inline marks as spans. Unmatched marks stay as plain text. */
  function inline(src: string, bold = false, italic = false): Span[] {
    const out: Span[] = [];
    let buf = "";
    const flush = () => {
      if (buf) out.push({ text: buf, bold, italic, code: false });
      buf = "";
    };
    let i = 0;
    while (i < src.length) {
      const c = src[i];
      if (c === "`") {
        const j = src.indexOf("`", i + 1);
        if (j > i + 1) {
          flush();
          out.push({ text: src.slice(i + 1, j), bold, italic, code: true });
          i = j + 1;
          continue;
        }
      }
      if (src.startsWith("**", i)) {
        const j = src.indexOf("**", i + 2);
        if (j > i + 2) {
          flush();
          out.push(...inline(src.slice(i + 2, j), true, italic));
          i = j + 2;
          continue;
        }
      }
      if (c === "[") {
        const m = /^\[([^\]]+)\]\(([^)\s]*)\)/.exec(src.slice(i));
        if (m) {
          flush();
          out.push(...inline(m[1], bold, italic));
          i += m[0].length;
          continue;
        }
      }
      // *italic* anywhere; _italic_ only at word edges, so snake_case stays as it is.
      const next = src[i + 1];
      if ((c === "*" || c === "_") && next && next !== " " && next !== c && (c === "*" || i === 0 || !WORD.test(src[i - 1]))) {
        let j = src.indexOf(c, i + 1);
        while (j !== -1 && (src[j - 1] === " " || (c === "_" && j + 1 < src.length && WORD.test(src[j + 1])))) {
          j = src.indexOf(c, j + 1);
        }
        if (j > i + 1) {
          flush();
          out.push(...inline(src.slice(i + 1, j), bold, true));
          i = j + 1;
          continue;
        }
      }
      buf += c;
      i += 1;
    }
    flush();
    return out;
  }

  function parse(src: string): Block[] {
    const blocks: Block[] = [];
    // Whether the previous line was blank, so a plain line starts a new paragraph.
    let gap = true;
    for (const raw of src.replace(/\r\n?/g, "\n").split("\n")) {
      const line = raw.trim();
      const indent = raw.replace(/\t/g, "    ").search(/\S/);
      const last = blocks[blocks.length - 1];
      if (!line) {
        gap = true;
        continue;
      }
      const h = /^(#{1,6})\s+(.*)$/.exec(line);
      const quote = /^>\s?(.*)$/.exec(line);
      const bullet = /^[-*+]\s+(.*)$/.exec(line);
      const numbered = /^(\d{1,9})[.)]\s+(.*)$/.exec(line);
      if (/^([-*_])(\s*\1){2,}$/.test(line)) {
        blocks.push({ kind: "hr" });
      } else if (h) {
        blocks.push({ kind: "h", level: Math.min(h[1].length, 3) as 1 | 2 | 3, text: h[2].replace(/\s+#+$/, "") });
      } else if (quote) {
        const text = quote[1].trim();
        if (last?.kind === "quote" && !gap) {
          if (!text) last.paras.push("");
          else if (last.paras[last.paras.length - 1]) last.paras[last.paras.length - 1] += ` ${text}`;
          else last.paras[last.paras.length - 1] = text;
        } else {
          blocks.push({ kind: "quote", paras: [text] });
        }
      } else if (bullet || numbered) {
        const ordered = !bullet;
        const text = bullet ? bullet[1] : numbered![2];
        const start = numbered ? Number(numbered[1]) : 1;
        const parent = last?.kind === "list" ? last.items[last.items.length - 1] : null;
        if (indent >= 2 && parent) {
          if (parent.sub?.ordered === ordered) parent.sub.items.push({ text, sub: null });
          else parent.sub = { ordered, start, items: [{ text, sub: null }] };
        } else if (last?.kind === "list" && last.ordered === ordered) {
          last.items.push({ text, sub: null });
        } else {
          blocks.push({ kind: "list", ordered, start, items: [{ text, sub: null }] });
        }
      } else if (!gap && last?.kind === "p") {
        last.text += ` ${line}`;
      } else if (!gap && last?.kind === "list") {
        // A wrapped list item.
        const item = last.items[last.items.length - 1];
        const target = item.sub ? item.sub.items[item.sub.items.length - 1] : item;
        target.text += ` ${line}`;
      } else {
        blocks.push({ kind: "p", text: line });
      }
      gap = false;
    }
    return blocks.map((b) => (b.kind === "quote" ? { ...b, paras: b.paras.filter(Boolean) } : b));
  }

  const blocks = $derived(parse(md));
</script>

{#snippet text(src: string)}{#each inline(src) as s, i (i)}{#if s.code}<code>{s.text}</code>{:else if s.bold && s.italic}<strong><em>{s.text}</em></strong>{:else if s.bold}<strong>{s.text}</strong>{:else if s.italic}<em>{s.text}</em>{:else}{s.text}{/if}{/each}{/snippet}

{#snippet list(l: List)}
  {#if l.ordered}
    <ol start={l.start}>
      {#each l.items as it, i (i)}<li>{@render text(it.text)}{#if it.sub}{@render list(it.sub)}{/if}</li>{/each}
    </ol>
  {:else}
    <ul>
      {#each l.items as it, i (i)}<li>{@render text(it.text)}{#if it.sub}{@render list(it.sub)}{/if}</li>{/each}
    </ul>
  {/if}
{/snippet}

<div class="md">
  {#each blocks as b, i (i)}
    {#if b.kind === "h"}
      {#if b.level === 1}
        <h1>{@render text(b.text)}</h1>
      {:else if b.level === 2}
        <h2>{@render text(b.text)}</h2>
      {:else}
        <h3>{@render text(b.text)}</h3>
      {/if}
    {:else if b.kind === "p"}
      <p>{@render text(b.text)}</p>
    {:else if b.kind === "quote"}
      <blockquote>
        {#each b.paras as para, j (j)}<p>{@render text(para)}</p>{/each}
      </blockquote>
    {:else if b.kind === "hr"}
      <hr />
    {:else}
      {@render list(b)}
    {/if}
  {/each}
</div>

<style>
  .md {
    font-size: 13.5px;
    line-height: 1.6;
    color: var(--text);
    user-select: text;
    overflow-wrap: anywhere;
  }
  .md > :first-child {
    margin-top: 0;
  }
  .md > :last-child {
    margin-bottom: 0;
  }
  h1 {
    margin: 0 0 4px;
    font-size: 18px;
    font-weight: 600;
    line-height: 1.3;
  }
  h2 {
    margin: 20px 0 6px;
    padding-bottom: 4px;
    font-size: 14.5px;
    font-weight: 600;
    border-bottom: 1px solid color-mix(in srgb, var(--border) 70%, transparent);
  }
  h3 {
    margin: 14px 0 4px;
    font-size: 13.5px;
    font-weight: 600;
  }
  p {
    margin: 0 0 8px;
  }
  em {
    color: var(--muted);
  }
  strong em,
  h1 em,
  h2 em,
  h3 em {
    color: inherit;
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12.5px;
    padding: 1px 4px;
    border-radius: 4px;
    background: color-mix(in srgb, var(--border) 45%, transparent);
  }
  ul,
  ol {
    margin: 0 0 8px;
    padding-left: 20px;
  }
  li {
    margin: 2px 0;
  }
  li > ul,
  li > ol {
    margin: 2px 0 0;
  }
  blockquote {
    margin: 0 0 8px;
    padding: 2px 0 2px 12px;
    border-left: 3px solid var(--border);
    color: var(--muted);
  }
  blockquote p:last-child {
    margin-bottom: 0;
  }
  hr {
    margin: 14px 0;
    border: 0;
    border-top: 1px solid var(--border);
  }
</style>

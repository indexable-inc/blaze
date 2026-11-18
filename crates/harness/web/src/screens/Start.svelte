<script lang="ts">
  import { post, type StartScreen } from "../protocol";
  import Meter from "../Meter.svelte";

  let { screen }: { screen: StartScreen } = $props();

  // The one state the page keeps: what is being typed. It stays until the screen changes away from start.
  let text = $state("");
  let field: HTMLTextAreaElement | undefined = $state();

  $effect(() => {
    field?.focus();
  });

  function ask(course: boolean) {
    const t = text.trim();
    if (t.length === 0) return;
    post({ t: "ask", text: t, course });
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      ask(e.metaKey);
    }
  }

  // The caret is always there: a press on the empty page puts it back.
  function onpointerdown(e: PointerEvent) {
    const el = e.target as Element | null;
    if (el && el.closest("button, a, textarea")) return;
    e.preventDefault();
    field?.focus();
  }

  const rows = $derived(screen.continue.slice(0, 3));
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<main class="start" {onpointerdown}>
  <div class="column inner">
    <h1 id="ask-label">What do you want to understand?</h1>

    <textarea
      bind:this={field}
      bind:value={text}
      {onkeydown}
      class="field"
      rows="1"
      spellcheck="false"
      autocomplete="off"
      placeholder="Ask anything"
      aria-labelledby="ask-label"
      aria-describedby="ask-hint"
    ></textarea>

    <p class="hint" id="ask-hint">
      <span class="key">return</span> to ask
      <span class="gap"></span>
      <span class="key">⌘</span><span class="key">return</span> to learn it as a lesson
    </p>

    {#if screen.note}
      <p class="note" role="alert">{screen.note}</p>
    {/if}

    {#if rows.length > 0}
      <section class="continue" aria-labelledby="continue-label">
        <h2 class="kicker" id="continue-label">Continue</h2>
        <ul>
          {#each rows as row (row.id)}
            <li>
              <button class="row" onclick={() => post({ t: "open", course: row.id })}>
                <span class="title">{row.title}</span>
                <span class="due">{row.due > 0 ? `${row.due} due` : ""}</span>
                <Meter value={row.mastery} label="{row.title} mastery" width="88px" />
              </button>
            </li>
          {/each}
        </ul>
      </section>
    {/if}
  </div>
</main>

<style>
  .start {
    height: 100%;
    overflow-y: auto;
  }
  .inner {
    padding-top: max(96px, 26vh);
    padding-bottom: 64px;
  }
  h1 {
    margin: 0 0 28px;
    font-size: 34px;
    font-weight: 600;
    line-height: 1.2;
    letter-spacing: -0.02em;
    color: var(--ink-strong);
  }
  .field {
    display: block;
    width: 100%;
    margin: 0;
    padding: 18px 20px;
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    background: var(--card);
    color: var(--ink-strong);
    font-size: 21px;
    line-height: 1.45;
    resize: none;
    field-sizing: content;
    min-height: calc(21px * 1.45 + 38px);
    max-height: 40vh;
    caret-color: var(--ink-strong);
  }
  .field:focus,
  .field:focus-visible {
    outline: none;
    border-color: var(--line-strong);
  }
  .field::placeholder {
    color: var(--ink-faint);
  }
  .hint {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 14px 2px 0;
    font-size: 14px;
    color: var(--ink-faint);
  }
  .hint .key {
    color: var(--ink-faint);
    border-color: var(--line-strong);
  }
  .gap {
    width: 14px;
  }
  .note {
    margin: 18px 2px 0;
    font-size: 15px;
    color: var(--error);
  }
  .continue {
    margin-top: 64px;
  }
  .continue .kicker {
    margin: 0 0 6px;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .row {
    display: grid;
    grid-template-columns: 1fr auto auto;
    align-items: center;
    gap: 20px;
    width: calc(100% + 24px);
    margin: 0 -12px;
    min-height: 50px;
    padding: 0 12px;
    border-radius: var(--radius);
  }
  .row:hover {
    background: var(--hover);
  }
  .title {
    font-size: 17px;
    font-weight: 500;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row:hover .title {
    color: var(--ink-strong);
  }
  .due {
    font-size: 14px;
    color: var(--ink-faint);
    font-variant-numeric: tabular-nums;
  }
</style>

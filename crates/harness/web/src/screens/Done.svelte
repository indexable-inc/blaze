<script lang="ts">
  import { post, type DoneScreen } from "../protocol";

  let { screen }: { screen: DoneScreen } = $props();

  function go() {
    post(screen.next ? { t: "next" } : { t: "back" });
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.isComposing || e.metaKey || e.ctrlKey) return;
    if (e.key === "Escape") {
      e.preventDefault();
      post({ t: "back" });
    } else if (e.key === "Enter") {
      e.preventDefault();
      go();
    }
  }
</script>

<svelte:window {onkeydown} />

<main class="done">
  <div class="column inner">
    <p class="kicker">{screen.kicker}</p>
    <h1>{screen.title}</h1>
    {#if screen.score}<p class="score">{screen.score}</p>{/if}

    <div class="actions">
      <button class="btn btn-primary" onclick={go}>
        {screen.next ? "Keep going" : "Back"} <span class="key">return</span>
      </button>
      {#if screen.next}
        <p class="next"><span class="label">Next</span> {screen.next}</p>
      {/if}
    </div>
  </div>
</main>

<style>
  .done {
    height: 100%;
    overflow-y: auto;
  }
  .inner {
    padding-top: max(96px, 28vh);
    padding-bottom: 64px;
  }
  h1 {
    margin: 10px 0 0;
    font-size: 34px;
    font-weight: 600;
    line-height: 1.2;
    letter-spacing: -0.02em;
    color: var(--ink-strong);
    text-wrap: balance;
  }
  .score {
    margin: 14px 0 0;
    font-size: 21px;
    line-height: 1.45;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 20px;
    margin-top: 40px;
  }
  .next {
    margin: 0;
    font-size: 15px;
    color: var(--ink-muted);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .label {
    color: var(--ink-faint);
    margin-right: 4px;
  }
</style>

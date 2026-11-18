<script lang="ts">
  import { post, type MakingScreen } from "../protocol";

  let { screen }: { screen: MakingScreen } = $props();

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      post({ t: "cancel" });
    }
  }
</script>

<svelte:window {onkeydown} />

<main class="making" aria-busy="true">
  <div class="column inner">
    <p class="kicker">{screen.kicker}</p>
    <h1>{screen.title}</h1>
    <p class="sub" role="status">
      <span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>
      {screen.sub}
    </p>
    <div class="actions">
      <button class="btn btn-quiet" onclick={() => post({ t: "cancel" })}>
        Cancel <span class="key">esc</span>
      </button>
    </div>
  </div>
</main>

<style>
  .making {
    height: 100%;
    overflow-y: auto;
  }
  .inner {
    padding-top: max(96px, 30vh);
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
  .sub {
    display: flex;
    align-items: center;
    gap: 12px;
    margin: 18px 0 0;
    font-size: 17px;
    color: var(--ink-muted);
  }
  /* A still mark for "writing": three dots stepping down in ink. */
  .dots {
    display: inline-flex;
    gap: 4px;
  }
  .dots i {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--ink-strong);
  }
  .dots i:nth-child(2) {
    background: var(--ink-muted);
  }
  .dots i:nth-child(3) {
    background: var(--ink-faint);
  }
  .actions {
    margin: 40px 0 0 -14px;
  }
</style>

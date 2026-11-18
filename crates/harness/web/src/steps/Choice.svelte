<script lang="ts">
  import { post, type Mark } from "../protocol";
  import Inline from "../Inline.svelte";
  import Icon from "../Icon.svelte";

  let {
    prompt,
    code,
    options,
    picked,
    mark,
  }: { prompt: string; code: string; options: string[]; picked: number | null; mark: Mark | null } = $props();

  // The right option once marked: the pick when right, else the option whose text is the answer.
  const rightAt = $derived(mark === null ? -1 : mark.right ? (picked ?? -1) : options.indexOf(mark.answer));

  function state(i: number): "idle" | "picked" | "right" | "wrong" | "rest" {
    if (mark === null) return picked === i ? "picked" : "idle";
    if (i === rightAt) return "right";
    if (i === picked) return "wrong";
    return "rest";
  }
</script>

<h2 class="prompt" id="choice-prompt"><Inline text={prompt} /></h2>
{#if code}
  <pre class="code"><code>{code}</code></pre>
{/if}

<div class="options" role="radiogroup" aria-labelledby="choice-prompt">
  {#each options as option, i (i)}
    {@const s = state(i)}
    <button
      class="option {s}"
      role="radio"
      aria-checked={picked === i}
      aria-disabled={mark !== null}
      data-option={i}
      tabindex={mark !== null ? -1 : picked === i || (picked === null && i === 0) ? 0 : -1}
      onclick={() => {
        if (mark === null) post({ t: "pick", i });
      }}
    >
      <span class="mark" aria-hidden="true">
        {#if s === "right"}
          <Icon name="check" size={18} />
        {:else if s === "wrong"}
          <Icon name="cross" size={18} />
        {:else if i < 9}
          {i + 1}
        {/if}
      </span>
      <span class="text rich"><Inline text={option} /></span>
      {#if s === "right"}<span class="sr-only">, the right answer</span>{/if}
      {#if s === "wrong"}<span class="sr-only">, your pick, not right</span>{/if}
    </button>
  {/each}
</div>

<style>
  pre {
    margin-top: 24px;
  }
  .options {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 28px;
  }
  .option {
    display: flex;
    align-items: flex-start;
    gap: 16px;
    width: 100%;
    min-height: 58px;
    padding: 15px 18px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-lg);
    font-size: 18px;
    line-height: 1.45;
    color: var(--ink);
  }
  .option.idle:hover {
    background: var(--hover);
    border-color: var(--ink-faint);
    color: var(--ink-strong);
  }
  .option.picked,
  .option.right,
  .option.wrong {
    border-width: 2px;
    padding: 14px 17px;
  }
  .option.picked {
    border-color: var(--ink-strong);
    background: var(--wash);
    color: var(--ink-strong);
  }
  .option.right {
    border-color: var(--success);
    background: var(--success-wash);
    color: var(--ink-strong);
    cursor: default;
  }
  .option.wrong {
    border-color: var(--error);
    background: var(--error-wash);
    color: var(--ink-strong);
    cursor: default;
  }
  .option.rest {
    color: var(--ink-faint);
    border-color: var(--line);
    cursor: default;
  }
  .mark {
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: 7px;
    border: 1px solid var(--line-strong);
    font-size: 13px;
    font-weight: 500;
    color: var(--ink-muted);
    font-variant-numeric: tabular-nums;
  }
  .picked .mark {
    border-color: var(--ink-strong);
    background: var(--ink-strong);
    color: var(--ground);
  }
  .right .mark {
    border-color: transparent;
    background: var(--success);
    color: var(--ground);
  }
  .wrong .mark {
    border-color: transparent;
    background: var(--error);
    color: var(--ground);
  }
  .rest .mark {
    border-color: var(--line);
    color: var(--ink-faint);
  }
  .text {
    flex: 1;
    min-width: 0;
    padding-top: 0;
  }
</style>

<script lang="ts">
  import { post, type StepScreen } from "../protocol";
  import Icon from "../Icon.svelte";
  import Inline from "../Inline.svelte";
  import Meter from "../Meter.svelte";
  import Read from "../steps/Read.svelte";
  import Choice from "../steps/Choice.svelte";
  import Fill from "../steps/Fill.svelte";
  import Order from "../steps/Order.svelte";

  let { screen }: { screen: StepScreen } = $props();

  const step = $derived(screen.step);
  const mark = $derived(screen.mark);
  // A step has no id in version 1: its course, lesson and content name it, so local typing resets on a new step.
  const stepKey = $derived(JSON.stringify([screen.course, screen.lesson, screen.again, step]));

  function primary() {
    if (step.kind === "read" || mark !== null) post({ t: "next" });
    else if (screen.can_submit) post({ t: "submit" });
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.defaultPrevented || e.isComposing || e.metaKey || e.ctrlKey) return;
    const target = e.target as HTMLElement | null;
    const inField = target?.tagName === "INPUT" || target?.tagName === "TEXTAREA";

    if (e.key === "Escape") {
      e.preventDefault();
      post({ t: "back" });
      return;
    }
    if (e.key === "Enter") {
      // A focused control other than a step's own rows answers its own Return (the back chevron, a link).
      if (target && !inField && target.closest("button, a") && !target.closest("[data-option], [data-item], .primary"))
        return;
      e.preventDefault();
      primary();
      return;
    }
    if (step.kind !== "choice" || mark !== null || inField || e.altKey) return;
    const n = step.options.length;
    if (/^[1-9]$/.test(e.key)) {
      const i = Number(e.key) - 1;
      if (i < n) {
        e.preventDefault();
        post({ t: "pick", i });
      }
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const from = screen.picked;
      const i = from === null ? (e.key === "ArrowDown" ? 0 : n - 1) : (from + (e.key === "ArrowDown" ? 1 : n - 1)) % n;
      post({ t: "pick", i });
      // Focus follows the choice when it was on the options.
      if (target?.closest("[data-option]")) document.querySelector<HTMLElement>(`[data-option="${i}"]`)?.focus();
    }
  }

  const hint = $derived.by(() => {
    if (mark !== null) return "";
    if (step.kind === "choice") return step.options.length > 1 ? `1–${Math.min(9, step.options.length)} to pick` : "";
    if (step.kind === "order") return "Drag, or ⌥↑ ⌥↓ to move";
    return "";
  });
</script>

<svelte:window {onkeydown} />

<main class="step">
  <header class="top">
    <button class="back" onclick={() => post({ t: "back" })} aria-label="Back to the course" title="Back (esc)">
      <Icon name="back" />
    </button>
    <div class="bar">
      <Meter value={screen.progress} label="Lesson progress" />
    </div>
  </header>

  <div class="scroll">
    <div class="column content">
      <p class="kicker line">
        {#if screen.again}
          <span class="again"><Icon name="again" size={15} />Again</span><span class="dot">·</span>
        {/if}
        <span>{screen.course}</span>
        {#if screen.lesson}<span class="dot">·</span><span class="faint">{screen.lesson}</span>{/if}
      </p>

      {#key stepKey}
        <div class="stage">
          {#if step.kind === "read"}
            <Read title={step.title} body={step.body} code={step.code} />
          {:else if step.kind === "choice"}
            <Choice prompt={step.prompt} code={step.code} options={step.options} picked={screen.picked} {mark} />
          {:else if step.kind === "fill"}
            <Fill prompt={step.prompt} typed={screen.typed} {mark} />
          {:else}
            <Order prompt={step.prompt} items={step.items} order={screen.order} {mark} />
          {/if}
        </div>
      {/key}
    </div>
  </div>

  <footer class="bottom" class:right={mark?.right === true} class:wrong={mark?.right === false}>
    <div class="column foot">
      <div class="feedback" role="status" aria-live="polite">
        {#if mark}
          <p class="verdict">
            <Icon name={mark.right ? "check" : "cross"} size={20} />
            {mark.right ? "Right" : "Not quite"}
          </p>
          {#if !mark.right && mark.answer}
            <p class="answer rich"><span class="label">Answer</span> <Inline text={mark.answer} /></p>
          {/if}
          {#if mark.why}
            <p class="why rich"><Inline text={mark.why} /></p>
          {/if}
        {:else if hint}
          <p class="hint">{hint}</p>
        {/if}
      </div>
      <div class="act">
        {#if step.kind === "read" || mark !== null}
          <button class="btn btn-primary primary" onclick={() => post({ t: "next" })}>
            Continue <span class="key">return</span>
          </button>
        {:else}
          <button class="btn btn-primary primary" disabled={!screen.can_submit} onclick={() => post({ t: "submit" })}>
            Check <span class="key">return</span>
          </button>
        {/if}
      </div>
    </div>
  </footer>
</main>

<style>
  .step {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .top {
    flex: none;
    display: grid;
    grid-template-columns: 1fr minmax(0, var(--column)) 1fr;
    align-items: center;
    column-gap: 32px;
    height: 60px;
    padding: 0 16px;
  }
  .back {
    justify-self: start;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 36px;
    height: 36px;
    border-radius: 9px;
    color: var(--ink-muted);
  }
  .back:hover {
    background: var(--hover);
    color: var(--ink-strong);
  }
  .bar {
    grid-column: 2;
  }
  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .content {
    padding-top: max(32px, 9vh);
    padding-bottom: 56px;
  }
  .line {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 14px;
    flex-wrap: wrap;
  }
  .dot,
  .faint {
    color: var(--ink-faint);
  }
  .again {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: var(--ink-strong);
  }
  .bottom {
    flex: none;
    border-top: 1px solid var(--line);
    padding: 20px 0 24px;
  }
  .bottom.right {
    background: var(--success-wash);
    border-top-color: transparent;
  }
  .bottom.wrong {
    background: var(--error-wash);
    border-top-color: transparent;
  }
  .foot {
    display: flex;
    align-items: flex-end;
    gap: 32px;
    min-height: 44px;
  }
  .feedback {
    flex: 1;
    min-width: 0;
  }
  .act {
    flex: none;
  }
  .verdict {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0;
    font-size: 19px;
    font-weight: 600;
    line-height: 26px;
  }
  .right .verdict {
    color: var(--success);
  }
  .wrong .verdict {
    color: var(--error);
  }
  .answer {
    margin: 8px 0 0;
    font-size: 17px;
    line-height: 1.5;
    color: var(--ink-strong);
    font-weight: 500;
  }
  .label {
    color: var(--ink-muted);
    font-weight: 500;
    margin-right: 4px;
  }
  .why {
    margin: 6px 0 0;
    font-size: 16px;
    line-height: 1.55;
    color: var(--ink);
    max-width: 520px;
    text-wrap: pretty;
  }
  .hint {
    margin: 0;
    font-size: 14px;
    line-height: 44px;
    color: var(--ink-faint);
  }
</style>

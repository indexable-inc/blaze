<script lang="ts">
  import { post, type Mark } from "../protocol";
  import Inline from "../Inline.svelte";

  let { prompt, typed, mark }: { prompt: string; typed: string; mark: Mark | null } = $props();

  // What is being typed is the page's; it starts from the app's copy when the step is drawn.
  // svelte-ignore state_referenced_locally
  let text = $state(typed);
  let input: HTMLInputElement | undefined = $state();

  const at = $derived(prompt.indexOf("___"));
  const before = $derived(at < 0 ? prompt : prompt.slice(0, at));
  const after = $derived(at < 0 ? "" : prompt.slice(at + 3));

  $effect(() => {
    if (mark === null) input?.focus();
  });
</script>

<h2 class="prompt fill">
  <Inline text={before} /><input
    bind:this={input}
    bind:value={text}
    oninput={() => post({ t: "type", text })}
    class="blank"
    class:right={mark?.right === true}
    class:wrong={mark?.right === false}
    readonly={mark !== null}
    spellcheck="false"
    autocomplete="off"
    autocapitalize="off"
    aria-label="The missing words"
  /><Inline text={after} />
</h2>

<style>
  .fill {
    line-height: 1.6;
  }
  .blank {
    display: inline-block;
    vertical-align: baseline;
    field-sizing: content;
    min-width: 5ch;
    max-width: 100%;
    margin: 0;
    padding: 0 6px 2px;
    border: 0;
    border-bottom: 2px solid var(--ink-faint);
    border-radius: 0;
    background: transparent;
    color: var(--ink-strong);
    font: inherit;
    font-weight: 600;
    caret-color: var(--ink-strong);
  }
  .blank:focus {
    border-bottom-color: var(--ink-strong);
    background: var(--hover);
  }
  .blank:focus-visible {
    outline: none;
  }
  .blank.right {
    color: var(--success);
    border-bottom-color: var(--success);
    background: var(--success-wash);
  }
  .blank.wrong {
    color: var(--error);
    border-bottom-color: var(--error);
    background: var(--error-wash);
    text-decoration: line-through;
    text-decoration-thickness: 2px;
  }
</style>

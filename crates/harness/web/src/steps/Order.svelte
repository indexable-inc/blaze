<script lang="ts">
  import { tick } from "svelte";
  import { post, type Mark } from "../protocol";
  import Inline from "../Inline.svelte";

  let { prompt, items, order, mark }: { prompt: string; items: string[]; order: number[]; mark: Mark | null } = $props();

  // The order as drawn: the app's, or the one being dragged to before it is posted.
  // svelte-ignore state_referenced_locally
  let cur = $state([...order]);
  $effect(() => {
    cur = [...order];
  });

  let list: HTMLOListElement | undefined = $state();
  let dragging: number | null = $state(null); // the item (an index into items) held under the pointer
  let said = $state(""); // what a screen reader hears after a move

  function same(a: number[], b: number[]): boolean {
    return a.length === b.length && a.every((v, i) => v === b[i]);
  }

  function send(next: number[]) {
    cur = next;
    if (!same(next, order)) post({ t: "order", order: [...next] });
  }

  function moved(from: number, to: number): number[] {
    const next = [...cur];
    const [it] = next.splice(from, 1);
    if (it !== undefined) next.splice(to, 0, it);
    return next;
  }

  async function focusItem(item: number) {
    await tick();
    list?.querySelector<HTMLElement>(`[data-item="${item}"]`)?.focus();
  }

  function onkeydown(e: KeyboardEvent, pos: number) {
    if (mark !== null) return;
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    e.preventDefault();
    e.stopPropagation();
    const to = e.key === "ArrowUp" ? pos - 1 : pos + 1;
    if (to < 0 || to >= cur.length) return;
    const item = cur[pos] ?? 0;
    if (e.altKey) {
      send(moved(pos, to));
      said = `${items[item] ?? ""}, moved to ${to + 1} of ${cur.length}`;
    }
    void focusItem(e.altKey ? item : (cur[to] ?? 0));
  }

  // Dragging: the row follows the pointer by position in the list (rows swap at once, no motion); released, the
  // order is posted. Listeners sit on the window so moving the row in the DOM never drops the drag.
  function onpointerdown(e: PointerEvent, item: number) {
    if (mark !== null || e.button !== 0) return;
    e.preventDefault();
    (e.currentTarget as HTMLElement).focus();
    dragging = item;
    const move = (ev: PointerEvent) => {
      if (!list || dragging === null) return;
      const rows = [...list.querySelectorAll<HTMLElement>("[data-item]")];
      let to = rows.length - 1;
      for (let i = 0; i < rows.length; i++) {
        const r = rows[i]?.getBoundingClientRect();
        if (r && ev.clientY < r.top + r.height / 2) {
          to = i;
          break;
        }
      }
      const from = cur.indexOf(dragging);
      if (from !== to && from >= 0) cur = moved(from, to);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      dragging = null;
      send(cur);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }
</script>

<h2 class="prompt" id="order-prompt"><Inline text={prompt} /></h2>

<ol bind:this={list} class="rows" aria-labelledby="order-prompt" aria-describedby="order-help">
  {#each cur as item, pos (item)}
    <li>
      <button
        class="row"
        class:held={dragging === item}
        class:right={mark?.right === true}
        class:wrong={mark?.right === false}
        data-item={item}
        aria-disabled={mark !== null}
        aria-roledescription="movable item"
        onpointerdown={(e) => onpointerdown(e, item)}
        onkeydown={(e) => onkeydown(e, pos)}
      >
        <span class="pos" aria-hidden="true">{pos + 1}</span>
        <span class="text rich"><Inline text={items[item] ?? ""} /></span>
        {#if mark === null}
          <svg class="grip" width="14" height="20" viewBox="0 0 14 20" aria-hidden="true">
            {#each [5, 10, 15] as y (y)}
              <circle cx="4.5" cy={y} r="1.4" />
              <circle cx="9.5" cy={y} r="1.4" />
            {/each}
          </svg>
        {/if}
      </button>
    </li>
  {/each}
</ol>
<p id="order-help" class="sr-only">Arrow keys move between items; Option with Up or Down moves the focused item.</p>
<p class="sr-only" aria-live="polite">{said}</p>

<style>
  .rows {
    list-style: none;
    margin: 28px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 16px;
    width: 100%;
    min-height: 58px;
    padding: 14px 16px 14px 18px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-lg);
    background: var(--card);
    font-size: 18px;
    line-height: 1.45;
    color: var(--ink);
    cursor: grab;
    touch-action: none;
    user-select: none;
  }
  .row:hover {
    border-color: var(--ink-faint);
    color: var(--ink-strong);
  }
  .row.held {
    cursor: grabbing;
    border-color: var(--ink-strong);
    background: var(--raised);
    color: var(--ink-strong);
  }
  .row.right {
    border-color: var(--success);
    cursor: default;
  }
  .row.wrong {
    border-color: var(--error);
    cursor: default;
  }
  .row[aria-disabled="true"]:hover {
    color: var(--ink);
  }
  .pos {
    flex: none;
    width: 26px;
    text-align: center;
    font-size: 14px;
    font-weight: 500;
    color: var(--ink-faint);
    font-variant-numeric: tabular-nums;
  }
  .right .pos {
    color: var(--success);
  }
  .wrong .pos {
    color: var(--error);
  }
  .text {
    flex: 1;
    min-width: 0;
  }
  .grip {
    flex: none;
    fill: var(--ink-faint);
  }
  .row:hover .grip,
  .held .grip {
    fill: var(--ink-muted);
  }
</style>

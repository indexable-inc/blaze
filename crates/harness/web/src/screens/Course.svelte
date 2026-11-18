<script lang="ts">
  import { post, type CourseScreen } from "../protocol";
  import Icon from "../Icon.svelte";
  import Inline from "../Inline.svelte";
  import Meter from "../Meter.svelte";
  import { pct } from "../text";

  let { screen }: { screen: CourseScreen } = $props();

  const next = $derived(screen.lessons.find((l) => l.state === "next"));
  const doneCount = $derived(screen.lessons.filter((l) => l.state === "done").length);

  function open(lesson: string) {
    post({ t: "lesson", course: screen.id, lesson });
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.isComposing || e.metaKey || e.ctrlKey) return;
    if (e.key === "Escape") {
      e.preventDefault();
      post({ t: "back" });
    } else if (e.key === "Enter" && next && !(e.target as Element | null)?.closest("button, a")) {
      e.preventDefault();
      open(next.id);
    }
  }
</script>

<svelte:window {onkeydown} />

<main class="course">
  <div class="column inner">
    <button class="back" onclick={() => post({ t: "back" })} aria-label="Back to start" title="Back (esc)">
      <Icon name="back" />
    </button>

    <p class="kicker">Course</p>
    <h1>{screen.title}</h1>
    {#if screen.goal}<p class="goal rich"><Inline text={screen.goal} /></p>{/if}

    <div class="meta">
      <Meter value={screen.mastery} label="Course mastery" width="140px" />
      <span>{pct(screen.mastery)} mastered</span>
      <span class="sep">·</span>
      <span>{doneCount} of {screen.lessons.length} lessons</span>
      {#if screen.due > 0}<span class="sep">·</span><span>{screen.due} due</span>{/if}
    </div>

    <ol class="path" aria-label="Lessons">
      {#each screen.lessons as lesson, i (lesson.id)}
        <li class={lesson.state} class:last={i === screen.lessons.length - 1}>
          <button class="lesson" onclick={() => open(lesson.id)} aria-current={lesson.state === "next" ? "step" : undefined}>
            <span class="node" aria-hidden="true">
              {#if lesson.state === "done"}<Icon name="check" size={14} />{/if}
            </span>
            <span class="words">
              <span class="title">{lesson.title}</span>
              {#if lesson.about}<span class="about rich"><Inline text={lesson.about} /></span>{/if}
            </span>
            {#if lesson.state === "next"}
              <span class="btn btn-primary start">Start <span class="key">return</span></span>
            {:else if lesson.state === "done"}
              <span class="score">{pct(lesson.mastery)}</span>
            {/if}
            <span class="sr-only">, {lesson.state === "done" ? "done" : lesson.state === "next" ? "next" : "later"}</span>
          </button>
        </li>
      {/each}
    </ol>
  </div>
</main>

<style>
  .course {
    height: 100%;
    overflow-y: auto;
  }
  .inner {
    padding-top: 20px;
    padding-bottom: 80px;
  }
  .back {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 36px;
    height: 36px;
    margin: 0 0 max(24px, 6vh) -8px;
    border-radius: 9px;
    color: var(--ink-muted);
  }
  .back:hover {
    background: var(--hover);
    color: var(--ink-strong);
  }
  h1 {
    margin: 8px 0 0;
    font-size: 34px;
    font-weight: 600;
    line-height: 1.2;
    letter-spacing: -0.02em;
    color: var(--ink-strong);
    text-wrap: balance;
  }
  .goal {
    margin: 12px 0 0;
    font-size: 19px;
    line-height: 1.55;
    color: var(--ink-muted);
    text-wrap: pretty;
  }
  .meta {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 24px;
    font-size: 14px;
    color: var(--ink-muted);
    font-variant-numeric: tabular-nums;
  }
  .meta :global(.meter) {
    margin-right: 4px;
  }
  .sep {
    color: var(--ink-faint);
  }
  .path {
    list-style: none;
    margin: 48px 0 0;
    padding: 0;
  }
  li {
    position: relative;
  }
  /* The path: one line through the nodes, from each node to the next. */
  li:not(.last)::before {
    content: "";
    position: absolute;
    left: 25px;
    top: 38px;
    bottom: -18px;
    width: 1px;
    background: var(--line-strong);
  }
  .lesson {
    position: relative;
    display: flex;
    align-items: center;
    gap: 18px;
    width: 100%;
    margin: 0 0 4px;
    padding: 14px 16px 14px 14px;
    border-radius: var(--radius-lg);
  }
  .lesson:hover {
    background: var(--hover);
  }
  .next .lesson {
    background: var(--card);
    border: 1px solid var(--line-strong);
    padding: 17px 16px 17px 13px;
    margin: 4px 0 8px;
  }
  .node {
    position: relative;
    z-index: 1;
    flex: none;
    align-self: flex-start;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    height: 24px;
    margin-top: 1px;
    border-radius: 50%;
    background: var(--ground);
    border: 1.5px solid var(--line-strong);
  }
  .done .node {
    background: var(--ink-muted);
    border-color: var(--ink-muted);
    color: var(--ground);
  }
  .next .node {
    background: var(--card);
    border: 2px solid var(--ink-strong);
  }
  .next .node::after {
    content: "";
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--ink-strong);
  }
  .words {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .title {
    font-size: 17px;
    font-weight: 500;
    line-height: 26px;
    color: var(--ink);
  }
  .about {
    font-size: 15px;
    line-height: 1.5;
    color: var(--ink-muted);
  }
  .next .title {
    font-size: 19px;
    font-weight: 600;
    color: var(--ink-strong);
  }
  .next .about {
    color: var(--ink);
  }
  .later .title {
    color: var(--ink-muted);
  }
  .later .about {
    color: var(--ink-faint);
  }
  .score {
    flex: none;
    font-size: 14px;
    color: var(--ink-faint);
    font-variant-numeric: tabular-nums;
  }
  .start {
    flex: none;
    height: 38px;
    padding: 0 16px;
    font-size: 15px;
  }
  .lesson:hover .start {
    background: var(--ink);
  }
</style>

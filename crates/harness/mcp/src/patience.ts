// Time as words the agent reads. A model does not feel a wait: a cell that blocks for three hours returns, from its
// side, right after it began. So every duration is said in the reply, against what the agent expected, and a long run
// has to be argued for before it starts. Pure functions over numbers (no clock, no IO): kernel.ts feeds them.

/** A cell with no estimate is taken to be this quick: the wait one call covers before a cell is backgrounded. */
export const ASSUMED_MS = 30_000;
/** A cell expected to take longer than this must say why it is worth the wait. */
export const LONG_MS = 5 * 60_000;
/** The fewest characters a reason is: enough for a sentence, so `why: "tests"` does not pass. */
export const WHY_MIN = 30;
/** A cell this many times over its estimate is said to be over. */
export const OVER = 2;
/** A cell with no estimate that ran this long is told it should have had one. */
export const UNSTATED_LONG_MS = 2 * 60_000;

/** `90 s` as `1m30s`, `0.12 s` as `0.12s`, three hours as `3h00m`. */
export function span(ms: number): string {
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)}s`;
  const s = Math.round(ms / 1000);
  if (s < 3600) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
}

/**
 * Before a cell runs: null to let it, or why it is refused. A run expected to be long needs a reason, because the
 * moment to doubt a slow run is before it starts: nothing is felt while it waits.
 */
export function gate(expect_ms: number | undefined, why: string | undefined): string | null {
  if (expect_ms !== undefined && (!Number.isFinite(expect_ms) || expect_ms <= 0)) return `expect_ms is ${expect_ms}: give how long you expect the cell to take, in milliseconds`;
  if (expect_ms === undefined || expect_ms <= LONG_MS) return null;
  if ((why ?? "").trim().length >= WHY_MIN) return null;
  return [
    `Not run: you expect this cell to take ${span(expect_ms)}, and a cell over ${span(LONG_MS)} needs \`why\`: one sentence saying what it will tell you that nothing faster can.`,
    "Something this slow usually means the approach is wrong. Before waiting, ask: can a smaller input answer it (one test, one crate, one file)? can it run in process or under simulation instead of for real?",
    "is a warm or cached copy available? is it already measured somewhere (a profile, a log, an earlier task)? can it run in the background while you do something else (sh, then out)?",
    "If it is still the right run, call again with `why`, and say the estimate and the reason to the person.",
  ].join("\n");
}

/** The hard limit of a cell: the caller's, else a multiple of the estimate, else none (a cell with no estimate is not stopped, only told). */
export function limit(expect_ms: number | undefined, timeout_ms: number | undefined): number | undefined {
  if (timeout_ms !== undefined) return timeout_ms;
  if (expect_ms === undefined) return undefined;
  return Math.max(4 * expect_ms, expect_ms + 60_000);
}

/** What a reply says about a cell's time: `1m12s · expected 20.0s · 3.6× over`, and a line of advice when it earned one. */
export function told(o: { elapsed_ms: number; expect_ms?: number; running: boolean; session_ms: number }): { time: string; advice: string | null } {
  const { elapsed_ms, expect_ms, running, session_ms } = o;
  const session = session_ms >= 10 * 60_000 ? ` · ${span(session_ms)} waited in cells this session` : "";
  if (expect_ms === undefined) {
    const long = elapsed_ms >= UNSTATED_LONG_MS;
    return {
      time: `${span(elapsed_ms)}${long ? " · no estimate given" : ""}${session}`,
      advice: long
        ? `This ${running ? "has run" : "ran"} ${span(elapsed_ms)} with no expect_ms. Say how long you expect before starting anything slow; a wait this long should be questioned before it begins${running ? ", and now: is it stuck, and is there a faster way to learn the same thing?" : "."}`
        : null,
    };
  }
  const ratio = elapsed_ms / expect_ms;
  if (ratio < OVER) return { time: `${span(elapsed_ms)} · expected ${span(expect_ms)}${session}`, advice: null };
  return {
    time: `${span(elapsed_ms)} · expected ${span(expect_ms)} · ${ratio.toFixed(1)}× over${session}`,
    advice: running
      ? `It is ${ratio.toFixed(1)}× over your estimate and still running. Do not just wait: look at what it is doing (task, out, a profile), and decide whether to stop it. Your estimate was wrong or something is; find out which.`
      : `It took ${ratio.toFixed(1)}× your estimate. Before running it or anything like it again, find out why (what was slow, from its output or a profile), and whether a smaller or different check answers the question.`,
  };
}

/** A cell stopped by its limit: why, as stop.ts words it, and what to do next when the limit came from an estimate. */
export function overLimit(limit_ms: number, expect_ms: number | undefined): { why: string; advice: string | null } {
  return expect_ms === undefined
    ? { why: `it passed its limit of ${span(limit_ms)}`, advice: null }
    : {
      why: `it passed its limit of ${span(limit_ms)}, and you expected ${span(expect_ms)}`,
      advice: "That is suspiciously slow. Find out what was slow before trying again; do not rerun it with a longer limit unchanged.",
    };
}

/**
 * The time the session has spent with at least one cell running: the union of the cells' intervals `[start, end]`, so
 * two cells that overlap count once.
 */
export function waited(intervals: readonly (readonly [number, number])[]): number {
  let ms = 0, until = -Infinity;
  for (const [from, to] of [...intervals].sort((a, b) => a[0] - b[0])) {
    if (to <= until) continue;
    ms += to - Math.max(from, until);
    until = to;
  }
  return ms;
}

/** The most notices one backgrounded cell sends while it runs (its end is one more). */
export const NOTICES = 4;

/**
 * When a backgrounded cell next tells the agent it is still running: at 2×, 4×, 8× and 16× its estimate (a cell with
 * no estimate: at 2 minutes, then 4× that each time). `sent` is how many it has sent; undefined when it has sent them all.
 * Elapsed milliseconds since the cell began.
 */
export function noticeAt(expect_ms: number | undefined, sent: number): number | undefined {
  if (sent >= NOTICES) return undefined;
  return expect_ms === undefined ? UNSTATED_LONG_MS * 4 ** sent : expect_ms * OVER * 2 ** sent;
}

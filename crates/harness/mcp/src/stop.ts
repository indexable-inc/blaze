// What stopping a cell means, as decisions and words (pure: no clock, no IO; cancel.ts does the stopping).
//
// 1. A cell is stopped by `cancel('t3')`, by the tool's `cancel` input, or by its limit (4x its estimate, or timeout_ms).
// 2. Its AbortSignal aborts: every typed call that waits (fetch, sh, $, follow, cargo.*, wait, Bun.sleep, the browser,
//    ask) rejects at once with the reason, so the cell's code unwinds through its own `finally` blocks.
// 3. Every process it started is killed with its whole process group (SIGTERM, then SIGKILL after KILL_GRACE_MS), and
//    every harnessd run it started is killed there; what it started with `sh(cmd, { detach: true })` is left, by name.
// 4. Its reply says what was killed, what would not die and what was left running.
// 5. Code that never checks the signal (an await on something of its own, a timer) keeps running in the kernel and the
//    task says so; a synchronous loop blocks the whole kernel and cannot be stopped from inside it, so the watchdog
//    (watchdog.ts) ends the kernel once such a cell is past its limit, and the journal says which cell did it.

/** How long a process group has to exit after SIGTERM before SIGKILL. */
export const KILL_GRACE_MS = 2_000;
/** How long after SIGKILL a group is waited for before it is reported as not dying. */
export const KILL_WAIT_MS = 500;
/** How long a stopped cell's own code has to return before it is said to ignore its signal. */
export const SETTLE_MS = 300;

export type StopReport = {
  id: string;
  /** Why it was stopped, as a clause: "cancelled by t5", "past its limit of 2.0s". */
  why: string;
  /** What was killed and exited; what was killed and did not exit in time; what the cell detached on purpose. */
  killed: string[];
  survived: string[];
  left: string[];
  /** The cell's own code had not returned SETTLE_MS after its signal aborted. */
  ignoring: boolean;
};

const list = (items: string[]) => items.join("; ");

/** The report as the agent reads it. */
export function stopText(r: StopReport): string {
  return [
    `${r.id} stopped: ${r.why}.`,
    r.killed.length ? `Killed: ${list(r.killed)}.` : "It had no process running.",
    r.survived.length ? `Did not exit after SIGKILL (still running): ${list(r.survived)}.` : "",
    r.left.length ? `Left running, detached on purpose: ${list(r.left)} (objects({ live: true }); kill(id) ends one).` : "",
    r.ignoring
      ? `Its own code has not returned: it ignores its signal (an await that never checks \`signal\`, or a timer of its own) and keeps running in the kernel until it does; tasks() shows when it has.`
      : "",
  ].filter(Boolean).join("\n");
}

// What the kernel does about a cell that cannot be stopped from inside: synchronous code (a `for (;;) {}`, a long
// spawnSync) holds the one thread every cell, timer and request shares, so the cell's limit cannot fire and no cancel
// can be heard. A worker thread watches from outside: when a cell is past its limit and the kernel's loop has not
// turned for GRACE_MS, it writes the cell's end into the journal (stopped, saying why) and ends the kernel. The agent's
// call fails instead of hanging for ever, and the next kernel has the task list and that reason back (journal.ts).
//
// Why not a worker per cell, which could be terminated alone: cells share live bindings (evaluator.ts): a handle, a
// function, a socket one cell made is used by the next. Across workers only structured clones cross, so every such
// value would be lost or copied. Ending the kernel costs the names (rerun makes them again); a worker per cell would
// cost shared scope for every cell, always, to save them in this one case.
//
// One case is let be: the loop is held by a wrapped Bun.spawnSync of a different cell than the one past its limit.
// That is a program running, which ends, and killing the kernel would kill honest work for another cell's lateness.
import { appendFileSync } from "node:fs";
import { isMainThread, Worker, workerData } from "node:worker_threads";
import { entryBytes } from "./journal.ts";

/** How long the loop must have been held, past the cell's limit, before the kernel is ended. */
export const GRACE_MS = 5_000;
const BEAT_MS = 200;

/** The shared cells: when the loop last turned, the earliest limit among running cells and whose it is, and the cell inside a spawnSync. */
const BEAT = 0, DEADLINE = 1, TASK = 2, SYNC = 3;
const num = (id: string) => Number(id.slice(1));

export type Watchdog = {
  /** `id` must be stopped by `deadline_ms`. */
  arm(id: string, deadline_ms: number): void;
  disarm(id: string): void;
  /** `id` is entering (or, null, has left) a synchronous spawn. */
  sync(id: string | null): void;
};

let active: Watchdog | undefined;
/** The running kernel's watchdog, if it has one (effects.ts marks spawnSync through it). */
export const watchdog = (): Watchdog | undefined => active;

/** Start watching this process. `journal`: where a cell's forced end is recorded. */
export function startWatchdog(o: { journal: string | null; grace_ms?: number }): Watchdog {
  const shared = new Float64Array(new SharedArrayBuffer(4 * 8));
  const limits = new Map<string, number>();
  const publish = () => {
    let first: [string, number] | undefined;
    for (const e of limits) if (!first || e[1] < first[1]) first = e;
    shared[DEADLINE] = first?.[1] ?? 0;
    shared[TASK] = first ? num(first[0]) : 0;
  };
  shared[BEAT] = Date.now();
  setInterval(() => { shared[BEAT] = Date.now(); }, BEAT_MS).unref();
  // Its start needs this thread's loop to turn a few times: a kernel is up long before a cell can hold it.
  const worker = new Worker(new URL(import.meta.url), { workerData: { shared, journal: o.journal, grace_ms: o.grace_ms ?? GRACE_MS, pid: process.pid } satisfies Watching });
  worker.unref();
  return (active = {
    arm(id, deadline_ms) { limits.set(id, deadline_ms); publish(); },
    disarm(id) { limits.delete(id); publish(); },
    sync(id) { shared[SYNC] = id ? num(id) : 0; },
  });
}

/** What the journal says of a cell the watchdog ended the kernel for. */
export const verdict = (id: string, held_ms: number): string =>
  `${id} stopped: it passed its limit while holding the kernel's only thread for ${(held_ms / 1000).toFixed(1)}s (synchronous code: a loop with no await, a long spawnSync), where no signal or cancel can reach it. The kernel was ended to end it; every cell's names went with it (rerun('tN') makes them again). Put an await in a long loop (await Bun.sleep(0) and signal().throwIfAborted()), or run the work as a process (sh, follow).`;

type Watching = { shared: Float64Array; journal: string | null; grace_ms: number; pid: number };

if (!isMainThread && workerData?.shared instanceof Float64Array) {
  const m = workerData as Watching;
  {
    setInterval(() => {
      const now = Date.now();
      const deadline = m.shared[DEADLINE]!, task = m.shared[TASK]!, sync = m.shared[SYNC]!;
      const held = now - m.shared[BEAT]!;
      if (!deadline || now < deadline + m.grace_ms || held < m.grace_ms || (sync !== 0 && sync !== task)) return;
      const id = `t${task}`;
      const why = verdict(id, held);
      try { if (m.journal) appendFileSync(m.journal, entryBytes({ e: "end", id, status: "stopped", result: why, ended_ms: now })); }
      catch { /* the kernel ends all the same: a held kernel with no record is still worse than a dead one */ }
      process.kill(m.pid, "SIGKILL");
    }, BEAT_MS);
  }
}

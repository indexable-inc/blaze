// Stopping a cell, for real: its signal, what it owns, and the kill (stop.ts has the rules and the words).
import { KILL_GRACE_MS, KILL_WAIT_MS, SETTLE_MS, stopText, type StopReport } from "./stop.ts";
import { currentTask, getTask, type Owned, type Task } from "./tasks.ts";

/** What a stopped cell's waits reject with. */
export class Stopped extends Error { override name = "Stopped"; }

/** The running cell's signal: aborted when the cell is stopped. Undefined outside a cell. */
export const signal = (): AbortSignal | undefined => currentTask()?.controller.signal;

/** `p`, or a rejection with the stop's reason the moment the running cell is stopped. Every wait a cell can be in goes through this. */
export function abortable<T>(p: PromiseLike<T> | T, s: AbortSignal | undefined = signal()): Promise<T> {
  if (!s) return Promise.resolve(p);
  if (s.aborted) {
    Promise.resolve(p).catch(() => {});
    return Promise.reject(s.reason);
  }
  return new Promise<T>((resolve, reject) => {
    const stop = () => reject(s.reason);
    s.addEventListener("abort", stop, { once: true });
    Promise.resolve(p).then(resolve, reject).finally(() => s.removeEventListener("abort", stop));
  });
}

/** Signal a whole process group (a child spawned `detached` leads its own). A group already gone is not an error. */
export function killGroup(pid: number, sig: NodeJS.Signals = "SIGTERM"): void {
  try { process.kill(-pid, sig); }
  catch (e) { if ((e as NodeJS.ErrnoException).code !== "ESRCH") throw e; }
}

/** The running cell owns `o` until it exits: stopping the cell kills it. */
export function own(o: Owned, t: Task | undefined = currentTask()): void {
  if (!t || t.status !== "running") return;
  // Started after the stop began: it goes the same way.
  if (t.controller.signal.aborted) o.kill(false);
  t.owned.add(o);
  void o.exited.then(() => t.owned.delete(o), () => t.owned.delete(o));
}

/** SIGTERM to a process group now, SIGKILL to what is left of it after KILL_GRACE_MS. */
export function terminateGroup(pid: number, exited: Promise<unknown>): void {
  killGroup(pid, "SIGTERM");
  const late = setTimeout(() => killGroup(pid, "SIGKILL"), KILL_GRACE_MS);
  void exited.then(() => clearTimeout(late), () => clearTimeout(late));
}

const pause = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const settledWithin = (ps: Promise<unknown>[], ms: number) => Promise.race([Promise.allSettled(ps), pause(ms)]);

/**
 * Stop `t` because `why`: abort its signal, kill what it owns, and say what happened (stop.ts). The same promise for
 * every caller; a cell that already ended is refused by name.
 */
export function stopTask(t: Task, why: string): Promise<string> {
  if (t.status !== "running") return Promise.reject(new Error(`${t.id} is not running (it is ${t.status}), so there is nothing to stop`));
  if (t.stopping) return t.stopping;
  return (t.stopping = (async () => {
    const owned = [...t.owned];
    t.controller.abort(new Stopped(`${t.id} was stopped: ${why}`));
    for (const o of owned) o.kill(false);
    await settledWithin(owned.map((o) => o.exited), KILL_GRACE_MS);
    const late = owned.filter((o) => t.owned.has(o));
    for (const o of late) o.kill(true);
    if (late.length) await settledWithin(late.map((o) => o.exited), KILL_WAIT_MS);
    let returned = false;
    void t.settled.then(() => { returned = true; });
    await Promise.race([t.settled, pause(SETTLE_MS)]);
    t.ignoring = !returned;
    if (t.ignoring) void t.settled.then(() => { t.ignoring = false; t.log(`[kernel] ${t.id}'s code has now returned`); });
    const report: StopReport = {
      id: t.id, why, ignoring: t.ignoring, left: [...t.detached],
      killed: owned.filter((o) => !t.owned.has(o)).map((o) => o.what), survived: owned.filter((o) => t.owned.has(o)).map((o) => o.what),
    };
    return stopText(report);
  })());
}

/** `cancel('t3')`: stop a running cell; resolves with what was killed and what was left. */
export function cancel(id: string | number, why?: string): Promise<string> {
  const t = getTask(id);
  const by = currentTask();
  if (by === t) return Promise.reject(new Error(`${t.id} cannot cancel itself: throw, or return`));
  return stopTask(t, why ?? (by ? `cancelled by ${by.id}` : "cancelled"));
}

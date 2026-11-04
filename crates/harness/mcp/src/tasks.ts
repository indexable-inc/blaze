// Every REPL cell is a task (t1, t2, ...). Tasks run concurrently; console output is captured per task
// through AsyncLocalStorage, so a backgrounded cell keeps its logs, including from timers it starts.
import { AsyncLocalStorage } from "node:async_hooks";
import { nuon } from "./nuon.ts";
import { table } from "./table.ts";

export type TaskStatus = "running" | "done" | "error";

export class Task {
  status: TaskStatus = "running";
  readonly started = new Date();
  ended?: Date;
  /** Raw value of the last expression (or the thrown error). */
  value: unknown;
  /** Formatted result: NUON, raw string, or the error stack. */
  result = "";
  readonly logs: string[] = [];
  readonly done: Promise<Task>;
  #resolve!: (t: Task) => void;
  constructor(readonly id: string, readonly code: string) {
    this.done = new Promise((r) => (this.#resolve = r));
  }
  get elapsed_ms() { return (this.ended ?? new Date()).getTime() - this.started.getTime(); }
  finish(ok: boolean, value: unknown, result: string) {
    Object.assign(this, { status: ok ? "done" : "error", value, result, ended: new Date() });
    this.#resolve(this);
  }
  /** Plain record for printing. */
  view() {
    const { id, status, code, started, elapsed_ms, logs, result } = this;
    return { id, status, code, started, elapsed_ms, logs, result };
  }
}

const registry = new Map<string, Task>();
let seq = 0;
const als = new AsyncLocalStorage<Task>();

export function newTask(code: string): Task {
  const t = new Task(`t${++seq}`, code);
  registry.set(t.id, t);
  return t;
}

/** Run fn with console output routed to the task's logs. */
export const inTask = <T>(t: Task, fn: () => T): T => als.run(t, fn);

let installed = false;
/** Patch console once: inside a task, log lines go to the task; outside, to the methods present at install time. */
export function installCapture() {
  if (installed) return;
  installed = true;
  for (const level of ["log", "info", "warn", "error", "debug"] as const) {
    const orig = console[level].bind(console);
    console[level] = (...args: unknown[]) => {
      const t = als.getStore();
      if (!t) return orig(...args);
      t.logs.push((level === "log" ? "" : `[${level}] `) + args.map((a) => (typeof a === "string" ? a : nuon(a, { maxRows: 100 }))).join(" "));
    };
  }
}

const getTask = (id: string | number): Task => {
  const t = registry.get(typeof id === "number" ? `t${id}` : id);
  if (!t) throw new Error(`no task ${id}; see tasks()`);
  return t;
};

const preview = (s: string, n: number) => { const one = s.replace(/\s+/g, " ").trim(); return one.length > n ? one.slice(0, n - 1) + "…" : one; };

/** All tasks, oldest first. */
export function tasks() {
  return table([...registry.values()].map((t) => ({
    id: t.id, status: t.status, code: preview(t.code, 60), started: t.started, elapsed_ms: t.elapsed_ms,
    logs: t.logs.length, result: preview(t.result, 80),
  })));
}

/** Snapshot of one task: status, full code, logs, formatted result. */
export function task(id: string | number) { return getTask(id).view(); }

/** Raw value of a finished task's last expression. */
export function taskValue(id: string | number) { return getTask(id).value; }

/** Wait for a task; on timeout returns the still-running snapshot instead of throwing. */
export async function wait(id: string | number, timeoutMs?: number) {
  const t = getTask(id);
  if (timeoutMs === undefined) await t.done;
  else { let timer: Timer | undefined; await Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, timeoutMs)))]); clearTimeout(timer); }
  return t.view();
}

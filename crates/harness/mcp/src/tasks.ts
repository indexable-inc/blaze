// Every REPL cell is a task (t1, t2, ...). Tasks run concurrently; console output is captured per task through
// AsyncLocalStorage, so a backgrounded cell keeps its logs, including from timers it starts. A task is a record: each
// change to it is appended to the kernel's journal (journal.ts), so the list, the logs and the results outlive the kernel.
import type { Docs } from "./docs.ts";
import { AsyncLocalStorage } from "node:async_hooks";
import { oneLine, present } from "./disclose.ts";
import { table } from "./table.ts";
import type { Effect } from "./effects.ts";
import type { TaskRecord } from "./journal.ts";

/** `stopped`: cancelled or past its limit (stop.ts). `lost`: it ran in an earlier kernel, which ended while it ran. */
export type TaskStatus = "running" | "done" | "error" | "stopped" | "lost";

/** Something a cell started that outlives a line of its code: a process group, a daemon run. Stopping the cell kills it. */
export type Owned = { what: string; kill(hard: boolean): void; exited: Promise<unknown> };

export class Task {
  status: TaskStatus = "running";
  started = new Date();
  ended?: Date;
  /** Raw value of the last expression (or the thrown error). Not kept across a kernel restart (`restored`). */
  value: unknown;
  /** The result as the agent reads it (disclose.ts), or the error's stack. */
  result = "";
  readonly logs: string[] = [];
  /** The tool call that ran it has replied while it still ran: the agent no longer waits on it, so what happens to it
   *  later (its end, a person's answer to its ask) is a notice to the agent (notices.ts). */
  returned = false;
  /** What the cell did outside the REPL, in order (effects.ts), and how many steps past the cap were not kept. */
  readonly effects: Effect[] = [];
  effectsDropped = 0;
  /** Bytes of shown values its steps carry (effects.ts bounds it). */
  valueBytes = 0;
  /** The UI it draws (ui/index.ts): the latest `{type, props, children}` tree (or a list of them) its markup rendered. */
  ui: unknown = undefined;
  /** Aborted when the cell is stopped; every typed call that waits honours its signal (cancel.ts). */
  readonly controller = new AbortController();
  /** What stopping the cell kills, and what it started detached on purpose (left running, named in the stop's report). */
  readonly owned = new Set<Owned>();
  readonly detached: string[] = [];
  /** Resolves when the cell's own code has returned (repl.ts sets it): a stopped cell's may be later than its end. */
  settled: Promise<unknown> = Promise.resolve();
  /** The stop in progress or done (stop.ts), once one began. */
  stopping?: Promise<string>;
  /** Stopped, and its own code has not returned: it ignores its signal. */
  ignoring = false;
  /** It ran in an earlier kernel: its record came from the journal, its value did not. */
  restored = false;
  readonly done: Promise<Task>;
  #resolve!: (t: Task) => void;
  /** How long the agent said it expects the cell to take, and why it is worth a long wait (patience.ts). */
  expect_ms?: number;
  why?: string;
  /** call: the agent's id for the tool call that ran this cell (Claude's tool_use id), when the client said it. */
  constructor(readonly id: string, readonly code: string, readonly call: string | null = null) {
    this.done = new Promise((r) => (this.#resolve = r));
  }
  get elapsed_ms() { return (this.ended ?? new Date()).getTime() - this.started.getTime(); }
  finish(status: Exclude<TaskStatus, "running">, value: unknown, result: string) {
    Object.assign(this, { status, value, result, ended: new Date() });
    this.#resolve(this);
    onChange(this, "end");
  }
  log(line: string) { this.logs.push(line); onChange(this, "log"); }
  /** Something about the task changed (a step began or ended): tell whoever watches (repl.ts posts it to harnessd). */
  touch() { onChange(this, "touch"); }
  /** The task as a plain record: what the journal keeps and `task()` prints. */
  record(): TaskRecord {
    const { id, status, code, call, expect_ms, why, logs, result } = this;
    return { id, status, code, call, started_ms: this.started.getTime(), ended_ms: this.ended?.getTime() ?? null, expect_ms: expect_ms ?? null, why: why ?? null, logs: [...logs], result };
  }
  view() {
    const { id, status, code, started, elapsed_ms, expect_ms, logs, result } = this;
    return { id, status, code, started, elapsed_ms, expect_ms, logs, result };
  }
}

export type Change = "start" | "log" | "touch" | "end";
const registry = new Map<string, Task>();
let seq = 0;
const als = new AsyncLocalStorage<Task>();
const watchers: ((t: Task, what: Change) => void)[] = [];
const onChange = (t: Task, what: Change) => { for (const w of watchers) w(t, what); };
/** Hear every task's changes (the REPL's posts to harnessd, the journal). */
export function watchTasks(fn: (t: Task, what: Change) => void) { watchers.push(fn); }

/** The task the running code belongs to, if any. */
export const currentTask = (): Task | undefined => als.getStore();

export function newTask(code: string, call: string | null = null, expect: { expect_ms?: number; why?: string } = {}): Task {
  const t = new Task(`t${++seq}`, code, call);
  t.expect_ms = expect.expect_ms;
  t.why = expect.why;
  registry.set(t.id, t);
  onChange(t, "start");
  return t;
}

/** Put back the tasks an earlier kernel journaled (journal-file.ts), ended as the journal says: ids go on from the last. */
export function restoreTasks(records: TaskRecord[]): void {
  for (const r of records) {
    const t = new Task(r.id, r.code, r.call);
    Object.assign(t, { restored: true, started: new Date(r.started_ms), expect_ms: r.expect_ms ?? undefined, why: r.why ?? undefined });
    t.logs.push(...r.logs);
    registry.set(t.id, t);
    t.finish(r.status as Exclude<TaskStatus, "running">, undefined, r.result);
    t.ended = new Date(r.ended_ms ?? r.started_ms);
    seq = Math.max(seq, Number(r.id.slice(1)) || 0);
  }
}

/** Forget every task (tests). */
export function resetTasks(): void { registry.clear(); seq = 0; }

/** The tool call id that ran task `id` (`t3`): null when the client did not say it, undefined when this kernel ran no such task. */
export const taskCall = (id: string): string | null | undefined => registry.get(id)?.call;

/** Every task, oldest first. */
export const allTasks = (): Task[] => [...registry.values()];

/** How many tasks this kernel has run: their ids are t1 to this. */
export const taskCount = (): number => seq;

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
      t.log((level === "log" ? "" : `[${level}] `) + args.map((a) => (typeof a === "string" ? a : present(a, { budget: 4 << 10 }).text)).join(" "));
    };
  }
}

export const getTask = (id: string | number): Task => {
  const t = registry.get(typeof id === "number" ? `t${id}` : id);
  if (!t) throw new Error(`no task ${id}: ${seq === 0 ? "no cell has run" : `cells are t1 to t${seq}`}; see tasks()`);
  return t;
};

/** All tasks, oldest first. */
export function tasks() {
  return table(allTasks().map((t) => ({
    id: t.id, status: t.ignoring ? "stopped (still running: ignores its signal)" : t.status, code: oneLine(t.code, 60), started: t.started, elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms,
    logs: t.logs.length, result: oneLine(t.result, 80),
  })));
}

/** Snapshot of one task: status, full code, logs, formatted result. */
export function task(id: string | number) { return getTask(id).view(); }

/** Raw value of a finished task's last expression. */
export function taskValue(id: string | number) {
  const t = getTask(id);
  if (t.restored) throw new Error(`${t.id} ran in an earlier kernel, which ended: its value is gone (its text is task('${t.id}').result). Run it again with rerun('${t.id}')`);
  return t.value;
}

/** What help() says about this module's calls (docs.ts). */
export const docs: Docs = {
  tasks: {
    area: "cells",
    sig: "tasks(): Table<{ id; status: running|done|error|stopped|lost; code; started; elapsed_ms; logs: count; result: preview }>",
    about: "every cell run so far, cells of an earlier kernel included (kept in its journal; one that was running when that kernel ended is lost).",
  },
  task: {
    area: "cells",
    sig: "task(id: 't3' | 3): { id; status; code; started; elapsed_ms; logs: string[]; result: string }",
    about: "snapshot of one cell; logs and result stay after it finishes.",
    example: "task('t3').logs.slice(-20)",
    throws: "Error naming the range of cells when there is no such task.",
  },
  taskValue: {
    area: "cells",
    sig: "taskValue(id): unknown",
    about: "raw value of a finished cell's last expression (or its thrown error).",
    throws: "Error naming the range of cells when there is no such task; Error for a cell an earlier kernel ran (its value is gone: rerun it).",
  },
};

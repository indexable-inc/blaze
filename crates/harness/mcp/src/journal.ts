// The task journal: every cell as a record that outlives its kernel. A task's start, each console line and its end are
// appended as they happen, as harness frames of deterministic CBOR (wire.ts, cbor.ts: the one encoding harness speaks),
// so after a kernel ends for any reason the next one for the same agent session (or, for a kernel run by hand, the same
// working directory) has the task list, the logs and the results back: `tasks()`, `task('t3')`, ids going on from the
// last. What is not kept: the values and the names cells bound, which are live objects of a process that is gone
// (handles, functions, sockets); `rerun('t3')` makes them again from the cell's code.
//
// This file is the pure half (entries to bytes, bytes to records; no IO): journal-file.ts reads and appends.
// An append is not fsynced: a kernel that dies loses nothing (the bytes are the OS's), a machine that dies may lose the
// tail, and a torn last frame is dropped on replay and counted, never read as a record.
import { decode, encode } from "./cbor.ts";
import { T_CBOR, frame, split } from "./wire.ts";

/** The journal's layout. A file of another layout is refused by name, never read as empty. */
export const LAYOUT = 1;
/** The most tasks a compaction keeps (the newest). */
export const KEEP = 200;
/** Characters of one task's console lines the journal keeps; past it the journal says how many lines it left out. */
export const LOG_MAX = 256 << 10;
/** Characters of a code or a result the journal keeps. */
export const TEXT_MAX = 64 << 10;

export type TaskRecord = {
  id: string; status: string; code: string; call: string | null; started_ms: number; ended_ms: number | null;
  expect_ms: number | null; why: string | null; logs: string[]; result: string;
};

export type Entry =
  | { e: "journal"; layout: number }
  /** A kernel began appending: `pid` says whether it still lives. */
  | { e: "kernel"; pid: number; repl: string; at_ms: number }
  | { e: "task"; task: TaskRecord }
  | { e: "log"; id: string; line: string }
  | { e: "end"; id: string; status: string; result: string; ended_ms: number };

const cut = (s: string) => (s.length > TEXT_MAX ? `${s.slice(0, TEXT_MAX)}… [${s.length - TEXT_MAX} more characters not journaled]` : s);

/** One entry as the bytes appended. */
export const entryBytes = (e: Entry): Uint8Array => frame(T_CBOR, encode(e));

/** A task's start as its entry (its logs and result so far included: a compaction writes whole records). */
export const taskEntry = (r: TaskRecord): Entry => ({ e: "task", task: { ...r, code: cut(r.code), result: cut(r.result) } });
export const endEntry = (r: TaskRecord): Entry => ({ e: "end", id: r.id, status: r.status, result: cut(r.result), ended_ms: r.ended_ms ?? r.started_ms });

export type Replay = {
  tasks: TaskRecord[];
  /** The last kernel that appended. */
  kernel: { pid: number; repl: string } | null;
  /** Bytes at the end that are not a whole frame (a write a crash cut). */
  torn: number;
  /** Why nothing was read: the file is not a journal of this layout. */
  refused: string | null;
};

/** The records a journal's bytes hold. Total: bad input gives `refused` or `torn`, never a throw. */
export function replay(bytes: Uint8Array): Replay {
  const out: Replay = { tasks: [], kernel: null, torn: 0, refused: null };
  const byId = new Map<string, TaskRecord & { logged: number }>();
  let rest = bytes;
  let first = true;
  while (rest.length) {
    let f: ReturnType<typeof split>;
    let e: Entry;
    try {
      f = split(rest);
      if (!f) { out.torn = rest.length; break; }
      e = decode(f.payload) as Entry;
    } catch { out.torn = rest.length; break; }
    rest = rest.subarray(f.size);
    if (first) {
      first = false;
      if (e.e !== "journal" || e.layout !== LAYOUT) return { ...out, refused: e.e === "journal" ? `it has layout ${e.layout}, and this kernel reads layout ${LAYOUT}` : "it does not begin as a task journal" };
      continue;
    }
    if (e.e === "kernel") out.kernel = { pid: e.pid, repl: e.repl };
    else if (e.e === "task") byId.set(e.task.id, { ...e.task, logs: [...e.task.logs], logged: e.task.logs.reduce((n, l) => n + l.length, 0) });
    else if (e.e === "log") { const t = byId.get(e.id); if (t) t.logs.push(e.line); }
    else if (e.e === "end") { const t = byId.get(e.id); if (t) Object.assign(t, { status: e.status, result: e.result, ended_ms: e.ended_ms }); }
  }
  out.tasks = [...byId.values()].map(({ logged: _logged, ...t }) => t);
  return out;
}

/** A journal holding exactly `tasks` (the newest KEEP), as a kernel `k` begins: what a restore rewrites the file to. */
export function compact(tasks: readonly TaskRecord[], k: { pid: number; repl: string; at_ms: number }): Uint8Array {
  const parts = [entryBytes({ e: "journal", layout: LAYOUT }), ...tasks.slice(-KEEP).map((t) => entryBytes(taskEntry(t))), entryBytes({ e: "kernel", ...k })];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
}

/** What a kernel appends for a task as it changes, with the bound on its console lines. */
export class Appender {
  #logged = new Map<string, number>();
  start(r: TaskRecord): Uint8Array { return entryBytes(taskEntry({ ...r, logs: [] })); }
  /** Null once the task's lines are past LOG_MAX (one last entry says so). */
  log(id: string, line: string): Uint8Array | null {
    const used = this.#logged.get(id) ?? 0;
    if (used > LOG_MAX) return null;
    this.#logged.set(id, used + line.length + 1);
    return entryBytes({ e: "log", id, line: used + line.length > LOG_MAX ? `# the journal keeps ${LOG_MAX} characters of a cell's console lines; later lines of ${id} are not in it` : cut(line) });
  }
  end(r: TaskRecord): Uint8Array { this.#logged.delete(r.id); return entryBytes(endEntry(r)); }
}

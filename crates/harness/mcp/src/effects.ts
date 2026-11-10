// A cell's effects: what it did to the world outside the REPL, as steps the app draws in place of the code ("Searching
// Google for …", then "Searched Google · 5 results" with the results under it).
//
// The kernel owns the exit points from a cell's JavaScript to the world, so it records them there, in one place: the
// running cell's effect log (Task.effects). Two kinds of exit point append to it:
//
// - The typed calls, each with its own words: rg, glob, ls, du, gitLog, follow and cargo.*, vfs, image, googleSearch, sh,
//   spawn, sql, skill (`step`/`effect` at their entry).
// - Thin wrappers over the globals a cell reaches the world through, with generic words: fetch, Bun.write, the reads of
//   Bun.file (text, json, bytes, arrayBuffer), Bun.spawn and Bun.spawnSync (installed once, globally, by `install`), and
//   the node:fs and node:fs/promises a cell imports (the REPL hands cells `fsFor(spec)` in place of the builtin).
//
// A step records its kind, its words while running (`doing`) and once done (`done`), what it touched (`target`), how it
// went (`ok`, `detail` such as "exit 0" or "5 results", `ms`) and, for steps whose result is data (a search, a query, a
// command's output), that result as the app shows it (shown.ts). A step taken inside another (the fetch googleSearch
// makes, the Bun.spawn of sh's local fallback) is not recorded again; code outside any cell (the MCP server itself)
// records nothing.
//
// What is not recorded, so a card never claims it is complete: node:child_process, Bun.$, sockets and WebSockets
// (Bun.connect, the harness browser's socket behind withHiddenPage; a cell's page.goto and browser.open are steps), node:fs's callback forms and its other functions (stat,
// readdir, streams), fs calls made by packages a cell imports (they get the real builtin), Bun.file's stream() and
// slices, and anything a native addon does. The harness's own requests to harnessd are not effects either.
import { AsyncLocalStorage } from "node:async_hooks";
import { currentTask, type Task } from "./tasks.ts";
import { brief, shown, type Shown, type ShownKind } from "./shown.ts";

/** `list`: a directory listing or file walk (ls, glob, du); `build`: a build, lint or test followed to a digest (follow,
 *  cargo.*); `wait`: waiting for another cell (wait), its value the value that cell gave; `skill`: a skill's instructions
 *  loaded (skills.ts), drawn as a quiet step and counted apart from file reads; `progress`: a cell saying how far along
 *  its own work is ([`progress`]). */
export type EffectKind = "exec" | "read" | "write" | "delete" | "net" | "search" | "list" | "build" | "spawn" | "sql" | "wait" | "skill" | "progress";

/** One step, as it is posted (the raw result is kept on the side, for telling a cell's value apart from its steps'). */
export type Effect = {
  n: number;
  kind: EffectKind;
  doing: string;
  done: string;
  target: string;
  status: "running" | "ok" | "error";
  started_ms: number;
  ms: number | null;
  detail: string;
  value: Shown | null;
};

/** The most steps a cell keeps; later ones are counted (Task.effectsDropped), not kept. */
export const MAX_EFFECTS = 200;
/** The bytes of shown values a cell's steps may carry in all; past it a step keeps its words and drops its value. */
const VALUE_BUDGET = 256 << 10;

const inside = new AsyncLocalStorage<true>();
const raws = new WeakMap<Task, unknown[]>();

/** The raw results of a task's steps (a cell whose value is one of them shows it once, under its step). */
export const rawsOf = (t: Task): unknown[] => raws.get(t) ?? [];

/** A running step; `end` it once. */
export class Step {
  constructor(readonly task: Task, readonly e: Effect) {}
  /** While it runs: new words for what it is doing now ("Checking blaze-core · 132/388") and/or a value to show so far. */
  update(u: { doing?: string; detail?: string; show?: unknown; kind?: ShownKind }) {
    const { task, e } = this;
    if (e.status !== "running") return;
    if (u.doing !== undefined) e.doing = u.doing;
    if (u.detail !== undefined) e.detail = u.detail;
    if (u.show !== undefined) e.value = shown(u.show, 16 << 10, u.kind);
    task.touch();
  }
  end(ok: boolean, detail = "", result?: unknown, show?: unknown, kind?: ShownKind) {
    const { task, e } = this;
    e.status = ok ? "ok" : "error";
    e.ms = Date.now() - e.started_ms;
    e.detail = detail;
    if (result !== undefined) (raws.get(task) ?? raws.set(task, []).get(task)!).push(result);
    if (show !== undefined && task.valueBytes < VALUE_BUDGET) {
      e.value = shown(show, Math.min(64 << 10, VALUE_BUDGET - task.valueBytes), kind);
      task.valueBytes += JSON.stringify(e.value).length;
    }
    task.touch();
  }
}

/** Begin a step in the running cell; null outside a cell, inside another step, or past MAX_EFFECTS. */
export function step(kind: EffectKind, doing: string, done: string, target: string): Step | null {
  const t = currentTask();
  if (!t || inside.getStore()) return null;
  if (t.effects.length >= MAX_EFFECTS) { t.effectsDropped++; t.touch(); return null; }
  const e: Effect = { n: t.effects.length, kind, doing, done, target, status: "running", started_ms: Date.now(), ms: null, detail: "", value: null };
  t.effects.push(e);
  t.touch();
  return new Step(t, e);
}

/** Run `fn` with nothing in it recorded as a step of its own (a typed call's own fetches and spawns). */
export const quiet = <T>(fn: () => T): T => inside.run(true, fn);

/** Run `fn` as one step: begun now, ended when it settles, with `after` saying its detail and the value shown. */
export async function effect<T>(
  kind: EffectKind, doing: string, done: string, target: string, fn: () => T | Promise<T>,
  after: (r: T) => { ok?: boolean; detail?: string; show?: unknown; kind?: ShownKind } = () => ({}),
): Promise<T> {
  const s = step(kind, doing, done, target);
  if (!s) return fn();
  try {
    const r = await quiet(fn);
    const a = after(r);
    s.end(a.ok ?? true, a.detail ?? "", r, a.show, a.kind);
    return r;
  } catch (err) {
    // Its detail is what the person reads: one plain sentence (the model gets the whole error from the cell).
    s.end(false, brief(err));
    throw err;
  }
}

const progressing = new WeakMap<Task, Step>();

/** Say how far along the running cell's work is: `done` of `total` (of anything: files, rows, pages), and in a few words
 *  what (`what`). The app's work bar fills to that share and says about how long is left, from how fast it has gone.
 *  Call it as the work moves; at `done >= total` the step ends. Outside a cell it does nothing. */
export function progress(done: number, total: number, what = "Working"): void {
  const t = currentTask();
  if (!t || !(total > 0)) return;
  const at = Math.max(0, Math.min(Math.round(done), Math.round(total)));
  let s = progressing.get(t);
  if (!s || s.e.status !== "running") {
    const made = step("progress", what, what, "");
    if (!made) return;
    progressing.set(t, (s = made));
  }
  s.update({ doing: `${what} \u{b7} ${at}/${Math.round(total)}` });
  if (at >= Math.round(total)) s.end(true, `${at}/${Math.round(total)}`);
}

export const progressDocs = {
  progress:
    "progress(done, total, what? = 'Working'): void — say how far along this cell's own long work is (files, rows, pages); the person's work bar fills to that share and says about how long is left. Call it as the work moves: `for (const [i, f] of files.entries()) { progress(i, files.length, 'Converting'); await convert(f) }`.",
};

/** Bytes for a step's detail: "812 B", "3.2 KB", "1.4 MB". */
export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1 << 20)).toFixed(1)} MB`;
}

/** A command or text as one short line, for a step's words. */
export function oneLine(s: string, n = 80): string {
  const one = s.replace(/\s+/g, " ").trim();
  return one.length > n ? one.slice(0, n - 1) + "…" : one;
}

/** A path as a step names it: relative to the working directory when under it. */
const pathOf = (p: unknown): string => {
  const s = typeof p === "string" ? p : p instanceof URL ? p.pathname : typeof p === "number" ? `fd ${p}` : (p as { name?: string })?.name ?? String(p);
  const cwd = process.cwd() + "/";
  return s.startsWith(cwd) ? s.slice(cwd.length) : s;
};
const sizeOf = (d: unknown): number | null =>
  typeof d === "string" ? Buffer.byteLength(d) : ArrayBuffer.isView(d) ? d.byteLength : d instanceof ArrayBuffer ? d.byteLength : null;

let installed = false;
/** Wrap the globals once: fetch, Bun.write, Bun.file's reads, Bun.spawn, Bun.spawnSync. */
export function install() {
  if (installed) return;
  installed = true;

  const realFetch = globalThis.fetch;
  const fetchStep: typeof fetch = Object.assign(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = input instanceof Request ? input.url : String(input);
    let host = url;
    try { host = new URL(url).host || url; } catch {}
    return effect("net", `Fetching ${host}`, `Fetched ${host}`, url, () => realFetch(input, init), (r: Response) => {
      const len = r.headers.get("content-length");
      return { ok: r.ok, detail: `${r.status}${len ? ` · ${bytes(Number(len))}` : ""}` };
    });
  }, realFetch);
  globalThis.fetch = fetchStep;

  const realWrite = Bun.write;
  Bun.write = ((dest: unknown, data: unknown, ...rest: unknown[]) => {
    const p = pathOf(dest);
    return effect("write", `Writing ${p}`, `Wrote ${p}`, p, () => (realWrite as (...a: unknown[]) => Promise<number>)(dest, data, ...rest), (n: number) => ({ detail: bytes(n) }));
  }) as typeof Bun.write;

  const realFile = Bun.file;
  Bun.file = ((...args: Parameters<typeof Bun.file>) => {
    const f = realFile(...args);
    const p = pathOf(f.name ?? args[0]);
    for (const m of ["text", "json", "bytes", "arrayBuffer"] as const) {
      const real = f[m].bind(f) as () => Promise<unknown>;
      Object.defineProperty(f, m, {
        value: () => effect("read", `Reading ${p}`, `Read ${p}`, p, real, () => ({ detail: bytes(f.size) })),
        configurable: true,
      });
    }
    return f;
  }) as typeof Bun.file;

  const argvOf = (a: unknown): string => {
    const cmd = Array.isArray(a) ? a : (a as { cmd?: string[] })?.cmd ?? [];
    return oneLine(cmd.join(" "));
  };
  const realSpawn = Bun.spawn;
  Bun.spawn = ((...args: unknown[]) => {
    const cmd = argvOf(args[0]);
    const s = step("exec", `Running ${cmd}`, `Ran ${cmd}`, cmd);
    const sub = (s ? quiet(() => (realSpawn as (...a: unknown[]) => Bun.Subprocess)(...args)) : (realSpawn as (...a: unknown[]) => Bun.Subprocess)(...args));
    s && sub.exited.then((code) => s.end(code === 0, `exit ${code}`), (e) => s.end(false, String(e)));
    return sub;
  }) as typeof Bun.spawn;
  const realSpawnSync = Bun.spawnSync;
  Bun.spawnSync = ((...args: unknown[]) => {
    const cmd = argvOf(args[0]);
    const s = step("exec", `Running ${cmd}`, `Ran ${cmd}`, cmd);
    const r = (realSpawnSync as (...a: unknown[]) => Bun.SyncSubprocess)(...args);
    s?.end(r.exitCode === 0, `exit ${r.exitCode}`);
    return r;
  }) as typeof Bun.spawnSync;
}

type Fn = (...a: unknown[]) => unknown;
/** One fs function wrapped as a step: `doing`/`done` its words ("Reading", "Read"), `size` the bytes it moved, if known. */
function fsStep(real: Fn, kind: EffectKind, doing: string, done: string, size: (a: unknown[], r: unknown) => number | null): Fn {
  const wrapped = (...a: unknown[]) => {
    const p = pathOf(a[0]);
    const s = step(kind, `${doing} ${p}`, `${done} ${p}`, p);
    if (!s) return real(...a);
    const finish = (r: unknown) => { const n = size(a, r); s.end(true, n == null ? "" : bytes(n)); return r; };
    const fail = (e: unknown) => { s.end(false, brief(e)); throw e; };
    try {
      const r = quiet(() => real(...a));
      return r instanceof Promise ? r.then(finish, fail) : finish(r);
    } catch (e) {
      return fail(e);
    }
  };
  return wrapped;
}

const readSize = (_a: unknown[], r: unknown) => sizeOf(r);
const writeSize = (a: unknown[]) => sizeOf(a[1]);
const none = () => null;
const SYNC: Record<string, [EffectKind, string, string, (a: unknown[], r: unknown) => number | null]> = {
  readFileSync: ["read", "Reading", "Read", readSize],
  writeFileSync: ["write", "Writing", "Wrote", writeSize],
  appendFileSync: ["write", "Appending to", "Appended to", writeSize],
  mkdirSync: ["write", "Creating", "Created", none],
  copyFileSync: ["write", "Copying", "Copied", none],
  cpSync: ["write", "Copying", "Copied", none],
  renameSync: ["write", "Moving", "Moved", none],
  rmSync: ["delete", "Deleting", "Deleted", none],
  unlinkSync: ["delete", "Deleting", "Deleted", none],
  rmdirSync: ["delete", "Deleting", "Deleted", none],
};
/** The promise functions, named as fs/promises names them (their Sync twins' words). */
const ASYNC = Object.fromEntries(Object.entries(SYNC).map(([k, v]) => [k.replace(/Sync$/, ""), v]));

let fsMod: Record<string, unknown> | undefined;
let fspMod: Record<string, unknown> | undefined;
/** What a cell gets for `import ... from spec`: node:fs and node:fs/promises with their reads, writes and deletes as
 *  steps; undefined for every other module (the real one). */
export async function fsFor(spec: string): Promise<Record<string, unknown> | undefined> {
  const name = spec.replace(/^node:/, "");
  if (name !== "fs" && name !== "fs/promises") return undefined;
  const wrap = (real: Record<string, unknown>, table: typeof SYNC) => {
    const out: Record<string, unknown> = { ...real };
    for (const [k, [kind, doing, done, size]] of Object.entries(table)) {
      if (typeof real[k] === "function") out[k] = fsStep((real[k] as Fn).bind(real), kind, doing, done, size);
    }
    return out;
  };
  const real = await import("node:fs") as unknown as Record<string, unknown>;
  const realP = await import("node:fs/promises") as unknown as Record<string, unknown>;
  fspMod ??= (() => { const m = wrap(realP, ASYNC); m.default = m; return m; })();
  fsMod ??= (() => { const m = wrap(real, SYNC); m.promises = fspMod; m.default = m; return m; })();
  return name === "fs" ? fsMod : fspMod;
}

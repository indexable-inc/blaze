// The object API: every agent PTY (s7), process run (r12) and run stage (r12.0) is an object with an
// id, a name, a parent, append-only streams (read here straight from ~/.harness, or via sql) and RPC
// through the daemon's one `rpc {id, method, args}` op (see crates/harness/harness/src/object.rs).
import { checkShell, refusal } from "./shell-check.ts";
import { existsSync, readFileSync } from "node:fs";
import { req } from "./daemon.ts";
import { sql } from "./sql.ts";
import { table, type Table } from "./table.ts";
import { effect, oneLine } from "./effects.ts";
import { messageRows, readLog, type Event, type Msg } from "./wire.ts";

export type Kind = "claude" | "codex" | "cursor" | "shell" | "proc";
export type ObjInfo = {
  id: string; kind: Kind | "stage"; name: string | null; parent: string | null; status: string | null;
  exit_code: number | null; signal?: string | null; cwd?: string | null; cmd?: string; pid?: number | null;
  created_ms?: number | null; ended_ms?: number | null; title?: string;
};
export type SpawnOpts = {
  name?: string; prompt?: string; cmd?: string; cwd?: string; env?: Record<string, string>; args?: string[];
  /** Agents: the model and reasoning effort it runs on. A sub agent (one with a parent) must give a name, a model and,
   *  when that model lists efforts, the effort (skill: agents). Never in args (--model/--effort there are refused). */
  model?: string; effort?: string;
  /** Parent object (default: this agent; procs: the run this REPL runs in, else this agent). null for none. */
  parent?: string | Obj | null;
  /** proc: initial stdin text; the pipe stays open for .stdin() unless liveStdin: false. */
  stdin?: string; liveStdin?: boolean; cols?: number; rows?: number;
  /** proc: why a command that only reads files or the tree needs the shell (else it is refused, shell-check.ts). */
  why?: string;
};
export type { Msg };
export type SliceOpts = { from?: number; to?: number; tail?: number; new?: boolean; cursor?: string };
export type WaitOpts = { status?: string; match?: string | RegExp; timeoutMs?: number };

const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
const AGENTS = new Set(["claude", "codex", "cursor"]);
const selfSession = () => (process.env.HARNESS_SESSION ? `s${process.env.HARNESS_SESSION}` : null);
const read = (p: string) => (existsSync(p) ? readFileSync(p, "utf8") : "");
const refId = (r: string | Obj) => (typeof r === "string" ? r : r.id);

function slice(text: string, o: SliceOpts, cursors: Map<string, number>, key: string): string {
  if (o.new) {
    const k = `${key}:${o.cursor ?? "default"}`;
    const at = cursors.get(k) ?? 0;
    cursors.set(k, text.length);
    text = text.slice(at);
  }
  if (o.from === undefined && o.to === undefined && o.tail === undefined) return text;
  let ls = text.split("\n");
  if (ls.at(-1) === "") ls.pop();
  if (o.from !== undefined || o.to !== undefined) ls = ls.slice((o.from ?? 1) - 1, o.to);
  if (o.tail !== undefined) ls = ls.slice(-o.tail);
  return ls.join("\n");
}

export class Obj {
  id: string;
  kind: ObjInfo["kind"];
  name: string | null;
  parent: string | null;
  status: string | null;
  exitCode: number | null;
  #info: ObjInfo;
  /** messages rows before the turn reply() waits for. */
  #turnBase = -1;
  #cursors = new Map<string, number>();

  constructor(info: ObjInfo) {
    this.id = info.id; this.kind = info.kind; this.name = info.name; this.parent = info.parent;
    this.status = info.status; this.exitCode = info.exit_code;
    this.#info = info;
  }
  #set(info: ObjInfo) {
    this.#info = info;
    Object.assign(this, { kind: info.kind, name: info.name, parent: info.parent, status: info.status, exitCode: info.exit_code });
  }
  get info(): ObjInfo { return this.#info; }
  get isAgent() { return AGENTS.has(this.kind); }
  get running() { return this.status === "running"; }

  /** Re-read status/exitCode from the daemon. */
  async refresh(): Promise<this> { this.#set(await req<ObjInfo>({ op: "object", id: this.id })); return this; }
  /** Generic RPC: obj.send('resize', {cols, rows}). */
  send<T = Record<string, unknown>>(method: string, args: Record<string, unknown> = {}): Promise<T> {
    return req<T>({ op: "rpc", id: this.id, method, args });
  }

  // ---- input
  /** Agents: type a message and submit it (the daemon's per-kind injection). reply() then waits for its answer. */
  async prompt(text: string, opts: { submit?: boolean } = {}): Promise<this> {
    if (!this.isAgent && this.kind !== "shell") throw new Error(`${this.id} is a ${this.kind}; prompt() is for agents (procs: stdin())`);
    this.#turnBase = this.#messages().length;
    await this.send("prompt", { text, submit: opts.submit ?? true });
    return this;
  }
  /** Raw bytes: PTY keystrokes for sessions, stdin for procs. */
  async input(data: string): Promise<this> { await this.send(this.kind === "proc" ? "stdin" : "input", { data }); return this; }
  async stdin(text: string): Promise<this> { await this.send("stdin", { data: text }); return this; }
  async closeStdin(): Promise<this> { await this.send("close_stdin"); return this; }
  /** Agents: Esc (stop the turn); shell: Ctrl-C; procs: SIGINT to the process group. */
  async interrupt(): Promise<this> { await this.send("interrupt"); return this; }
  async signal(sig: string | number): Promise<this> { await this.send("signal", { sig }); return this; }
  /** Sessions: kill the process; procs: SIGTERM the group, SIGKILL after 3s. */
  async kill(): Promise<this> { await this.send("kill"); return this; }
  async resize(cols: number, rows: number): Promise<this> { await this.send("resize", { cols, rows }); return this; }
  /** Sessions: name it (the app shows it in place of its name; the daemon keeps it); "" gives it its automatic name again. */
  async rename(title: string): Promise<this> { await this.send("rename", { title }); return this; }

  // ---- reading
  /** The rendered terminal screen (sessions). */
  async screen(): Promise<string> { return (await this.send<{ text: string }>("screen")).text; }

  #runDir() { return `${HOME()}/runs/${this.id.split(".")[0]}`; }
  #file(stream: "stdout" | "stderr" | "pty"): string {
    if (this.kind === "proc") {
      const d = this.#runDir();
      if (stream === "stderr") return read(`${d}/final.err`);
      const m = existsSync(`${d}/meta.json`) ? JSON.parse(read(`${d}/meta.json`)) : { pipelines: [] };
      return (m.pipelines as { stages: number[] }[]).map((p) => read(`${d}/s${p.stages.at(-1)}.out`)).join("");
    }
    if (this.kind === "stage") return read(`${this.#runDir()}/s${this.id.split(".")[1]}.${stream === "stderr" ? "err" : "out"}`);
    return stream === "stderr" ? "" : read(`${HOME()}/objects/${this.id}/pty`);
  }
  /** Full stored stdout (procs: what the caller sees, the last stage of each pipeline; sessions: the PTY). */
  get stdout(): string { return this.#file("stdout"); }
  get stderr(): string { return this.#file("stderr"); }
  /** Raw PTY bytes as text, escape codes stripped. */
  get pty(): string { return Bun.stripANSI(this.#file("pty")); }
  /** One stream sliced like out(): { from, to (1-based lines), tail, new: true (since the last read with that cursor) }. */
  read(stream: "stdout" | "stderr" | "pty" = "stdout", opts: SliceOpts = {}): string {
    return slice(stream === "pty" ? this.pty : this.#file(stream), opts, this.#cursors, stream);
  }

  #messages(): Msg[] {
    // A chat session's rows are projected from its log; a TUI session's are its transcript's `messages` file.
    const chat = `${HOME()}/objects/${this.id}/chat`;
    if (existsSync(chat)) {
      const log = readLog(new Uint8Array(readFileSync(chat)));
      if ("notALog" in log) throw new Error(`${this.id}: ${chat} is ${log.notALog}`);
      return messageRows(this.id, log.records.flatMap((r): Event[] => (r.event ? [r.event] : []))).map(({ object: _, ...m }) => m);
    }
    const out: Msg[] = [];
    for (const line of read(`${HOME()}/objects/${this.id}/messages`).split("\n")) {
      if (!line) continue;
      try {
        const r = JSON.parse(line);
        out.push({ n: out.length + 1, role: r.role, kind: r.kind, tool: r.tool ?? null, text: r.text ?? "", ts_ms: r.ts_ms ?? null });
      } catch { /* partial line */ }
    }
    return out;
  }
  /** The agent's conversation (kind text | thinking | tool_call | tool_result | turn_end), from its transcript. */
  get messages(): Table<Msg> { return table(this.#messages()); }

  /** Lines of this object's streams (procs include their stages) as a Table, from the store (sql `lines`). */
  async events(opts: { stream?: string; since?: number; tail?: number } = {}): Promise<Table<{ object: string; stream: string; line_no: number; text: string }>> {
    const rows = await sql<{ object: string; stream: string; line_no: number; text: string }>`
      select object, stream, line_no, text from lines
      where (object = ${this.id} or object like ${this.id + ".%"}) and (${opts.stream ?? null}::varchar is null or stream = ${opts.stream ?? null})
        and line_no > ${opts.since ?? 0}
      order by object, stream, line_no`;
    return opts.tail === undefined ? rows : table(rows.slice(-opts.tail));
  }
  children(): Promise<Table<ObjRow>> { return objects({ parent: this.id, all: true }); }

  // ---- waiting
  /**
   * Wait for a condition: no argument = until it stops running; { status } until status equals it; { match }
   * until stdout (procs) or the screen (sessions) matches; or a predicate over the refreshed handle.
   */
  async wait(cond?: ((o: Obj) => boolean | Promise<boolean>) | WaitOpts): Promise<this> {
    const o: WaitOpts = typeof cond === "function" || !cond ? {} : cond;
    const deadline = Date.now() + (o.timeoutMs ?? 3_600_000);
    if (!cond && this.kind === "proc") {
      while (Date.now() < deadline) {
        this.#set(await this.send<ObjInfo>("wait", { timeout_ms: Math.min(30_000, Math.max(1, deadline - Date.now())) }));
        if (!this.running) return this;
      }
      throw new Error(`${this.id} still running after ${o.timeoutMs}ms`);
    }
    for (;;) {
      await this.refresh();
      let ok: boolean;
      if (typeof cond === "function") ok = await cond(this);
      else if (o.match !== undefined) {
        const text = this.kind === "proc" || this.kind === "stage" ? this.stdout : await this.screen();
        ok = typeof o.match === "string" ? text.includes(o.match) : o.match.test(text);
      } else if (o.status !== undefined) ok = this.status === o.status;
      else ok = !this.running;
      if (ok) return this;
      if (Date.now() > deadline) throw new Error(`timed out after ${o.timeoutMs}ms waiting on ${this.id}`);
      await Bun.sleep(300);
    }
  }

  /**
   * Agents: wait until the agent finishes the turn started by the last prompt() (or the spawn prompt, or the
   * turn in flight) and return its last assistant message. Driven by the messages stream's turn_end marker.
   */
  async reply(timeoutMs = 600_000): Promise<string> {
    if (this.kind !== "claude" && this.kind !== "codex") throw new Error(`reply() reads the transcript, which harness mirrors for claude and codex only (${this.id} is ${this.kind}); use wait({match}) / screen()`);
    const deadline = Date.now() + timeoutMs;
    if (this.#turnBase < 0) this.#turnBase = this.#inFlight();
    for (;;) {
      await this.send("sync").catch(() => {});
      const after = this.#messages().slice(this.#turnBase);
      const u = after.findIndex((r) => r.role === "user" && r.kind === "text");
      const e = u < 0 ? -1 : after.findIndex((r, i) => i > u && r.kind === "turn_end");
      if (e >= 0) {
        const last = after.slice(u, e).findLast((r) => r.role === "assistant" && r.kind === "text" && r.text.trim());
        this.#turnBase += e + 1;
        return last?.text ?? after[e]!.text;
      }
      if (Date.now() > deadline) throw new Error(`${this.id}: no reply within ${timeoutMs}ms; see .messages / screen()`);
      if ((await this.refresh()).status !== "running") throw new Error(`${this.id} exited (${this.exitCode}) before replying`);
      await Bun.sleep(400);
    }
  }
  /** Where a fresh handle's reply() starts: the last user turn if it hasn't ended, else the end. */
  #inFlight(): number {
    const ms = this.#messages();
    const u = ms.findLastIndex((r) => r.role === "user" && r.kind === "text");
    return u >= 0 && !ms.slice(u).some((r) => r.kind === "turn_end") ? u : ms.length;
  }
  /** spawn() sets where reply() starts (0 for a new agent with a prompt). */
  _base(n: number) { this.#turnBase = n; return this; }
  _messageCount() { return this.#messages().length; }

  toJSON() { const { id, kind, name, parent, status, exitCode } = this; return { id, kind, name, parent, status, exitCode }; }
  [Bun.inspect.custom]() { return this.toJSON(); }
}

/**
 * Start an object, or return the running one with this name (get-or-create, atomic in the daemon).
 * kind 'proc' runs opts.cmd like sh() (every stage stored) with a live stdin; agents get opts.prompt.
 */
export function spawn(kind: Kind, opts: SpawnOpts = {}): Promise<Obj> {
  const what = kind === "proc" ? oneLine(opts.cmd ?? "") : AGENTS.has(kind) ? `${kind} agent${opts.name ? ` ${opts.name}` : ""}` : `${kind}${opts.name ? ` ${opts.name}` : ""}`;
  return effect("spawn", `Starting ${what}`, `Started ${what}`, opts.name ?? kind, () => spawnObject(kind, opts), (o) => ({ detail: o.id }));
}

async function spawnObject(kind: Kind, opts: SpawnOpts): Promise<Obj> {
  const env: Record<string, string> = {};
  let parent: string | null = opts.parent === undefined ? null : opts.parent === null ? null : refId(opts.parent);
  let body: Record<string, unknown>;
  if (kind === "proc") {
    if (!opts.cmd) throw new Error("spawn('proc') needs { cmd }");
    const verdict = checkShell(opts.cmd, opts.why);
    if (!verdict.ok) throw refusal(verdict.message);
    for (const [k, v] of Object.entries({ ...process.env, ...opts.env })) if (v !== undefined) env[k] = v;
    if (opts.parent === undefined) parent = process.env.HARNESS_RUN ?? selfSession();
    body = {
      cmd: opts.cmd, cwd: opts.cwd ?? process.cwd(), env, clear_env: true, stdin: opts.stdin ?? null, live_stdin: opts.liveStdin ?? true,
      session: process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null,
    };
  } else {
    if (opts.parent === undefined) parent = selfSession();
    if (parent !== null && AGENTS.has(kind)) {
      // effort: the daemon requires it when the model's listed entry takes efforts (it knows the lists).
      const missing = [!opts.name && "name", !opts.model && "model"].filter(Boolean);
      if (missing.length) throw new Error(`a ${kind} sub agent needs an explicit ${missing.join(", ")}: spawn('${kind}', { name, model, effort, prompt })`);
    }
    body = {
      args: opts.args ?? [], cwd: opts.cwd ?? process.cwd(), env: opts.env ?? {}, prompt: opts.prompt ?? null, cols: opts.cols ?? 140, rows: opts.rows ?? 45,
      model: opts.model ?? null, effort: opts.effort ?? null,
    };
  }
  const r = await req<{ id: string; existing: boolean }>({ op: "spawn_object", kind, name: opts.name ?? null, parent, ...body });
  const o = await obj(r.id);
  if (kind !== "proc") {
    if (!r.existing) o._base(opts.prompt ? 0 : -1);
    else if (opts.prompt) await o.prompt(opts.prompt);
  } else if (r.existing && opts.stdin) await o.stdin(opts.stdin);
  return o;
}

/** A handle on any object by id ('s7', 'r12', 'r12.0') or name (the running holder, else the latest). */
export async function obj(ref: string | Obj): Promise<Obj> {
  if (ref instanceof Obj) return ref.refresh();
  return new Obj(await req<ObjInfo>({ op: "object", id: ref }));
}

export type ObjRow = { id: string; kind: string; name: string | null; parent: string | null; status: string | null; exit_code: number | null; cmd: string | null; cwd: string | null; created: Date | null; ended_ms: number | null };
export type ObjFilter = { kind?: string; parent?: string | Obj; status?: string; name?: string; live?: boolean; all?: boolean; limit?: number };

/** Every object, live and past (from the index the daemon keeps current), newest first. Stages and REPL cells only with all: true or kind. */
export async function objects(f: ObjFilter = {}): Promise<Table<ObjRow>> {
  const parent = f.parent === undefined ? null : refId(f.parent);
  const status = f.live ? "running" : f.status ?? null;
  const rows = await sql<Omit<ObjRow, "created"> & { created_ms: number | null }>`
    select id, kind, name, parent, status, exit_code, case when kind in ('proc', 'stage') then args->>0 end as cmd, cwd, created_ms, ended_ms
    from objects
    where (${f.kind ?? null}::varchar is null or kind = ${f.kind ?? null})
      and (${f.kind ?? null}::varchar is not null or ${f.all ?? false} or kind not in ('stage', 'repl'))
      and (${parent}::varchar is null or parent = ${parent})
      and (${status}::varchar is null or status = ${status})
      and (${f.name ?? null}::varchar is null or name = ${f.name ?? null})
    order by created_ms desc nulls last limit ${f.limit ?? 200}`;
  return table(rows.map((r) => ({
    id: r.id, kind: r.kind, name: r.name, parent: r.parent, status: r.status, exit_code: r.exit_code,
    cmd: r.cmd && r.cmd.length > 80 ? r.cmd.slice(0, 79) + "…" : r.cmd, cwd: r.cwd,
    created: r.created_ms ? new Date(r.created_ms) : null, ended_ms: r.ended_ms,
  })));
}

/** Objects as a tree (depth-first, `depth` + indented id): under root, else every root; me marks this agent. */
export async function tree(root?: string | Obj, f: ObjFilter = {}): Promise<Table<{ depth: number; id: string; kind: string; name: string | null; status: string | null; exit_code: number | null; cmd: string | null; me: boolean }>> {
  const all = await objects({ limit: 1000, ...f });
  const ids = new Set(all.map((o) => o.id));
  const kids = new Map<string | null, ObjRow[]>();
  for (const o of [...all].reverse()) {
    const p = o.parent && ids.has(o.parent) ? o.parent : null;
    (kids.get(p) ?? kids.set(p, []).get(p)!).push(o);
  }
  const me = selfSession();
  const out: { depth: number; id: string; kind: string; name: string | null; status: string | null; exit_code: number | null; cmd: string | null; me: boolean }[] = [];
  const walk = (o: ObjRow, depth: number) => {
    out.push({ depth, id: "  ".repeat(depth) + o.id, kind: o.kind, name: o.name, status: o.status, exit_code: o.exit_code, cmd: o.cmd, me: o.id === me });
    for (const k of kids.get(o.id) ?? []) walk(k, depth + 1);
  };
  const r = root === undefined ? null : typeof root === "string" && !/^[sr]\d/.test(root) ? (await obj(root)).id : refId(root);
  const start = r ? all.filter((o) => o.id === r) : kids.get(null) ?? [];
  for (const o of start) walk(o, 0);
  return table(out);
}

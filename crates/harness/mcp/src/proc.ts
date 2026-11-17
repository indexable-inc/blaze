// Process table for subprocesses started from the REPL: sh() spawns and returns a handle at once;
// stdout/stderr are kept in memory (last maxBytes per stream, older lines dropped but still numbered).
import type { Docs } from "./docs.ts";
import { table, type Table } from "./table.ts";
import { req, daemonPresent, DaemonUnreachable, SOCK } from "./daemon.ts";
import { abortable, killGroup, own } from "./cancel.ts";
import { currentTask } from "./tasks.ts";
import { REPL_ID, session as selfSession } from "./self.ts";
import { quiet, step } from "./effects.ts";
import { oneLine } from "./disclose.ts";
import { checkShell, refusal } from "./shell-rule.ts";

type Stream = "stdout" | "stderr";

class Buf {
  text = "";
  /** Chars and lines dropped from the front once the buffer passed max. */
  droppedChars = 0;
  droppedLines = 0;
  bytes = 0;
  readonly cursors = new Map<string, number>();
  constructor(readonly max: number) {}
  get truncated() { return this.droppedChars > 0; }
  get end() { return this.droppedChars + this.text.length; }
  push(chunk: string, bytes: number) {
    this.bytes += bytes;
    this.text += chunk;
    if (this.text.length <= this.max) return;
    let cut = this.text.indexOf("\n", this.text.length - this.max);
    cut = cut < 0 ? this.text.length - this.max : cut + 1;
    const gone = this.text.slice(0, cut);
    this.droppedLines += gone.split("\n").length - 1;
    this.droppedChars += cut;
    this.text = this.text.slice(cut);
  }
}

export type ShOpts = {
  cwd?: string; env?: Record<string, string>; stdin?: string; maxBytes?: number; timeoutMs?: number;
  /** Spawn here with Bun.spawn instead of through harnessd (no stored stages). */
  local?: boolean;
  /** Keep it running when the cell that started it is stopped (a server, a watcher). Only through harnessd, which holds it as a run every agent sees in objects(). */
  detach?: boolean;
  /** Why a command that only reads files or the tree needs the shell; without it such a command is refused (shell-rule.ts). */
  why?: string;
};
export type ShResult = { id: string; run: string | null; pid: number; exit_code: number | null; signal: string | null; stdout: string; stderr: string };

const shq = (a: string) => (/^[\w@%+=:,./-]+$/.test(a) ? a : `'${a.replaceAll("'", `'\\''`)}'`);
type ReadResp = { data: string; offset: number; next_offset: number; size: number; eof: boolean; done: boolean };

export class Proc {
  readonly started = new Date();
  ended?: Date;
  exit_code: number | null = null;
  signal: string | null = null;
  pid = 0;
  /** harnessd run id ('r12') when the command went through the daemon; every stage's output is stored there. */
  run: string | null = null;
  backend: "daemon" | "local" = "local";
  readonly out: Record<Stream, Buf>;
  readonly exited: Promise<ShResult>;
  /** Resolves once the backend is known (run id set for daemon runs). */
  readonly started_on: Promise<void>;
  #ready!: () => void;
  #sub?: Bun.Subprocess<"ignore" | Blob, "pipe", "pipe">;
  #killed?: NodeJS.Signals;
  constructor(readonly id: string, readonly cmd: string, readonly cwd: string, argv: string[], readonly opts: ShOpts) {
    const max = opts.maxBytes ?? 8 << 20;
    this.out = { stdout: new Buf(max), stderr: new Buf(max) };
    this.started_on = new Promise((r) => (this.#ready = r));
    this.exited = !opts.local && daemonPresent()
      ? this.#daemon().catch((e) => {
        if (e instanceof DaemonUnreachable && !opts.detach) return this.#local(argv);
        throw e;
      })
      : this.#local(argv);
  }

  async #daemon(): Promise<ShResult> {
    const env: Record<string, string> = {};
    for (const [k, v] of Object.entries({ ...process.env, ...this.opts.env })) if (v !== undefined) env[k] = v;
    const r = await req<{ id: string; pid: number }>({
      op: "run", cmd: this.cmd, cwd: this.cwd, env, clear_env: true, session: selfSession(), repl: REPL_ID,
      parent: process.env.HARNESS_RUN ?? null, stdin: this.opts.stdin ?? null,
    });
    this.backend = "daemon";
    this.run = r.id;
    this.pid = r.pid;
    this.#ready();
    // The cell owns the run (stopping the cell kills it, cancel.ts) unless it was detached on purpose, which the stop's report names.
    if (this.opts.detach) currentTask()?.detached.push(`${r.id} (${oneLine(this.cmd, 60)})`);
    else own({ what: `${r.id} (${oneLine(this.cmd, 60)})`, exited: this.exited, kill: () => void req({ op: "run_kill", id: r.id }).catch((e) => console.error(`kill ${r.id}: ${e}`)) });
    if (this.#killed !== undefined) await req({ op: "run_kill", id: r.id });
    const timer = this.opts.timeoutMs ? setTimeout(() => this.kill(), this.opts.timeoutMs) : undefined;
    const pump = async (s: Stream, stream: "out" | "err") => {
      let offset = 0;
      for (;;) {
        const v = await req<ReadResp>({ op: "run_read", id: r.id, stream, offset, wait_ms: 1000, max_bytes: 4 << 20 });
        if (v.data) this.out[s].push(v.data, Buffer.byteLength(v.data));
        offset = v.next_offset;
        if (v.done && v.eof) return;
      }
    };
    await Promise.all([pump("stdout", "out"), pump("stderr", "err")]);
    clearTimeout(timer);
    const m = await req<RunMeta>({ op: "run_status", id: r.id });
    this.ended = new Date();
    this.exit_code = m.exit_code;
    this.signal = m.signal;
    return this.result();
  }

  #local(argv: string[]): Promise<ShResult> {
    const opts = this.opts;
    const sub = (this.#sub = Bun.spawn(argv, {
      cwd: this.cwd, env: { ...process.env, ...opts.env }, stdin: opts.stdin != null ? new Blob([opts.stdin]) : "ignore", stdout: "pipe", stderr: "pipe",
      // Its own process group: kill() and a stop of the cell (the cell owns it, effects.ts) end everything it started.
      detached: true,
    }));
    const timer = opts.timeoutMs ? setTimeout(() => this.kill(), opts.timeoutMs) : undefined;
    void sub.exited.then(() => clearTimeout(timer));
    this.backend = "local";
    this.pid = sub.pid;
    this.#ready();
    if (this.#killed !== undefined) killGroup(sub.pid, this.#killed);
    const pump = async (s: Stream, r: ReadableStream<Uint8Array>) => {
      const dec = new TextDecoder();
      for await (const c of r) this.out[s].push(dec.decode(c, { stream: true }), c.byteLength);
      this.out[s].push(dec.decode(), 0);
    };
    const pipes = Promise.all([pump("stdout", sub.stdout), pump("stderr", sub.stderr)]);
    // Exit status lands when the process exits; a grandchild holding the pipes open delays the result by at most 500ms.
    return sub.exited.then(async () => {
      this.ended = new Date();
      this.exit_code = sub.exitCode;
      this.signal = sub.signalCode == null ? null : String(sub.signalCode);
      await Promise.race([pipes, Bun.sleep(500)]);
      return this.result();
    });
  }

  get status() { return this.ended ? (this.signal ? "killed" : "exited") : "running"; }
  get elapsed_ms() { return (this.ended ?? new Date()).getTime() - this.started.getTime(); }
  result(): ShResult {
    const { id, run, pid, exit_code, signal } = this;
    return { id, run, pid, exit_code, signal, stdout: this.out.stdout.text, stderr: this.out.stderr.text };
  }
  /** `await sh(...)` waits for exit and gives { exit_code, stdout, stderr }. */
  then<A, B>(ok?: ((r: ShResult) => A | PromiseLike<A>) | null, fail?: ((e: unknown) => B | PromiseLike<B>) | null) { return abortable(this.exited).then(ok, fail); }
  /** Signal the command and everything it started (its process group). Daemon runs: SIGTERM, then SIGKILL after 3s (the signal argument is ignored). */
  kill(signal: NodeJS.Signals = "SIGTERM") {
    if (this.run) req({ op: "run_kill", id: this.run }).catch((e) => console.error(`kill ${this.run}: ${e}`));
    else if (this.#sub) killGroup(this.#sub.pid, signal);
    else this.#killed = signal;
    return this;
  }
  view() {
    const { id, run, backend, pid, cmd, cwd, status, exit_code, signal, started, ended, elapsed_ms } = this;
    const o = this.out;
    return { id, run, backend, pid, cmd, cwd, status, exit_code, signal, started, ended, elapsed_ms,
      stdout_bytes: o.stdout.bytes, stderr_bytes: o.stderr.bytes, truncated: o.stdout.truncated || o.stderr.truncated };
  }
  [Bun.inspect.custom]() { return this.view(); }
}

const registry = new Map<string, Proc>();
let seq = 0;

/** Spawn a command; returns a handle at once. `await` it for { exit_code, stdout, stderr }. Goes through harnessd when it's up. */
export function sh(cmd: string | string[], opts: ShOpts = {}): Proc {
  const text = typeof cmd === "string" ? cmd : cmd.map(shq).join(" ");
  // The shell is for running programs; reading files or the tree has typed calls (glob, ls, du, rg, Bun.file) that the person
  // sees as what they did.
  if (opts.detach && (opts.local || !daemonPresent())) {
    throw new Error(`sh(${JSON.stringify(oneLine(text, 60))}, { detach: true }): a detached command is held by harnessd, so it stays visible in objects() after its cell is gone; ${opts.local ? "it cannot also be local: true" : `harnessd is not running (no ${SOCK()})`}`);
  }
  const verdict = checkShell(text, opts.why);
  if (!verdict.ok) throw refusal(verdict.message);
  const argv = typeof cmd === "string" ? ["/bin/sh", "-c", cmd] : cmd;
  const short = oneLine(text);
  const s = step("exec", `Running ${short}`, `Ran ${short}`, text);
  const p = quiet(() => new Proc(`p${++seq}`, text, opts.cwd ?? process.cwd(), argv, opts));
  registry.set(p.id, p);
  // The step's result is what it printed: stdout, then stderr, the tail of each that fits.
  s && p.exited.then(
    (r) => s.end(r.exit_code === 0, r.signal ? `killed (${r.signal})` : `exit ${r.exit_code}`, r, [r.stdout, r.stderr].filter(Boolean).join("\n").slice(-(16 << 10)) || undefined),
    (e) => s.end(false, e instanceof Error ? e.message : String(e)),
  );
  return p;
}

export type ProcRef = string | number | Proc;
const getProc = (ref: ProcRef): Proc => {
  if (ref instanceof Proc) return ref;
  const p = typeof ref === "number" ? [...registry.values()].findLast((x) => x.pid === ref)
    : registry.get(ref) ?? [...registry.values()].find((x) => x.run === ref);
  if (!p) throw new Error(`no process ${ref}; see procs()`);
  return p;
};

/** All REPL-started processes, finished ones included. */
export function procs() {
  return table([...registry.values()].map((p) => {
    const { id, run, pid, cmd, status, exit_code, started, elapsed_ms, stdout_bytes, stderr_bytes, truncated } = p.view();
    return { id, run, pid, cmd: cmd.length > 80 ? cmd.slice(0, 79) + "…" : cmd, status, exit_code, started, elapsed_ms, stdout_bytes, stderr_bytes, truncated };
  }));
}

export type ReadOpts = { from?: number; to?: number; tail?: number; new?: boolean; cursor?: string };

/** Lines of one stream as a Table<{ n, text }> (n is 1-based and stable even after old output is dropped). Every read moves the cursor to the end. */
export function lines(ref: ProcRef, stream: Stream = "stdout", opts: ReadOpts = {}) {
  const b = getProc(ref).out[stream];
  const key = opts.cursor ?? "default";
  let base = b.droppedLines;
  let text = b.text;
  if (opts.new) {
    const at = Math.max(b.cursors.get(key) ?? 0, b.droppedChars) - b.droppedChars;
    base += b.text.slice(0, at).split("\n").length - 1;
    text = b.text.slice(at);
  }
  b.cursors.set(key, b.end);
  const ls = text.split("\n");
  if (ls.at(-1) === "") ls.pop();
  let rows = ls.map((t, i) => ({ n: base + i + 1, text: t }));
  if (opts.from !== undefined) rows = rows.filter((r) => r.n >= opts.from!);
  if (opts.to !== undefined) rows = rows.filter((r) => r.n <= opts.to!);
  if (opts.tail !== undefined) rows = rows.slice(-opts.tail);
  return table(rows);
}

/** Line filter shared by stored-stage reads. */
function pickLines(text: string, opts: ReadOpts) {
  const ls = text.split("\n");
  if (ls.at(-1) === "") ls.pop();
  let rows = ls.map((t, i) => ({ n: i + 1, text: t }));
  if (opts.from !== undefined) rows = rows.filter((r) => r.n >= opts.from!);
  if (opts.to !== undefined) rows = rows.filter((r) => r.n <= opts.to!);
  if (opts.tail !== undefined) rows = rows.slice(-opts.tail);
  return rows.map((r) => r.text).join("\n");
}

export type RunStage = {
  text: string; pipeline: number; pid: number | null; exit_code: number | null; signal: string | null;
  status: "pending" | "running" | "done" | "skipped" | "unknown"; start_ms: number | null; end_ms: number | null; stdout_bytes: number; stderr_bytes: number;
};
export type RunMeta = {
  id: string; cmd: string; cwd: string; session: number | null; parent: string | null; pid: number; start_ms: number; end_ms: number | null;
  exit_code: number | null; signal: string | null; status: "running" | "exited" | "killed" | "lost"; split: boolean;
  pipelines: { op: ";" | "&&" | "||" | null; stages: number[] }[]; stages: RunStage[];
  final_out_bytes: number; final_err_bytes: number;
  procs?: { pid: number; ppid: number; depth: number; stat: string; etime: string; command: string }[];
};
export type RunRef = string | Proc;
const runId = async (r: RunRef): Promise<string> => {
  if (typeof r === "string") return r;
  await Promise.race([r.started_on, r.exited.catch(() => {})]);
  if (!r.run) throw new Error(`${r.id} did not run through harnessd (no stored stages)`);
  return r.run;
};

const stageCursors = new Map<string, number>();
/** Stored output of one stage of any agent's run (or 'final': what the caller saw). Same line options as out(); new: true reads on from the last read with that cursor. */
export async function stage(run: RunRef, i: number | "final" = "final", opts: ReadOpts & { stream?: "out" | "err" } = {}): Promise<string> {
  const id = await runId(run);
  const stream = opts.stream ?? "out";
  const key = `${id}:${i}:${stream}:${opts.cursor ?? "default"}`;
  let offset = opts.new ? stageCursors.get(key) ?? 0 : 0;
  let text = "";
  for (;;) {
    const v = await req<ReadResp>({ op: "run_read", id, stage: i === "final" ? null : i, stream, offset, max_bytes: 16 << 20 });
    text += v.data;
    offset = v.next_offset;
    if (v.eof || !v.data) break;
  }
  stageCursors.set(key, offset);
  return pickLines(text, opts);
}

/** Full record of a run (any agent's): per-stage pid/exit/bytes/status, live processes while running. */
export async function runInfo(run: RunRef): Promise<RunMeta & { stages: Table<RunStage & { i: number }> }> {
  const m = await req<RunMeta>({ op: "run_status", id: await runId(run) });
  return { ...m, stages: table(m.stages.map((s, i) => ({ i, ...s }))) };
}

/** Recent harnessd runs, newest first: this agent's by default, every agent's with all: true. */
export async function runs(opts: { all?: boolean; session?: number; limit?: number } = {}) {
  const session = opts.all ? null : opts.session ?? selfSession();
  const { runs } = await req<{ runs: RunMeta[] }>({ op: "runs", session, limit: opts.limit ?? 50 });
  return table(runs.map((r) => ({
    id: r.id, session: r.session, status: r.status, exit_code: r.exit_code ?? r.signal,
    stages: r.stages.map((s, i) => `${i}:${s.status === "done" ? s.exit_code : s.status}`).join(" "),
    cmd: r.cmd.length > 80 ? r.cmd.slice(0, 79) + "…" : r.cmd, cwd: r.cwd, started: new Date(r.start_ms),
    elapsed_ms: (r.end_ms ?? Date.now()) - r.start_ms, out_bytes: r.final_out_bytes, err_bytes: r.final_err_bytes,
  })));
}

/** stdout as a string; same options as lines(). With { stage }, reads that stage's stored stdout from harnessd (async; ref may be any agent's run id). */
export function out(ref: ProcRef, opts?: ReadOpts): string;
export function out(ref: RunRef, opts: ReadOpts & { stage: number | "final" }): Promise<string>;
export function out(ref: ProcRef, opts: ReadOpts & { stage?: number | "final" } = {}): string | Promise<string> {
  if (opts.stage !== undefined) return stage(typeof ref === "number" ? getProc(ref) : ref, opts.stage, { ...opts, stream: "out" });
  return lines(ref, "stdout", opts).map((r) => r.text).join("\n");
}
/** stderr as a string; same options as out(). */
export function err(ref: ProcRef, opts?: ReadOpts): string;
export function err(ref: RunRef, opts: ReadOpts & { stage: number | "final" }): Promise<string>;
export function err(ref: ProcRef, opts: ReadOpts & { stage?: number | "final" } = {}): string | Promise<string> {
  if (opts.stage !== undefined) return stage(typeof ref === "number" ? getProc(ref) : ref, opts.stage, { ...opts, stream: "err" });
  return lines(ref, "stderr", opts).map((r) => r.text).join("\n");
}
/** Signal a process (default SIGTERM). */
export const kill = (ref: ProcRef, signal?: NodeJS.Signals) => getProc(ref).kill(signal).view();

/** The REPL's process tree (from `ps`): every descendant of this server, indented by depth; proc is the sh() id if tracked. */
export async function ptree() {
  const ps = Bun.spawn(["ps", "-axo", "pid=,ppid=,stat=,etime=,command="], { stdout: "pipe", stderr: "ignore" });
  const text = await new Response(ps.stdout).text();
  const kids = new Map<number, { pid: number; ppid: number; stat: string; etime: string; command: string }[]>();
  for (const line of text.split("\n")) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(\S+)\s+(\S+)\s+(.*)$/);
    if (!m) continue;
    const r = { pid: +m[1]!, ppid: +m[2]!, stat: m[3]!, etime: m[4]!, command: m[5]! };
    if (r.pid === ps.pid) continue;
    (kids.get(r.ppid) ?? kids.set(r.ppid, []).get(r.ppid)!).push(r);
  }
  const byPid = new Map([...registry.values()].map((p) => [p.pid, p.id]));
  const all = new Map<number, { pid: number; ppid: number; stat: string; etime: string; command: string }>();
  for (const rs of kids.values()) for (const r of rs) all.set(r.pid, r);
  const rows: { pid: number; ppid: number; depth: number; proc: string | null; stat: string; etime: string; command: string }[] = [];
  const walk = (pid: number, depth: number) => {
    for (const r of kids.get(pid) ?? []) {
      const command = r.command.length > 100 ? r.command.slice(0, 99) + "…" : r.command;
      rows.push({ pid: r.pid, ppid: r.ppid, depth, proc: byPid.get(r.pid) ?? null, stat: r.stat, etime: r.etime, command: "  ".repeat(depth) + command });
      walk(r.pid, depth + 1);
    }
  };
  walk(process.pid, 0);
  // Commands run through harnessd are the daemon's children, not ours: show each live run's tree too.
  for (const p of registry.values()) {
    const r = p.backend === "daemon" && p.status === "running" ? all.get(p.pid) : undefined;
    if (!r) continue;
    rows.push({ pid: r.pid, ppid: r.ppid, depth: 0, proc: p.id, stat: r.stat, etime: r.etime, command: `[${p.run}] ` + r.command.slice(0, 100) });
    walk(r.pid, 1);
  }
  return table(rows);
}

/** What help() says about this module's calls (docs.ts). */
export const docs: Docs = {
  sh: {
    area: "shell",
    for: "run a program; start a server or a watcher that keeps running",
    sig: "sh(cmd: string | string[], { cwd?, env?, stdin?: string, why?, maxBytes? = 8MB per stream kept in memory (oldest lines dropped, line numbers kept), timeoutMs?, local?: true to bypass harnessd, detach?: true }): Proc",
    about: "the awaitable form of spawn('proc'): start now, return a handle at once; `await sh(...)` (or a cell ending in sh(...)) waits and gives { id: 'p1', run: 'r12' | null, pid, exit_code, signal, stdout, stderr }. Runs through harnessd (bash) when it's up: the command is split at top-level ; && || and |, and the FULL stdout and stderr of EVERY pipeline stage is stored as run r12 stage 0, 1, ... readable later by any agent (`cargo clippy 2>&1 | head -n5` shows you 5 lines, clippy still runs to the end and stage(r, 0) has all of it — so pipe through head/tail/grep freely, nothing is lost). stdout here = last stage of each pipeline; stderr = every stage's. With no daemon it is a local process (run: null). Handle: id, run, pid, status running|exited|killed, kill(). The cell that starts a command owns it: when the cell is stopped (cancel, its limit) the command and everything it started is killed; a cell that ends normally leaves it running. { detach: true } keeps it through a stop too (harnessd holds it; objects() shows it). The shell is for running programs: a command line that only reads files or the tree (cat, head, tail, grep, rg, find, ls, stat, wc, sed -n, awk, du, tree, which), a GitHub read through gh, or curl/wget is refused with the typed call to use unless { why } says why the shell is needed; filtering a program's output (cargo test 2>&1 | grep FAIL) runs.",
    example: "const server = sh('bun run dev', { detach: true }); await Bun.sleep(500); out(server, { new: true })",
    throws: "Error 'Not run: ...' for a command that only reads files or the tree (it names the typed call; { why } lets it run); Error for { detach: true } without harnessd. `await sh(...)` does not throw on a non-zero exit: read exit_code.",
  },
  procs: {
    area: "shell",
    sig: "procs(): Table<{ id; run; pid; cmd; status; exit_code; started; elapsed_ms; stdout_bytes; stderr_bytes; truncated }>",
    about: "this REPL's sh() processes, finished included.",
  },
  out: {
    area: "shell",
    for: "read what a process printed so far",
    sig: "out(p: 'p1' | 'r12' | pid | Proc, { from?, to?, tail?, new?: true, cursor? = 'default', stage? }): string",
    about: "stdout lines; from/to are 1-based inclusive line numbers, tail last N; new: only output since the previous read with that cursor (every read moves it to the end). With stage: n | 'final' it returns Promise<string> of that stored stage (same as stage()).",
    example: "out('p1', { tail: 20 })",
  },
  err: {
    area: "shell",
    sig: "err(p, opts?): string",
    about: "stderr, same options as out() (stage too).",
  },
  lines: {
    area: "shell",
    sig: "lines(p, 'stdout' | 'stderr' = 'stdout', opts?): Table<{ n; text }>",
    about: "same options as out(), as numbered rows.",
  },
  kill: {
    area: "shell",
    for: "stop a process",
    sig: "kill(p, signal? = 'SIGTERM'): proc record",
    about: "signal a command and everything it started (its process group). harnessd runs: SIGTERM to the run's process group, SIGKILL after 3s.",
  },
  ptree: {
    area: "shell",
    sig: "ptree(): Promise<Table<{ pid; ppid; depth; proc: 'p1' | null; stat; etime; command (indented by depth) }>>",
    about: "live process tree of everything this REPL spawned, grandchildren included, plus each live harnessd run's tree (from ps).",
  },
  runs: {
    area: "shell",
    sig: "runs({ all?: boolean, session?: number, limit? = 50 }): Promise<Table<{ id: 'r12'; session; status: running|exited|killed|lost; exit_code; stages: '0:0 1:running' (index:exit|status); cmd; cwd; started; elapsed_ms; out_bytes; err_bytes }>>",
    about: "harnessd runs newest first: this agent's by default, every agent's with all: true (or one session's).",
  },
  stage: {
    area: "shell",
    sig: "stage(run: 'r12' | Proc, i: number | 'final' = 'final', { stream?: 'out' | 'err' = 'out', from?, to?, tail?, new?, cursor? }): Promise<string>",
    about: "stored output of one pipeline stage of any agent's run (works while it runs: new: true tails it); 'final' = what the caller saw (stdout of each pipeline's last stage; stream 'err' = all stderr).",
  },
  runInfo: {
    area: "shell",
    sig: "runInfo(run: 'r12' | Proc): Promise<{ id; cmd; cwd; session; parent; status; exit_code; signal; start_ms; end_ms; pipelines: { op: ';'|'&&'|'||'|null; stages: number[] }[]; stages: Table<{ i; text; pipeline; pid; exit_code; signal; status: pending|running|done|skipped|unknown; start_ms; end_ms; stdout_bytes; stderr_bytes }>; procs?: live process tree while running }>",
    about: "the run graph for one command.",
  },
};

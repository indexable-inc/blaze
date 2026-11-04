// Process table for subprocesses started from the REPL: sh() spawns and returns a handle at once;
// stdout/stderr are kept in memory (last maxBytes per stream, older lines dropped but still numbered).
import { table } from "./table.ts";

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

export type ShOpts = { cwd?: string; env?: Record<string, string>; stdin?: string; maxBytes?: number; timeoutMs?: number };
export type ShResult = { id: string; pid: number; exit_code: number | null; signal: string | null; stdout: string; stderr: string };

export class Proc {
  readonly started = new Date();
  ended?: Date;
  exit_code: number | null = null;
  signal: string | null = null;
  readonly out: Record<Stream, Buf>;
  readonly sub: Bun.Subprocess<"ignore" | Blob, "pipe", "pipe">;
  readonly exited: Promise<ShResult>;
  constructor(readonly id: string, readonly cmd: string, readonly cwd: string, argv: string[], opts: ShOpts) {
    const max = opts.maxBytes ?? 8 << 20;
    this.out = { stdout: new Buf(max), stderr: new Buf(max) };
    this.sub = Bun.spawn(argv, {
      cwd, env: { ...process.env, ...opts.env }, stdin: opts.stdin != null ? new Blob([opts.stdin]) : "ignore", stdout: "pipe", stderr: "pipe",
      ...(opts.timeoutMs ? { timeout: opts.timeoutMs } : {}),
    });
    const pump = async (s: Stream, r: ReadableStream<Uint8Array>) => {
      const dec = new TextDecoder();
      for await (const c of r) this.out[s].push(dec.decode(c, { stream: true }), c.byteLength);
      this.out[s].push(dec.decode(), 0);
    };
    const pipes = Promise.all([pump("stdout", this.sub.stdout), pump("stderr", this.sub.stderr)]);
    // Exit status lands when the process exits; a grandchild holding the pipes open delays the result by at most 500ms.
    this.exited = this.sub.exited.then(async () => {
      this.ended = new Date();
      this.exit_code = this.sub.exitCode;
      this.signal = this.sub.signalCode == null ? null : String(this.sub.signalCode);
      await Promise.race([pipes, Bun.sleep(500)]);
      return this.result();
    });
  }
  get pid() { return this.sub.pid; }
  get status() { return this.ended ? (this.signal ? "killed" : "exited") : "running"; }
  get elapsed_ms() { return (this.ended ?? new Date()).getTime() - this.started.getTime(); }
  result(): ShResult {
    const { id, pid, exit_code, signal } = this;
    return { id, pid, exit_code, signal, stdout: this.out.stdout.text, stderr: this.out.stderr.text };
  }
  /** `await sh(...)` waits for exit and gives { exit_code, stdout, stderr }. */
  then<A, B>(ok?: ((r: ShResult) => A | PromiseLike<A>) | null, fail?: ((e: unknown) => B | PromiseLike<B>) | null) { return this.exited.then(ok, fail); }
  kill(signal: NodeJS.Signals | number = "SIGTERM") { this.sub.kill(signal); return this; }
  view() {
    const { id, pid, cmd, cwd, status, exit_code, signal, started, ended, elapsed_ms } = this;
    const o = this.out;
    return { id, pid, cmd, cwd, status, exit_code, signal, started, ended, elapsed_ms,
      stdout_bytes: o.stdout.bytes, stderr_bytes: o.stderr.bytes, truncated: o.stdout.truncated || o.stderr.truncated };
  }
  [Bun.inspect.custom]() { return this.view(); }
}

const registry = new Map<string, Proc>();
let seq = 0;

/** Spawn a command (string runs under `sh -c`); returns a handle at once. `await` it for { exit_code, stdout, stderr }. */
export function sh(cmd: string | string[], opts: ShOpts = {}): Proc {
  const argv = typeof cmd === "string" ? ["/bin/sh", "-c", cmd] : cmd;
  const p = new Proc(`p${++seq}`, typeof cmd === "string" ? cmd : cmd.join(" "), opts.cwd ?? process.cwd(), argv, opts);
  registry.set(p.id, p);
  return p;
}

export type ProcRef = string | number | Proc;
const getProc = (ref: ProcRef): Proc => {
  if (ref instanceof Proc) return ref;
  const p = typeof ref === "number" ? [...registry.values()].findLast((x) => x.pid === ref) : registry.get(ref);
  if (!p) throw new Error(`no process ${ref}; see procs()`);
  return p;
};

/** All REPL-started processes, finished ones included. */
export function procs() {
  return table([...registry.values()].map((p) => {
    const { id, pid, cmd, status, exit_code, started, elapsed_ms, stdout_bytes, stderr_bytes, truncated } = p.view();
    return { id, pid, cmd: cmd.length > 80 ? cmd.slice(0, 79) + "…" : cmd, status, exit_code, started, elapsed_ms, stdout_bytes, stderr_bytes, truncated };
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

/** stdout as a string; same options as lines(). */
export const out = (ref: ProcRef, opts: ReadOpts = {}) => lines(ref, "stdout", opts).map((r) => r.text).join("\n");
/** stderr as a string; same options as lines(). */
export const err = (ref: ProcRef, opts: ReadOpts = {}) => lines(ref, "stderr", opts).map((r) => r.text).join("\n");
/** Signal a process (default SIGTERM). */
export const kill = (ref: ProcRef, signal?: NodeJS.Signals | number) => getProc(ref).kill(signal).view();

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
  const rows: { pid: number; ppid: number; depth: number; proc: string | null; stat: string; etime: string; command: string }[] = [];
  const walk = (pid: number, depth: number) => {
    for (const r of kids.get(pid) ?? []) {
      const command = r.command.length > 100 ? r.command.slice(0, 99) + "…" : r.command;
      rows.push({ pid: r.pid, ppid: r.ppid, depth, proc: byPid.get(r.pid) ?? null, stat: r.stat, etime: r.etime, command: "  ".repeat(depth) + command });
      walk(r.pid, depth + 1);
    }
  };
  walk(process.pid, 0);
  return table(rows);
}

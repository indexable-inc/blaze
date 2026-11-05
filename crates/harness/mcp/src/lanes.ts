// The REPL's lanes, as the MCP server (mcp.ts) runs them. A lane is one evaluator process (evaluator.ts) holding its own
// bindings: lanes run in parallel, and cells in one lane share bindings and run concurrently with each other. Each cell is
// type-checked here first (check/, one TypeScript process for every lane), against the globals and every name the lane's
// earlier cells bound; a type error refuses it, naming the line. Cancelling, a timeout or a reset kills the lane's process
// group (and the harnessd runs its cells started): its bindings are gone, and its next cell starts a new evaluator.
//
// A lane's restorable bindings (hibernate.ts) are saved to `<HARNESS_HOME>/repl/s<session>/<lane>.state` shortly after each
// cell ends and when the server stops, and restored when a server for the same chat starts the lane again (a resumed chat
// after `harness stop`). What could not be saved (functions, class instances, handles) is named in the first reply after.
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { TypeService } from "./check/type-service.ts";
import { vetTyped, type CheckerState, type LaneChecker } from "./check/cell-types.ts";
import { isSvelteCell } from "./ui/compile.ts";
import { req, daemonPresent } from "./daemon.ts";
import type { FromEvaluator, ToEvaluator } from "./protocol.ts";

const EVALUATOR = fileURLToPath(new URL("./evaluator.ts", import.meta.url));
const GLOBALS = fileURLToPath(new URL("./globals.ts", import.meta.url));
export const LANE_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,39}$/;
/** A lane's state is saved this long after its last cell ended (one write for a burst of cells). */
const SAVE_MS = 300;
const READY_MS = 30_000;

/** `1.24 s`, `12.3 s`, `124 s`. */
export const seconds = (ms: number) => `${(ms / 1000).toFixed(ms < 10_000 ? 2 : ms < 100_000 ? 1 : 0)} s`;

export interface Cell {
  readonly id: string;
  readonly lane: Lane;
  readonly code: string;
  readonly started: number;
  ended?: number;
  ok?: boolean;
  text: string;
  /** The reply has said how it ended (a cell that outlived its call is said once, in the next reply). */
  delivered: boolean;
  readonly done: Promise<void>;
  finish(ok: boolean, text: string): void;
}

function cell(id: string, lane: Lane, code: string): Cell {
  let resolve!: () => void;
  const done = new Promise<void>((r) => (resolve = r));
  const c: Cell = {
    id, lane, code, started: Date.now(), text: "", delivered: false, done,
    finish(ok, text) {
      if (c.ended !== undefined) return;
      Object.assign(c, { ok, text, ended: Date.now() });
      resolve();
    },
  };
  return c;
}

export class Lane {
  /** Bumped each time the lane's evaluator is replaced: its checker scope is keyed by it. */
  gen = 0;
  proc?: ChildProcess;
  started?: Promise<void>;
  exited?: Promise<void>;
  /** This evaluator's id in harnessd: its cells are `<repl>.t3`, and the runs its cells start carry it. */
  repl?: string;
  cwd = process.cwd();
  readonly running = new Map<string, Cell>();
  /** Said once, in the next reply: what a restore brought back, or why it could not. */
  notes: string[] = [];
  readonly asked = new Map<number, (m: FromEvaluator) => void>();
  reqs = 0;
  saveTimer?: Timer;
  constructor(readonly name: string) {}
  get key() { return `${this.name}#${this.gen}`; }
}

export interface LanesOptions {
  /** Where lanes' state is saved; none: nothing is saved or restored. */
  stateDir?: string;
  /** Ask the person (MCP elicitation); false when the client cannot. */
  confirm(message: string): Promise<boolean>;
  /** The evaluator's environment (tests point HARNESS_SOCK elsewhere). */
  env?: Record<string, string | undefined>;
}

/** `<HARNESS_HOME or ~/.harness>/repl/s<HARNESS_SESSION>`: one chat's lanes, the same after a restart; none outside a chat. */
export function stateDirFor(env: Record<string, string | undefined> = process.env): string | undefined {
  if (!env.HARNESS_SESSION) return undefined;
  return join(env.HARNESS_HOME ?? join(homedir(), ".harness"), "repl", `s${env.HARNESS_SESSION}`);
}

export class Lanes {
  readonly lanes = new Map<string, Lane>();
  readonly cells = new Map<string, Cell>();
  private seq = 0;
  private types?: TypeService;
  private globalNames?: string[];
  private readonly spillDir: string;

  constructor(private readonly options: LanesOptions) {
    this.spillDir = options.stateDir ? join(options.stateDir, "out") : join(tmpdir(), `harness-repl-${process.pid}`);
  }

  lane(name: string): Lane {
    let lane = this.lanes.get(name);
    if (!lane) this.lanes.set(name, (lane = new Lane(name)));
    return lane;
  }

  statePath(lane: Lane): string | undefined {
    return this.options.stateDir && join(this.options.stateDir, `${lane.name}.state`);
  }

  private checker(lane: Lane): LaneChecker | undefined {
    return this.types?.lane(lane.key, () => lane.cwd);
  }

  /** The lane's evaluator, started (and its saved state restored) if it is not running. */
  start(lane: Lane): Promise<void> {
    return (lane.started ??= this.boot(lane));
  }

  private async boot(lane: Lane): Promise<void> {
    const env = { ...process.env, ...this.options.env, HARNESS_LANE: lane.name };
    // Its own process group, so a cancel kills what its cells spawned too; its stdout is not the MCP channel.
    const proc = spawn(process.execPath, [EVALUATOR], { cwd: lane.cwd, env, detached: true, stdio: ["ignore", 2, 2, "ipc"] });
    lane.proc = proc;
    let ready!: (m: Extract<FromEvaluator, { type: "ready" }>) => void;
    let failed!: (e: Error) => void;
    const readied = new Promise<Extract<FromEvaluator, { type: "ready" }>>((r, f) => { ready = r; failed = f; });
    lane.exited = new Promise<void>((resolve) => proc.on("exit", (code, signal) => {
      failed(new Error(`lane ${lane.name}'s evaluator exited (${signal ?? `code ${code}`}) before it was ready`));
      if (lane.proc === proc) this.lost(lane, `lane ${lane.name}'s evaluator exited (${signal ?? `code ${code}`}); its bindings are gone`);
      resolve();
    }));
    proc.on("message", (raw: unknown) => {
      const m = raw as FromEvaluator; // evaluator.ts is this socket's only peer, speaking protocol.ts
      switch (m.type) {
        case "ready": return ready(m);
        case "done": {
          const c = lane.running.get(m.id);
          if (!c) return;
          lane.running.delete(m.id);
          lane.cwd = m.cwd;
          c.finish(m.ok, m.text);
          if (!isSvelteCell(c.code)) this.checker(lane)?.commit(c.code, m.bound).catch((e) => console.error(`[harness] checker commit ${c.id}:`, e));
          this.saveSoon(lane);
          return;
        }
        case "confirm": {
          this.options.confirm(m.message).then((ok) => this.send(lane, { type: "confirmed", req: m.req, ok }), () => this.send(lane, { type: "confirmed", req: m.req, ok: false }));
          return;
        }
        case "peeked": return lane.asked.get(-Number(m.id.slice(1)))?.(m);
        default: return lane.asked.get(m.req)?.(m);
      }
    });
    const m = await Promise.race([readied, new Promise<never>((_, f) => setTimeout(() => f(new Error(`lane ${lane.name}'s evaluator was not ready in ${READY_MS / 1000} s`)), READY_MS).unref())]);
    lane.repl = m.repl;
    if (!this.types) {
      this.globalNames = m.names;
      this.types = new TypeService({ module: GLOBALS, names: m.names });
    }
    const path = this.statePath(lane);
    if (path && existsSync(path)) await this.restore(lane, path);
    void this.checker(lane)?.warm();
  }

  private ask<T extends FromEvaluator>(lane: Lane, message: (req: number) => ToEvaluator, key?: number): Promise<T> {
    const req = key ?? ++lane.reqs;
    return new Promise<T>((resolve, reject) => {
      lane.asked.set(req, (m) => { lane.asked.delete(req); m.type === "failed" ? reject(new Error(m.error)) : resolve(m as T); });
      lane.exited?.then(() => { if (lane.asked.delete(req)) reject(new Error(`lane ${lane.name}'s evaluator exited`)); });
      this.send(lane, message(req));
    });
  }

  private send(lane: Lane, m: ToEvaluator) {
    try { lane.proc?.send(m); } catch (e) { console.error(`[harness] lane ${lane.name}: ${e instanceof Error ? e.message : String(e)}`); }
  }

  private async restore(lane: Lane, path: string) {
    try {
      const m = await this.ask<Extract<FromEvaluator, { type: "restored" }>>(lane, (req) => ({ type: "restore", req, path }));
      lane.cwd = m.cwd;
      await this.checker(lane)?.load(m.checker, m.restored);
      const said: string[] = [];
      if (m.restored.length) said.push(`Restored lane ${lane.name} from before the restart: ${m.restored.join(", ")}.`);
      if (m.dropped.length) said.push(`Not restored (functions, class instances and handles do not survive a restart; run their cells again): ${m.dropped.join(", ")}.`);
      if (said.length) lane.notes.push(`${said.join(" ")} The working directory is ${m.cwd}.`);
    } catch (e) {
      const bad = `${path}.bad`;
      try { renameSync(path, bad); } catch { /* reported below either way */ }
      lane.notes.push(`Lane ${lane.name}'s saved bindings could not be restored (${e instanceof Error ? e.message : String(e)}), so it starts empty; the file is now ${bad}.`);
    }
  }

  /** Save the lane's state soon: once per burst of cells. */
  private saveSoon(lane: Lane) {
    if (!this.statePath(lane)) return;
    clearTimeout(lane.saveTimer);
    lane.saveTimer = setTimeout(() => { void this.save(lane).catch((e) => console.error(`[harness] saving lane ${lane.name}:`, e)); }, SAVE_MS);
  }

  /** Write the lane's restorable bindings and their types to its state file (all or nothing); what was saved and dropped. */
  async save(lane: Lane): Promise<{ saved: string[]; dropped: string[]; bytes: number } | undefined> {
    clearTimeout(lane.saveTimer);
    const path = this.statePath(lane);
    if (!path || !lane.proc || !lane.repl) return undefined;
    mkdirSync(this.options.stateDir!, { recursive: true, mode: 0o700 });
    const checker: CheckerState = (await this.checker(lane)?.save()) ?? { cells: {}, scope: [] };
    return this.ask<Extract<FromEvaluator, { type: "saved" }>>(lane, (req) => ({ type: "save", req, path, checker }));
  }

  /** The evaluator went away: its running cells fail with `why`, and the lane starts over with its next cell. */
  private lost(lane: Lane, why: string) {
    clearTimeout(lane.saveTimer);
    for (const c of lane.running.values()) c.finish(false, why);
    lane.running.clear();
    void this.types?.drop(lane.key);
    lane.gen++;
    lane.proc = undefined;
    lane.started = undefined;
    lane.repl = undefined;
  }

  /**
   * Stop the lane: kill its process group and the harnessd runs its cells started, fail its running cells with `why`, and
   * forget its saved state. Its bindings are gone; the cells it stopped are returned.
   */
  async kill(lane: Lane, why: string): Promise<Cell[]> {
    const stopped = [...lane.running.values()];
    const proc = lane.proc;
    const repl = lane.repl;
    const exited = lane.exited;
    this.lost(lane, why);
    if (proc?.pid) {
      try { process.kill(-proc.pid, "SIGKILL"); } catch { proc.kill("SIGKILL"); }
      await exited;
    }
    if (repl) await this.killRuns(repl);
    const path = this.statePath(lane);
    if (path) rmSync(path, { force: true });
    return stopped;
  }

  /** Kill the harnessd runs (sh(), $) a stopped evaluator's cells started: they are the daemon's children, not the lane's. */
  private async killRuns(repl: string) {
    if (!daemonPresent()) return;
    try {
      const session = process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null;
      const runs = await req<{ id: string; repl?: string | null; status: string }[] | { runs: { id: string; repl?: string | null; status: string }[] }>({ op: "runs", session, limit: 200 });
      const list = Array.isArray(runs) ? runs : runs.runs;
      await Promise.all(list.filter((r) => r.repl === repl && r.status === "running").map((r) => req({ op: "run_kill", id: r.id }).catch(() => undefined)));
    } catch (e) {
      console.error(`[harness] killing the runs of ${repl}:`, e);
    }
  }

  /**
   * Check `code` as the lane's next cell and start it: the cell, or the refusal (a type error, an `as any`). `notes` gets
   * what the reply should also say (the cell ran without its check, and why).
   */
  async run(lane: Lane, code: string, call: string | null, notes: string[]): Promise<Cell | { refusal: string }> {
    await this.start(lane);
    let run = code;
    if (!isSvelteCell(code)) {
      const checker = this.checker(lane);
      if (checker) {
        const vetted = await vetTyped(code, checker, { warn: (text) => notes.push(text) });
        if (vetted.refusal) return { refusal: vetted.refusal };
        if (vetted.run) run = vetted.run;
      }
    }
    // The checker can take a while: the lane may have been cancelled meanwhile.
    if (!lane.proc) await this.start(lane);
    const c = cell(`t${++this.seq}`, lane, code);
    this.cells.set(c.id, c);
    lane.running.set(c.id, c);
    this.send(lane, { type: "run", id: c.id, code: run, call, spill: join(this.spillDir, `${c.id}.txt`) });
    return c;
  }

  /** What a running cell has printed so far (its last 4,000 characters), or nothing if it does not answer within a second. */
  async peek(c: Cell): Promise<string> {
    const lane = c.lane;
    if (!lane.proc) return "";
    const key = -Number(c.id.slice(1));
    const answer = this.ask<Extract<FromEvaluator, { type: "peeked" }>>(lane, () => ({ type: "peek", id: c.id }), key).then((m) => m.text, () => "");
    return Promise.race([answer, new Promise<string>((r) => setTimeout(() => r(""), 1000).unref())]);
  }

  /** Every running cell, oldest first. */
  running(): Cell[] {
    return [...this.lanes.values()].flatMap((l) => [...l.running.values()]).sort((a, b) => a.started - b.started);
  }

  /** Save every lane, then stop them all (the server is going away). */
  async close(): Promise<void> {
    await Promise.all([...this.lanes.values()].map((l) => Promise.race([
      this.save(l).catch((e) => console.error(`[harness] saving lane ${l.name}:`, e)),
      new Promise((r) => setTimeout(r, 3000).unref()),
    ])));
    for (const l of this.lanes.values()) if (l.proc?.pid) { try { process.kill(-l.proc.pid, "SIGKILL"); } catch { /* gone */ } }
    await this.types?.close().catch(() => undefined);
  }

  /** The globals cells get, as the evaluator listed them (after the first lane started). */
  names(): string[] | undefined { return this.globalNames; }
}

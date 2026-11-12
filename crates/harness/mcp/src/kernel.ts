// The kernel's one API: a request in, an answer out, in one versioned schema. Every way in is an adapter of a few lines
// over `Kernel.handle`: MCP over stdio (mcp.ts, how agents connect today) and the kernel's own socket in harness frames
// of deterministic CBOR (serve.ts, which a CLI or the app can speak). What a reply says (time against the estimate,
// backgrounding, a refusal, a stop's report, notices that waited) is decided here and in the pure modules it calls
// (patience.ts, stop.ts, disclose.ts, notices.ts), never in an adapter.
import { Stopped, stopTask } from "./cancel.ts";
import { setRunner } from "./cells.ts";
import { presentLogs } from "./disclose.ts";
import { cellOf, cellStack } from "./evaluator.ts";
import { description } from "./help.ts";
import { noticeText } from "./notices.ts";
import { gate, limit, told, waited } from "./patience.ts";
import { post } from "./post.ts";
import { Repl } from "./repl.ts";
import { allTasks, currentTask, getTask, watchTasks, type Task } from "./tasks.ts";
import { notices, pump, tellAgent } from "./tell.ts";
import type { Checker } from "./typecheck.ts";
import { watch } from "./watch.ts";

/** The schema of `Call` and `Answer`. A peer speaking another is refused by number. */
export const SCHEMA = 1;

export type Call =
  /** Run a cell. It answers when the cell ends or after `yield_ms`, whichever is first. */
  | { op: "run"; code: string; yield_ms?: number; timeout_ms?: number; expect_ms?: number; why?: string; call?: string | null }
  /** Stop a running cell (stop.ts). */
  | { op: "cancel"; task: string }
  /** The tool's description (help.ts). */
  | { op: "describe" };

export type Answer = { text: string; error: boolean; task?: string; status?: string };

/** A cell that finishes within this answers in the same call. Backgrounding one sooner costs the agent a whole turn to
 *  come back for it (about 6 s of its own, measured 2026-11-10), which is longer than most waits it saved. */
export const YIELD_MS = 30_000;

const FIELDS: Record<Call["op"], Record<string, "string" | "number" | "string?">> = {
  run: { code: "string", yield_ms: "number", timeout_ms: "number", expect_ms: "number", why: "string", call: "string?" },
  cancel: { task: "string" },
  describe: {},
};

/** `v` as a Call, or why it is not one, naming the field. */
export function parseCall(v: unknown): Call | string {
  if (typeof v !== "object" || v === null) return "a request is a record with an `op`";
  const o = v as Record<string, unknown>;
  if (typeof o.op !== "string" || !(o.op in FIELDS)) return `unknown op ${JSON.stringify(o.op)}: this kernel (schema ${SCHEMA}) has run, cancel, describe`;
  const fields = FIELDS[o.op as Call["op"]];
  for (const [k, x] of Object.entries(o)) {
    if (k === "op" || x === undefined) continue;
    const want = fields[k];
    if (!want) return `${o.op} has no field \`${k}\`; it takes ${Object.keys(fields).join(", ") || "none"}`;
    if (!(typeof x === want.replace("?", "") || (want.endsWith("?") && x === null))) return `${o.op}.${k} is ${x === null ? "null" : typeof x}, and must be a ${want.replace("?", " or null")}`;
  }
  const required = o.op === "run" ? "code" : o.op === "cancel" ? "task" : null;
  if (required && typeof o[required] !== "string") return `${o.op} needs \`${required}\``;
  return o as Call;
}

/** What a tool with one input record means (the MCP `ts` tool): `cancel` names a cell to stop, else `code` is a cell to run. */
export function toolCall(input: { code?: string; cancel?: string; yield_ms?: number; timeout_ms?: number; expect_ms?: number; why?: string }, call: string | null): Call | string {
  const { code, cancel, ...rest } = input;
  if (cancel !== undefined && code !== undefined) return "give `code` to run a cell or `cancel` to stop one, not both";
  if (cancel !== undefined) return { op: "cancel", task: cancel };
  if (code === undefined) return "give `code`, the TypeScript to run (or `cancel`, the id of a cell to stop)";
  return { op: "run", code, ...rest, call };
}

/** Which cells an error nobody awaited belongs to, from most to least certain: the cell whose code its stack passes
 *  through; the cell whose context raised it; the only cell running; every cell running; the last cell that ran. */
export function blame(e: unknown, context: Task | undefined, tasks: readonly Task[]): { tasks: Task[]; sure: boolean } {
  const id = cellOf(e);
  const named = id ? tasks.find((t) => t.id === id && !t.restored) : undefined;
  if (named) return { tasks: [named], sure: true };
  if (context) return { tasks: [context], sure: true };
  const running = tasks.filter((t) => t.status === "running");
  if (running.length) return { tasks: running, sure: running.length === 1 };
  const last = tasks.findLast((t) => !t.restored);
  return { tasks: last ? [last] : [], sure: false };
}

export class Kernel {
  readonly repl: Repl;
  /** What the first reply must say before anything else (the journal's restore, a kernel-wide failure). */
  readonly notes: string[] = [];

  constructor(globals: Record<string, unknown>, o: { checker?: Checker; known?: string[] } = {}) {
    this.repl = new Repl(globals, o);
    watchTasks(post);
    setRunner((code) => this.repl.run(code));
  }

  /** The time the session has spent with a cell running (overlapping cells once). */
  sessionMs = (): number => waited(allTasks().filter((t) => !t.restored).map((t) => [t.started.getTime(), (t.ended ?? new Date()).getTime()] as const));

  /** An error nobody awaited (a rejection, a timer's throw) is the cell's that caused it: a line in its logs, and a
   *  notice to the agent when no reply of that cell is still to come. With no cell to give it to, the next reply says it. */
  unawaited(e: unknown, what: "an unawaited promise rejected" | "a callback threw"): void {
    // A stop rejects every wait of its cell: the ones nobody was awaiting are the stop, not news.
    if (e instanceof Stopped) return;
    const { tasks, sure } = blame(e, currentTask(), allTasks());
    const text = cellStack(e);
    if (!tasks.length) { this.notes.push(`[kernel] ${what} before any cell ran: ${text}`); return; }
    const whose = sure ? "" : tasks.length > 1 ? ` (while ${tasks.map((t) => t.id).join(", ")} ran: it is one of theirs)` : ` (after ${tasks[0]!.id}, the last cell to run: probably its)`;
    for (const t of tasks) {
      const line = `[${t.id}] ${what}${whose}: ${text}`;
      if (t.status !== "running" || t.returned) tellAgent(t, line, { tag: "kernel" });
      else t.log(`[kernel] ${line}`);
    }
  }

  /** Answer a request. A string is a request already found wanting (`toolCall`): it is answered as the error it is. */
  async handle(call: Call | string): Promise<Answer> {
    const bad = typeof call === "string" ? call : parseCall(call);
    if (typeof bad === "string" || typeof call === "string") return { text: String(bad), error: true };
    if (call.op === "describe") return { text: description(), error: false };
    notices.callBegan();
    try {
      const a = call.op === "cancel" ? await this.#cancel(call.task) : await this.#run(call);
      return { ...a, text: this.#carry(a.text, a.task) };
    } catch (e) {
      return { text: this.#carry(e instanceof Error ? e.message : String(e)), error: true };
    } finally {
      void pump();
    }
  }

  /** A reply carries what waited to be said: the kernel's notes first, then notices of other cells. */
  #carry(text: string, about?: string): string {
    // The end of the cell this reply is about is the reply itself.
    const waiting = notices.callEnds().filter((n) => !(n.end && n.task === about));
    const head = this.notes.splice(0);
    const tail = waiting.length ? `\n\n# While you were away:\n${noticeText(waiting)}` : "";
    return `${head.length ? `${head.join("\n")}\n` : ""}${text}${tail}`;
  }

  async #cancel(id: string): Promise<Answer> {
    const t = getTask(id);
    const report = await stopTask(t, "cancelled");
    await t.done;
    return { text: `[${t.id} stopped ${told({ elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms, running: false, session_ms: this.sessionMs() }).time}]\n${report}`, error: false, task: t.id, status: t.status };
  }

  async #run(c: Extract<Call, { op: "run" }>): Promise<Answer> {
    // The moment to doubt a slow run is before it starts (patience.ts).
    const refused = gate(c.expect_ms, c.why);
    if (refused !== null) return { text: refused, error: true };
    const t = this.repl.run(c.code, { limit_ms: limit(c.expect_ms, c.timeout_ms), call: c.call ?? null, expect_ms: c.expect_ms, why: c.why });
    let timer: Timer | undefined;
    await Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, c.yield_ms ?? YIELD_MS)))]);
    clearTimeout(timer);
    const running = t.status === "running";
    const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms: c.expect_ms, running, session_ms: this.sessionMs() });
    const logs = t.logs.length ? `${presentLogs(t.logs, { id: t.id })}\n` : "";
    const note = advice ? `${advice}\n` : "";
    if (running) {
      t.returned = true;
      // From here the cell's time reaches the agent as notices: when it ends, and each time it passes its estimate again.
      watch(t, this.sessionMs);
      return { text: `[${t.id} running ${time}, backgrounded: you will be told when it ends or runs over; go on with other work]\n${note}${logs}`.trimEnd(), error: false, task: t.id, status: t.status };
    }
    return { text: `[${t.id} ${t.status} ${time}]\n${note}${logs}${t.result}`, error: t.status !== "done", task: t.id, status: t.status };
  }
}

// A persistent TypeScript REPL: each cell is TypeScript (or Svelte markup), type-checked against the kernel's globals and
// the names earlier cells bound (typecheck.ts), then run as a strict async function whose top-level names later cells
// see (evaluator.ts). Cells run concurrently; each is a task (tasks.ts) with its own console capture and AbortSignal,
// and the caller decides how long to wait before backgrounding it (kernel.ts).
import { resolve as resolvePath } from "node:path";
import { abortable, stopTask } from "./cancel.ts";
import { present } from "./disclose.ts";
import { install as installEffects, fsFor } from "./effects.ts";
import { Scope, cellStack, prepare } from "./evaluator.ts";
import { hint } from "./help.ts";
import { packages, parseSpec } from "./packages.ts";
import { overLimit } from "./patience.ts";
import { installCapture, inTask, newTask, type Task } from "./tasks.ts";
import type { Checker } from "./typecheck.ts";
import { watchdog } from "./watchdog.ts";
import { isSvelteCell, stripCell } from "./ui/compile.ts";
import { count, installUi, isElement, show, uiError } from "./ui/index.ts";

/** What a cell's `import` gets: node:fs with its steps (effects.ts), a path relative to the cell's working directory,
 *  an npm package from the shared, pinned cache (packages.ts), or the builtin. */
export async function importFor(spec: string): Promise<unknown> {
  const fs = await fsFor(spec);
  if (fs) return fs;
  if (spec.startsWith("./") || spec.startsWith("../")) return import(resolvePath(process.cwd(), spec));
  const p = parseSpec(spec);
  return p ? packages().import(spec, p) : import(spec);
}

/** A cell refused before it ran (its types): the words are the whole error, with no stack. */
export class Refused extends Error { override name = "Refused"; }

export type RunOptions = { limit_ms?: number; call?: string | null; expect_ms?: number; why?: string };

export class Repl {
  readonly scope: Scope;

  checker?: Checker;
  /** Names a missing name may have meant, beyond the scope's own (the documented calls, `cargo.test`). */
  readonly #known: string[];

  /** `globals`: the kernel's functions by name. `checker`: the cells' type checker, when cells are checked. */
  constructor(globals: Record<string, unknown>, o: { checker?: Checker; known?: string[] } = {}) {
    this.checker = o.checker;
    this.#known = o.known ?? [];
    // __import: a cell's `import`, static or dynamic (importFor).
    this.scope = new Scope({ ...globals, __import: importFor });
    installUi(this.scope.globals);
    installCapture();
    installEffects();
  }

  /** Start a cell as a task and return it at once. Past `limit_ms` it is stopped (cancel.ts). */
  run(code: string, o: RunOptions = {}): Task {
    const t = newTask(code, o.call ?? null, o);
    // Its limit is enforced here when the cell awaits, and from outside the thread when it never does (watchdog.ts):
    // armed before the cell's first line runs, which may be the one that never returns.
    if (o.limit_ms !== undefined) watchdog()?.arm(t.id, Date.now() + o.limit_ms);
    void t.done.then(() => watchdog()?.disarm(t.id));
    const cell = inTask(t, () => this.#exec(t));
    t.settled = cell.then(() => {}, () => {});
    const timer = o.limit_ms === undefined ? undefined : setTimeout(() => void stopTask(t, overLimit(o.limit_ms!, t.expect_ms).why).catch(() => {}), o.limit_ms);
    // What showing the value throws (a component's first render) is the cell's error too.
    void abortable(cell, t.controller.signal).then((value) => inTask(t, () => this.#finish(t, value))).catch(async (e) => {
      if (!t.stopping) return t.finish("error", e, this.#failure(e));
      const advice = o.limit_ms === undefined ? null : overLimit(o.limit_ms, t.expect_ms).advice;
      t.finish("stopped", e, [await t.stopping, advice].filter(Boolean).join("\n"));
    }).finally(() => clearTimeout(timer));
    return t;
  }

  async #exec(t: Task): Promise<unknown> {
    // TypeScript with its types erased, or a cell of Svelte markup compiled (ui/compile.ts; the compiler's warnings go to
    // the cell's logs, which the agent reads). Its types are checked first (typecheck.ts): markup is its compiler's to check.
    const prepared = prepare(stripCell(t.code, (w) => t.log(`[svelte] ${w}`)));
    const admitted = this.checker && !isSvelteCell(t.code) ? await this.checker.admit(t.code, prepared.names) : undefined;
    if (admitted && "refusal" in admitted) throw new Refused(admitted.refusal);
    return this.scope.run(prepared, t.id, (bound) => admitted?.settle(bound));
  }

  #finish(t: Task, value: unknown): void {
    // A cell whose value is a component draws it (the person sees it under the cell); the agent is told it did.
    if (isElement(value)) {
      show(t, value);
      return t.finish("done", undefined, `UI shown (${count(t.ui)} elements)`);
    }
    const text = present(value, { id: t.id }).text;
    t.finish("done", value, t.ui === undefined ? text : `UI shown (${count(t.ui)} elements)${value === undefined ? "" : `\n${text}`}`);
  }

  /** An error as the agent reads it: at the cell's own line, with the nearest name and where to look when it names one. */
  #failure(e: unknown): string {
    if (e instanceof Refused) return e.message;
    // The parser's errors carry the cell position in their message; their stack is the parser's.
    if (e instanceof SyntaxError && typeof (e as { raisedAt?: unknown }).raisedAt === "number") return `SyntaxError: ${e.message}`;
    // An error that came through the cell's UI is said at the cell's line, without Svelte's frames or the kernel's (ui/index.ts).
    const text = uiError(e) ?? cellStack(e);
    const more = e instanceof Error ? hint(e.message, [...this.scope.names(), ...this.#known]) : null;
    return more ? `${text}\n${more}` : text;
  }
}

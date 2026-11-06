// A persistent TypeScript REPL: each cell is TS, top-level declarations survive into later cells,
// top-level await works, the last expression is the result, and console output is captured per task
// (see tasks.ts). Cells run concurrently; the caller decides how long to wait before backgrounding.
//
// Each cell is posted to harnessd as it runs (`repl_task`): indexed as a `repl` object, and, when the agent said which
// tool call ran it (`call`), drawn by the app as that call's card. A post is the cell's whole state: its code, status,
// logs, steps (effects.ts) and, once done, its value as the app shows it (shown.ts). Its first post goes at once, later
// ones while it runs are coalesced to one per POST_MS; all go out in order, and its end goes at once. A cell that draws UI
// (Svelte 5 markup, ui/) carries its tree as `ui`; each change to it after the end is posted again, also one per POST_MS.
import * as acorn from "acorn";
import { resolve as resolvePath } from "node:path";
import { packages, parseSpec } from "./packages.ts";
import { nuon } from "./nuon.ts";
import { installCapture, inTask, newTask, watchTasks, type Task } from "./tasks.ts";
import { req, daemonPresent, listen } from "./daemon.ts";
import { install as installEffects, fsFor, rawsOf } from "./effects.ts";
import { brief, shown } from "./shown.ts";
import { stripCell } from "./ui/compile.ts";
import { count, dispatch, installUi, isElement, show, uiError } from "./ui/index.ts";

// This REPL process as the parent of its cells' objects in the harness index (`<repl>.t3`, kind repl).
export const REPL_ID = `repl-${process.pid}-${Date.now().toString(36)}`;
const cap = (s: string, n = 64 << 10) => (s.length > n ? s.slice(0, n) + `… [${s.length - n} more chars]` : s);
const POST_MS = 100;
const posting = new WeakMap<Task, { chain: Promise<unknown>; timer?: Timer }>();

/** The cell's state as harnessd gets it. */
export function body(t: Task) {
  const finished = t.status !== "running";
  // A value that is one of its steps' results (a cell ending in `await googleSearch(...)`) is shown once, under the step.
  const value = !finished || t.value === undefined || t.ui !== undefined || rawsOf(t).includes(t.value) ? null
    // An error: one plain sentence for the person (`message`), its stack as the model got it (the harness frames below the cell cut).
    : t.status === "error" ? { t: "error" as const, message: brief(t.value), stack: t.result }
    : shown(t.value);
  return {
    op: "repl_task" as const, repl: REPL_ID, id: t.id, call: t.call, session: process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null,
    cwd: process.cwd(), code: cap(t.code), status: t.status, started_ms: t.started.getTime(), ended_ms: t.ended?.getTime() ?? null,
    logs: cap(t.logs.join("\n")), result: cap(t.result), effects: t.effects, effects_dropped: t.effectsDropped, value, ui: t.ui ?? null,
  };
}

/** This kernel's UI feed (daemon.ts `listen`), opened when a cell first draws UI: what the person does to it comes back here. */
let feed: { stop(): void } | undefined;
function listenOnce() {
  if (feed || !daemonPresent()) return;
  feed = listen(REPL_ID, (ev) => {
    const id = ev.cell.slice(ev.cell.lastIndexOf(".") + 1);
    if (ev.cell !== `${REPL_ID}.${id}`) return console.error(`[harness] ui event for cell ${ev.cell}: not this kernel's (${REPL_ID})`);
    dispatch(id, ev.handler, ev.value).then((why) => { if (why) console.error(`[harness] ${why}`); });
  });
}

/** Index a cell in harnessd (best effort; the REPL works without the daemon): now when it starts or ends (`now`), else within POST_MS. */
function post(t: Task, now = false) {
  if (!daemonPresent()) return;
  if (t.ui !== undefined) listenOnce();
  const first = !posting.has(t);
  const p = posting.get(t) ?? posting.set(t, { chain: Promise.resolve() }).get(t)!;
  const send = () => {
    p.timer = undefined;
    // In order: each post waits for the one before it, and reads the task when it goes.
    p.chain = p.chain.then(() => req(body(t))).catch(() => {});
  };
  if (first || now) {
    clearTimeout(p.timer);
    send();
  } else if (!p.timer) {
    p.timer = setTimeout(send, POST_MS);
  }
}

const AsyncFunction = (async () => {}).constructor as new (...args: string[]) => (...a: unknown[]) => Promise<unknown>;

type Node = acorn.Node & Record<string, any>;

function patternNames(p: Node, out: string[] = []): string[] {
  switch (p.type) {
    case "Identifier": out.push(p.name); break;
    case "ObjectPattern": for (const prop of p.properties) patternNames(prop.type === "RestElement" ? prop.argument : prop.value, out); break;
    case "ArrayPattern": for (const el of p.elements) if (el) patternNames(el, out); break;
    case "RestElement": patternNames(p.argument, out); break;
    case "AssignmentPattern": patternNames(p.left, out); break;
  }
  return out;
}

/** Every `import(x)` expression, at any depth, becomes `__import(x)`, so a dynamic import resolves the way a static one
 *  does (packages.ts), not from this file's folder. */
function dynamicImports(js: string): string {
  const ast = acorn.parse(js, { ecmaVersion: "latest", sourceType: "module", allowAwaitOutsideFunction: true }) as Node;
  const starts: number[] = [];
  const walk = (n: unknown) => {
    if (Array.isArray(n)) { for (const c of n) walk(c); return; }
    if (typeof n !== "object" || n === null || typeof (n as Node).type !== "string") return;
    const node = n as Node;
    if (node.type === "ImportExpression") starts.push(node.start);
    for (const k of Object.keys(node)) if (k !== "type" && k !== "start" && k !== "end") walk(node[k]);
  };
  walk(ast);
  let out = js;
  // `import` is six characters at the expression's start; edits go from the end so earlier offsets hold.
  for (const s of starts.sort((a, b) => b - a)) out = out.slice(0, s) + "__import" + out.slice(s + 6);
  return out;
}

/** What a cell's `import` gets: node:fs with its steps (effects.ts), a path relative to the cell's working directory,
 *  an npm package from the shared, pinned cache (packages.ts), or the builtin. */
export async function importFor(spec: string): Promise<unknown> {
  const fs = await fsFor(spec);
  if (fs) return fs;
  if (spec.startsWith("./") || spec.startsWith("../")) return import(resolvePath(process.cwd(), spec));
  const p = parseSpec(spec);
  return p ? packages().import(spec, p) : import(spec);
}

/** Rewrite top-level declarations into assignments on the persistent scope; return the last expression. */
export function rewrite(source: string): { body: string; names: string[] } {
  const js = dynamicImports(source);
  const ast = acorn.parse(js, { ecmaVersion: "latest", sourceType: "module", allowAwaitOutsideFunction: true }) as Node;
  const src = (n: Node) => js.slice(n.start, n.end);
  const names: string[] = [];
  const edits: { start: number; end: number; text: string }[] = [];
  const stmts: Node[] = ast.body;

  for (const [i, raw] of stmts.entries()) {
    let node = raw;
    if (node.type === "ExportNamedDeclaration" && node.declaration) node = node.declaration;
    else if (node.type === "ExportDefaultDeclaration") node = node.declaration;
    let text: string | undefined;
    if (node.type === "VariableDeclaration") {
      text = ";(" + node.declarations.map((d: Node) => {
        names.push(...patternNames(d.id));
        return d.init ? `${src(d.id)} = ${src(d.init)}` : `${d.id.name} = undefined`;
      }).join(", ") + ");";
    } else if ((node.type === "FunctionDeclaration" || node.type === "ClassDeclaration") && node.id) {
      names.push(node.id.name);
      text = `;${node.id.name} = ${src(node)};`;
    } else if (node.type === "ImportDeclaration") {
      const spec = JSON.stringify(node.source.value);
      const parts: string[] = [];
      for (const s of node.specifiers) {
        names.push(s.local.name);
        if (s.type === "ImportNamespaceSpecifier") parts.push(`${s.local.name} = __m`);
        else if (s.type === "ImportDefaultSpecifier") parts.push(`${s.local.name} = __m.default`);
        else parts.push(`${s.local.name} = __m[${JSON.stringify(s.imported.name ?? s.imported.value)}]`);
      }
      text = `;{ const __m = await __import(${spec}); ${parts.join("; ")} }`;
    } else if (i === stmts.length - 1 && node.type === "ExpressionStatement") {
      text = `return (${src(node.expression)});`;
    }
    if (text !== undefined) edits.push({ start: raw.start, end: raw.end, text });
  }
  let body = js;
  for (const e of edits.reverse()) body = body.slice(0, e.start) + e.text + body.slice(e.end);
  return { body, names };
}

export class Repl {
  /** Persistent bindings; seeded with the exposed functions. */
  readonly scope: Record<string, unknown>;

  constructor(globals: Record<string, unknown>) {
    // __import: a cell's `import`, static or dynamic (importFor).
    this.scope = { ...globals, __import: importFor };
    installUi(this.scope);
    installCapture();
    installEffects();
    watchTasks(post);
  }

  /** Start a cell as a task and return it at once; cells run concurrently, each with its own console capture.
   *  `call` is the agent's id for the tool call running it, when known. */
  run(code: string, timeoutMs?: number, call: string | null = null): Task {
    const t = newTask(code, call);
    post(t);
    inTask(t, () => this.#run(t, code, timeoutMs)).then(({ ok, value, result }) => { t.finish(ok, value, result); post(t, true); });
    return t;
  }

  async #run(t: Task, code: string, timeoutMs?: number): Promise<{ ok: boolean; value: unknown; result: string }> {
    try {
      // TypeScript with its types erased, or a cell of Svelte markup compiled (ui/compile.ts); the compiler's warnings go to the
      // cell's logs, which the agent reads.
      const js = stripCell(code, (w) => t.logs.push(`[svelte] ${w}`));
      const { body, names } = rewrite(js);
      for (const n of names) if (!(n in this.scope)) this.scope[n] = undefined;
      // Sloppy-mode `with` routes bare identifiers to the persistent scope, falling back to globals.
      const fn = new AsyncFunction("__scope", `with (__scope) { return await (async () => {\n${body}\n})(); }`);
      let timer: Timer | undefined;
      const value = await (timeoutMs === undefined ? fn(this.scope) : Promise.race([
        fn(this.scope),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`cell timed out after ${timeoutMs}ms`)), timeoutMs); }),
      ]).finally(() => clearTimeout(timer)));
      // A cell whose value is a component draws it (the person sees it under the cell); the agent is told it did.
      if (isElement(value)) {
        show(t, value);
        return { ok: true, value: undefined, result: `UI shown (${count(t.ui)} elements)` };
      }
      if (t.ui !== undefined) return { ok: true, value, result: `UI shown (${count(t.ui)} elements)${value === undefined ? "" : `\n${typeof value === "string" ? value : nuon(value, { maxRows: 100 })}`}` };
      return { ok: true, value, result: value === undefined ? "undefined" : typeof value === "string" ? value : nuon(value, { maxRows: 100 }) };
    } catch (e) {
      // An error that came through the cell's UI is said at the cell's line, without Svelte's frames or the kernel's (ui/index.ts).
      const ui = uiError(e);
      if (ui !== undefined) return { ok: false, value: e, result: ui };
      // Drop the harness frames below the cell.
      const stack = e instanceof Error ? (e.stack ?? e.message).split("\n") : [String(e)];
      const cut = stack.findIndex((l) => l.includes("at #run "));
      return { ok: false, value: e, result: (cut > 0 ? stack.slice(0, cut) : stack).join("\n") };
    }
  }
}

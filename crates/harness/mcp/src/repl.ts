// A persistent TypeScript REPL: each cell is TS, top-level declarations survive into later cells,
// top-level await works, the last expression is the result, and console output is captured per task
// (see tasks.ts). Cells run concurrently; the caller decides how long to wait before backgrounding.
import * as acorn from "acorn";
import { nuon } from "./nuon.ts";
import { installCapture, inTask, newTask, type Task } from "./tasks.ts";
import { req, daemonPresent } from "./daemon.ts";

// This REPL process as the parent of its cells' objects in the harness index (`<repl>.t3`, kind repl).
export const REPL_ID = `repl-${process.pid}-${Date.now().toString(36)}`;
const cap = (s: string, n = 64 << 10) => (s.length > n ? s.slice(0, n) + `… [${s.length - n} more chars]` : s);
/** Index a cell in harnessd (best effort; the REPL works without the daemon). */
function post(t: Task) {
  if (!daemonPresent()) return;
  req({
    op: "repl_task", repl: REPL_ID, id: t.id, session: process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null,
    cwd: process.cwd(), code: cap(t.code), status: t.status, started_ms: t.started.getTime(), ended_ms: t.ended?.getTime() ?? null,
    logs: cap(t.logs.join("\n")), result: cap(t.result),
  }).catch(() => {});
}

const transpiler = new Bun.Transpiler({ loader: "ts", target: "bun", trimUnusedImports: false, deadCodeElimination: false });
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

/** Rewrite top-level declarations into assignments on the persistent scope; return the last expression. */
export function rewrite(js: string): { body: string; names: string[] } {
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
      text = `;{ const __m = await import(${spec}); ${parts.join("; ")} }`;
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
    this.scope = { ...globals };
    installCapture();
  }

  /** Start a cell as a task and return it at once; cells run concurrently, each with its own console capture. */
  run(code: string, timeoutMs?: number): Task {
    const t = newTask(code);
    post(t);
    inTask(t, () => this.#run(code, timeoutMs)).then(({ ok, value, result }) => { t.finish(ok, value, result); post(t); });
    return t;
  }

  async #run(code: string, timeoutMs?: number): Promise<{ ok: boolean; value: unknown; result: string }> {
    try {
      const js = transpiler.transformSync(code);
      const { body, names } = rewrite(js);
      for (const n of names) if (!(n in this.scope)) this.scope[n] = undefined;
      // Sloppy-mode `with` routes bare identifiers to the persistent scope, falling back to globals.
      const fn = new AsyncFunction("__scope", `with (__scope) { return await (async () => {\n${body}\n})(); }`);
      let timer: Timer | undefined;
      const value = await (timeoutMs === undefined ? fn(this.scope) : Promise.race([
        fn(this.scope),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`cell timed out after ${timeoutMs}ms`)), timeoutMs); }),
      ]).finally(() => clearTimeout(timer)));
      return { ok: true, value, result: value === undefined ? "undefined" : typeof value === "string" ? value : nuon(value, { maxRows: 100 }) };
    } catch (e) {
      // Drop the harness frames below the cell.
      const stack = e instanceof Error ? (e.stack ?? e.message).split("\n") : [String(e)];
      const cut = stack.findIndex((l) => l.includes("at #run "));
      return { ok: false, value: e, result: (cut > 0 ? stack.slice(0, cut) : stack).join("\n") };
    }
  }
}

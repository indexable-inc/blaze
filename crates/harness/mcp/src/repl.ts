// A persistent TypeScript REPL: each cell is TS, top-level declarations survive into later cells,
// top-level await works, the last expression is the result, and console output is captured.
import * as acorn from "acorn";

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
  #queue: Promise<unknown> = Promise.resolve();

  constructor(globals: Record<string, unknown>) {
    this.scope = { ...globals };
  }

  /** Cells run one at a time so console capture stays per-cell. */
  run(code: string, timeoutMs = 60_000) {
    const job = this.#queue.then(() => this.#run(code, timeoutMs));
    this.#queue = job.catch(() => {});
    return job;
  }

  async #run(code: string, timeoutMs: number): Promise<{ ok: boolean; result: string; logs: string[] }> {
    const logs: string[] = [];
    const saved = { log: console.log, info: console.info, warn: console.warn, error: console.error, debug: console.debug };
    const capture = (level: string) => (...args: unknown[]) =>
      logs.push((level === "log" ? "" : `[${level}] `) + args.map((a) => (typeof a === "string" ? a : Bun.inspect(a, { depth: 6 }))).join(" "));
    for (const k of Object.keys(saved) as (keyof typeof saved)[]) console[k] = capture(k);
    try {
      const js = transpiler.transformSync(code);
      const { body, names } = rewrite(js);
      for (const n of names) if (!(n in this.scope)) this.scope[n] = undefined;
      // Sloppy-mode `with` routes bare identifiers to the persistent scope, falling back to globals.
      const fn = new AsyncFunction("__scope", `with (__scope) { return await (async () => {\n${body}\n})(); }`);
      let timer: Timer | undefined;
      const value = await Promise.race([
        fn(this.scope),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`cell timed out after ${timeoutMs}ms`)), timeoutMs); }),
      ]).finally(() => clearTimeout(timer));
      return { ok: true, result: value === undefined ? "undefined" : Bun.inspect(value, { depth: 8, colors: false }), logs };
    } catch (e) {
      return { ok: false, result: e instanceof Error ? (e.stack ?? e.message) : String(e), logs };
    } finally {
      Object.assign(console, saved);
    }
  }
}

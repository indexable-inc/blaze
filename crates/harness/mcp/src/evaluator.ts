// How a cell's code runs and how its names reach later cells (the mechanism is den's, repl/src/evaluator.ts).
//
// A cell is the body of a strict async function, so `const`, `let`, `class` and `function` at its top level are real
// lexical bindings with the language's own rules (a `const` cannot be assigned, a name used before its line is an error,
// an undeclared name assigned is an error, not a new global). For each such name the cell registers a getter and a
// setter over its binding; a later cell finds an earlier cell's names through them, so a name is one live binding
// however many cells and closures read it. The newest initialized binding of a name wins, which makes redeclaring a
// name in a later cell a new binding, and a cell that threw before it reached `const x = ...` leaves the older `x`.
//
// Lookup: the strict function is made inside one `with (scope)` whose scope is a proxy answering only for the names
// cells bound and the kernel's globals, so everything else resolves as in any module. That one statement is the whole
// of the non-strict code: it is what lets a cell name a binding that did not exist when the cell was compiled.
//
// Cells run concurrently and share these bindings: a cell sees a name as soon as the cell declaring it has initialized
// it, even while that cell still runs.
import * as acorn from "acorn";

type Node = acorn.Node & Record<string, any>;
type Edit = { start: number; end: number; text: string };

const parse = (js: string) => acorn.parse(js, { ecmaVersion: "latest", sourceType: "module", allowAwaitOutsideFunction: true, allowReturnOutsideFunction: true }) as Node;

const children = (n: Node): Node[] => {
  const out: Node[] = [];
  for (const k of Object.keys(n)) {
    const v = n[k];
    if (Array.isArray(v)) { for (const c of v) if (c && typeof c.type === "string") out.push(c); }
    else if (v && typeof v === "object" && typeof v.type === "string") out.push(v);
  }
  return out;
};

function patternNames(p: Node, out: Set<string>): void {
  switch (p.type) {
    case "Identifier": out.add(p.name); break;
    case "ObjectPattern": for (const prop of p.properties) patternNames(prop.type === "RestElement" ? prop.argument : prop.value, out); break;
    case "ArrayPattern": for (const el of p.elements) if (el) patternNames(el, out); break;
    case "RestElement": patternNames(p.argument, out); break;
    case "AssignmentPattern": patternNames(p.left, out); break;
  }
}

const FUNCTION = /^(?:Function|ArrowFunction|Class)(?:Declaration|Expression)$/;

/** A cell ready to run: JavaScript whose lines are the cell's own, and the names it binds for later cells. */
export type Prepared = { js: string; names: string[] };

/**
 * Compile a cell (JavaScript: its types already erased) for [`Scope.run`]: a static import becomes a `const` over
 * `__import` on the same lines, every `import(x)` becomes `__import(x)` (so a cell's imports resolve as packages.ts and
 * the cell's working directory say, not from this file's folder), and a last expression statement is returned.
 * Throws the parser's SyntaxError, which names the cell's line and column.
 */
export function prepare(js: string): Prepared {
  const ast = parse(js);
  const src = (n: Node) => js.slice(n.start, n.end);
  const names = new Set<string>();
  const edits: Edit[] = [];
  const walk = (n: Node, top: boolean, inFn: boolean) => {
    if (n.type === "ImportExpression") edits.push({ start: n.start, end: n.start + 6, text: "__import" });
    if (!inFn) {
      if ((n.type === "FunctionDeclaration" || n.type === "ClassDeclaration") && top && n.id) names.add(n.id.name);
      // `var` in a block of the cell is the cell's; a function's own declarations are its own.
      if (n.type === "VariableDeclaration" && (top || n.kind === "var")) for (const d of n.declarations) patternNames(d.id, names);
    }
    const fn = inFn || FUNCTION.test(n.type);
    for (const c of children(n)) walk(c, false, fn);
  };
  const body: Node[] = ast.body;
  for (const [i, raw] of body.entries()) {
    // `export` means nothing in a cell: every top-level name is kept.
    if (raw.type === "ExportNamedDeclaration" && !raw.declaration) { edits.push({ start: raw.start, end: raw.end, text: "" }); continue; }
    const node: Node = (raw.type === "ExportNamedDeclaration" || raw.type === "ExportDefaultDeclaration") ? raw.declaration : raw;
    if (node !== raw) edits.push({ start: raw.start, end: node.start, text: "" });
    if (node.type === "ImportDeclaration") {
      const named: string[] = [];
      let all: string | undefined;
      for (const s of node.specifiers) {
        names.add(s.local.name);
        if (s.type === "ImportNamespaceSpecifier") all = s.local.name;
        else if (s.type === "ImportDefaultSpecifier") named.push(`default: ${s.local.name}`);
        else named.push(`${JSON.stringify(s.imported.name ?? s.imported.value)}: ${s.local.name}`);
      }
      const load = `await __import(${src(node.source)})`;
      const text = !node.specifiers.length ? `${load};`
        : all && named.length ? `const ${all} = ${load}, { ${named.join(", ")} } = ${all};`
        : all ? `const ${all} = ${load};`
        : `const { ${named.join(", ")} } = ${load};`;
      // The lines the declaration took stay, so later lines keep their numbers.
      edits.push({ start: node.start, end: node.end, text: text + "\n".repeat(src(node).split("\n").length - 1) });
      continue;
    }
    walk(node, true, false);
    if (i === body.length - 1 && node.type === "ExpressionStatement") {
      edits.push({ start: node.start, end: node.expression.start, text: "return (" }, { start: node.expression.end, end: node.end, text: ");" });
    }
  }
  // From the end, so earlier offsets hold; at one offset a replacement goes before an insertion in front of it.
  edits.sort((a, b) => b.start - a.start || (b.end - b.start) - (a.end - a.start));
  let out = js;
  for (const e of edits) out = out.slice(0, e.start) + e.text + out.slice(e.end);
  return { js: out, names: [...names] };
}

type Binding = { get(): unknown; set(v: unknown): void; cell: string };

/** The kernel's own frames under a cell: never the agent's concern. */
const KERNEL_FRAME = /\/src\/(?:evaluator|repl|kernel|cancel|tasks)\.ts:|processTicksAndRejections|node:async_hooks/;

/** An error as the agent reads it: its message, and its stack down to the cell's own last frame (`t3.cell.ts:LINE`, the cell's line). */
export function cellStack(e: unknown): string {
  if (!(e instanceof Error)) return String(e);
  const lines = (e.stack ?? "").split("\n");
  const last = lines.findLastIndex((l) => /\bt\d+\.cell\.ts:/.test(l));
  const text = (last >= 0 ? lines.slice(0, last + 1) : lines.filter((l) => !KERNEL_FRAME.test(l))).join("\n");
  return text.includes(e.message) ? text : `${e.name}: ${e.message}${text ? `\n${text}` : ""}`;
}

/** The cell (`t3`) whose code `e`'s stack passes through, if any. */
export const cellOf = (e: unknown): string | undefined => (e instanceof Error ? /\b(t\d+)\.cell\.ts:/.exec(e.stack ?? "")?.[1] : undefined);

/** The names cells share: the kernel's globals, and every binding a cell made. */
export class Scope {
  /** Per name, newest first. A cell's bindings join when it starts and, once it ends, replace the older ones (or leave, if it never initialized them). */
  readonly #bindings = new Map<string, Binding[]>();
  readonly #proxy: object;

  /** `globals` is read live, with its getters: what is added to it later is seen. */
  constructor(readonly globals: Record<string, unknown>) {
    const live = (name: string): Binding | undefined => {
      for (const b of this.#bindings.get(name) ?? []) { try { b.get(); return b; } catch { /* declared, not initialized: the one before it */ } }
      return undefined;
    };
    this.#proxy = new Proxy(Object.create(null), {
      has: (_t, key) => typeof key === "string" && (this.#bindings.has(key) || Object.hasOwn(globals, key)),
      get: (_t, key) => {
        if (typeof key !== "string") return undefined;
        const stack = this.#bindings.get(key);
        if (!stack) return globals[key];
        const b = live(key);
        if (b) return b.get();
        if (Object.hasOwn(globals, key)) return globals[key];
        throw new ReferenceError(`${key} is declared by cell ${stack[0]!.cell}, which is still running and has not reached that line`);
      },
      set: (_t, key, value) => {
        if (typeof key !== "string") return false;
        const b = live(key);
        if (!b) {
          if (Object.hasOwn(globals, key)) throw new TypeError(`${key} is the kernel's and cannot be assigned; declare your own with let ${key} = ...`);
          return false;
        }
        try { b.set(value); }
        catch (e) {
          if (e instanceof TypeError) throw new TypeError(`${key} was declared with const by cell ${b.cell} and cannot be assigned; declare it again with let ${key} = ...`);
          throw e;
        }
        return true;
      },
    });
  }

  /** Every name a cell can use: bound by a cell, or a global. */
  names(): string[] { return [...new Set([...this.#bindings.keys(), ...Object.keys(this.globals)])]; }

  /** Run a prepared cell as `cell` (`t3`, the name its stack frames carry); its value, or what it threw. When it ends
   *  either way, `ended` hears which of its names it really bound. */
  async run(p: Prepared, cell: string, ended?: (bound: string[]) => void): Promise<unknown> {
    const reg = `__bind_${cell}`;
    const registrations = p.names.map((n) => `${reg}(${JSON.stringify(n)}, () => ${n}, (__v) => { ${n} = __v; });`).join(" ");
    // One line in front of the cell's first, so a frame's line is the cell's; indirect eval, so it closes over nothing here.
    const make = (0, eval)(
      `(function (__scope, ${reg}) { with (__scope) { return async function () { "use strict"; ${registrations} ${p.js}\n}; } })\n//# sourceURL=${cell}.cell.ts`,
    ) as (scope: object, bind: (name: string, get: () => unknown, set: (v: unknown) => void) => void) => () => Promise<unknown>;
    const mine: [string, Binding][] = [];
    const fn = make(this.#proxy, (name, get, set) => {
      const b = { get, set, cell };
      mine.push([name, b]);
      const stack = this.#bindings.get(name);
      if (stack) stack.unshift(b); else this.#bindings.set(name, [b]);
    });
    try {
      return await fn();
    } finally {
      const kept: string[] = [];
      for (const [name, b] of mine) {
        const stack = this.#bindings.get(name)!;
        let bound = true;
        try { b.get(); } catch { bound = false; }
        // Initialized: it is the name now, and nothing can reach the older ones. Never initialized: it was never the name.
        if (bound) { stack.splice(stack.indexOf(b) + 1); kept.push(name); }
        else stack.splice(stack.indexOf(b), 1);
        if (!stack.length) this.#bindings.delete(name);
      }
      ended?.(kept);
    }
  }
}

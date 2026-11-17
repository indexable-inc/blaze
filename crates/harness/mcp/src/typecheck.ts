// Cells are type-checked before they run, with earlier cells' names typed (the design is den's, repl/src/type-service.ts,
// cut to one kernel): the pinned TypeScript (7, the Go compiler) runs as one process behind its API, over a small
// project in a temporary directory:
//
//   prelude.d.ts   the kernel's globals, each declared with the type its function has (functions/index.ts `Globals`)
//   c1.ts, c2.ts   each cell that ran and bound names, as a module exporting them
//   cell.ts        the cell being checked: a header importing every name earlier cells bound, then the cell's own text
//
// so a check costs one changed file: 5 to 15 ms warm on an M-series Mac (measured 2026-11-12, typecheck.test.ts holds it
// under CHECK_WARM_MS), about 0.2 s for the first cell. A cell with a type error is refused with its first errors at the
// cell's own lines, before any of it runs. `any` and `@ts-ignore` turn the check off for what they touch, which is how a
// guessed call (`glob(p, { dot: true } as any)`) gets through, so a cell that uses them is refused too.
//
// What is not checked, said here and in help: a cell of Svelte markup (its compiler checks it), and the types of npm
// packages a cell imports by name (they are installed when the cell runs, packages.ts; such an import is `any`).
// When the checker itself fails (its process dies, no answer in CHECK_MS) the cell is refused saying so, and the next
// check starts a new process: no cell runs unchecked.
import { mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { API } from "typescript/unstable/async";
import { clip } from "./disclose.ts";
import { missingName } from "./help.ts";
import { outsideTask } from "./tasks.ts";

/** This package's folder: its node_modules has TypeScript and Bun's types. */
const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
/** How long one check may take before the cell is refused for it. */
export const CHECK_MS = 20_000;
/** The most errors a refusal lists. */
const SHOWN = 6;
/** `Cannot find module` and `no declaration file`: what an npm package not yet installed reports. */
const UNRESOLVED = new Set([2307, 7016, 2792]);
/** Names in a cell's scope that are the kernel's plumbing, not for cells. */
const HIDDEN = /^(?:__|\$\$)/;

/** A cell let through: its names are in scope for later cells from now; `settle` once it ended, with the names it really bound. */
export type Admitted = { settle(bound: readonly string[]): void };

export interface Checker {
  /** Start the checker's process and load the globals' types, so the first cell does not wait for them. */
  warm(): Promise<void>;
  /**
   * Check `source`, which binds `names` when it runs (evaluator.ts). `{ refusal }`: why it may not run (its type errors,
   * at its lines). Else its names are typed for later cells at once, since a cell still running already shares them;
   * a name it turns out never to have bound (it threw first) goes back to the earlier cell's when it settles.
   */
  admit(source: string, names: readonly string[]): Promise<{ refusal: string } | Admitted>;
  close(): Promise<void>;
}

export type Globals = {
  /** The module whose exported type `Globals` has each typed global as a member. */
  module: string;
  /** The names of that type's members cells get. */
  typed: readonly string[];
  /** Every other name in scope (UI elements, which only markup uses): declared, typed `unknown`. */
  untyped: readonly string[];
  /** The documented calls, to point an unknown name at help. */
  known: readonly string[];
};

/** The globals as declarations. */
export function preludeText(g: Globals): string {
  const name = (n: string) => /^[A-Za-z_$][\w$]*$/.test(n);
  return [
    `import type { Globals } from ${JSON.stringify(g.module)};`,
    "declare global {",
    ...g.typed.filter(name).map((n) => `  const ${n}: Globals[${JSON.stringify(n)}];`),
    ...g.untyped.filter((n) => name(n) && !HIDDEN.test(n) && !g.typed.includes(n)).map((n) => `  const ${n}: unknown;`),
    "}",
    "export {};",
    "",
  ].join("\n");
}

type Kind = "const" | "let" | "type";
type Diagnostic = { pos: number; code: number; text: string; messageChain?: readonly { text: string }[] };

/** The 1-based line of `offset` in `source`. */
const lineOf = (source: string, offset: number): number => {
  let line = 1;
  for (let i = 0; i < offset && i < source.length; i++) if (source.charCodeAt(i) === 10) line++;
  return line;
};

/** A refusal as the agent reads it. */
export function refusal(errors: readonly { line: number; text: string }[], known: readonly string[]): string {
  const lines = errors.slice(0, SHOWN).map((e) => {
    const missing = missingName(e.text);
    const at = missing ? known.find((k) => k === missing || k.endsWith(`.${missing}`)) ?? /Did you mean '([\w$]+)'/.exec(e.text)?.[1] : undefined;
    return `  line ${e.line}: ${clip(e.text, 400)}${at && known.includes(at) ? ` (help('${at}'))` : ""}`;
  });
  const more = errors.length > SHOWN ? `\n  … and ${errors.length - SHOWN} more` : "";
  return `Not run: the cell has ${errors.length === 1 ? "a type error" : `${errors.length} type errors`}. Nothing of it ran.\n${lines.join("\n")}${more}\nhelp('name') shows a call's real signature.`;
}

export class TypeChecker implements Checker {
  readonly dir = realpathSync(mkdtempSync(join(tmpdir(), "harness-types-")));
  readonly #config = join(this.dir, "tsconfig.json");
  readonly #cell = join(this.dir, "cell.ts");
  #api: Promise<API> | undefined;
  #opened = false;
  #queue: Promise<unknown> = Promise.resolve();
  #pending: { created: string[]; changed: string[] } = { created: [], changed: [] };
  #cellWritten = false;
  #count = 0;
  /** Each name earlier cells bound: the module that exports it and how a later cell sees it. */
  readonly #scope = new Map<string, { cell: string; kind: Kind }>();

  constructor(readonly globals: Globals) {
    writeFileSync(this.#config, JSON.stringify({
      compilerOptions: {
        target: "ESNext", module: "Preserve", moduleResolution: "bundler", lib: ["ESNext", "DOM"], types: ["bun"], typeRoots: [join(ROOT, "node_modules", "@types")],
        strict: true, noEmit: true, allowImportingTsExtensions: true, skipLibCheck: true,
      },
      files: ["prelude.d.ts", "cell.ts"],
    }));
    writeFileSync(join(this.dir, "prelude.d.ts"), preludeText(globals));
    writeFileSync(this.#cell, "export {};\n");
  }

  /** One at a time, in order: the process has one view of the files. */
  #run<T>(work: () => Promise<T>): Promise<T> {
    const result = this.#queue.then(() => outsideTask(work));
    this.#queue = result.catch(() => undefined);
    return result;
  }

  #start(): Promise<API> {
    // The checker's process is the kernel's: started in no cell's context, so stopping the cell that happened to need it
    // first does not kill it (cancel.ts kills what a cell starts).
    return (this.#api ??= outsideTask(() => import("typescript/unstable/async").then(({ API }) => { this.#opened = false; return new API({ cwd: this.dir }); })));
  }

  async #stop(): Promise<void> {
    const api = this.#api;
    this.#api = undefined;
    await api?.then((a) => a.close()).catch(() => undefined);
  }

  /** The line in front of a cell: every name earlier cells bound that it does not declare again. On the cell's first line, so every line keeps its number. */
  #header(own: ReadonlySet<string>): string {
    const imports = new Map<string, string[]>();
    const lets: string[] = [];
    for (const [name, { cell, kind }] of this.#scope) {
      if (own.has(name)) continue;
      // A `let` may be assigned by a later cell, which an import cannot be.
      if (kind === "let") lets.push(`declare let ${name}: typeof import("./${cell}.ts").${name};`);
      else (imports.get(cell) ?? imports.set(cell, []).get(cell)!).push(kind === "type" ? `type ${name}` : name);
    }
    return [...[...imports].map(([cell, names]) => `import { ${names.join(", ")} } from "./${cell}.ts";`), ...lets].map((l) => `${l} `).join("");
  }

  warm(): Promise<void> {
    return this.#run(async () => { await this.#check("void [glob, rg, sh, fetch, Bun.file];"); });
  }

  admit(source: string, names: readonly string[]): Promise<{ refusal: string } | Admitted> {
    return this.#run(async () => {
      const checked = await this.#check(source);
      return typeof checked === "string" ? { refusal: checked } : this.#commit(source, names, checked);
    });
  }

  /** Why `source` may not run, or what it declares at its top level. */
  async #check(source: string): Promise<string | Map<string, Kind>> {
    {
      const directive = /^.*@ts-(?:ignore|expect-error|nocheck)\b/m.exec(source);
      if (directive) {
        return refusal([{ line: lineOf(source, directive.index), text: `${/@ts-[\w-]+/.exec(directive[0])![0]} turns the type check off for what follows it, which hides the mistake it was written to get past: fix the type` }], []);
      }
      let timer: Timer | undefined;
      const deadline = new Promise<never>((_, fail) => { timer = setTimeout(() => fail(new Error(`no answer in ${CHECK_MS / 1000} s`)), CHECK_MS); });
      try {
        const api = await this.#start();
        const { SyntaxKind } = await import("typescript/unstable/ast");
        type N = {
          kind: number; pos: number; end: number; text?: string; name?: N; elements?: N[]; forEachChild?(f: (n: N) => void): void;
          declarationList?: { flags: number; declarations: { name: N }[] }; importClause?: { name?: N; namedBindings?: N };
        };
        // One pass: the cell behind a header naming what earlier cells bound, its diagnostics and what it declares itself.
        const pass = async (own: ReadonlySet<string>) => {
          const header = this.#header(own);
          writeFileSync(this.#cell, `${header}${source}\nexport {};\n`);
          (this.#cellWritten ? this.#pending.changed : this.#pending.created).push(this.#cell);
          this.#cellWritten = true;
          const fileChanges = this.#pending;
          this.#pending = { created: [], changed: [] };
          const snapshot = await Promise.race([api.updateSnapshot({ ...(this.#opened ? {} : { openProjects: [this.#config] }), fileChanges }), deadline]);
          this.#opened = true;
          try {
            const project = snapshot.getProject(this.#config);
            if (!project) throw new Error("TypeScript opened no project for the cell");
            // The client keeps a file's syntax tree across snapshots when told only of changes: the tree would be the last cell's.
            api.clearSourceFileCache();
            const [found, file] = await Promise.race([Promise.all([project.program.getSemanticDiagnostics(this.#cell), project.program.getSourceFile(this.#cell)]), deadline]);
            if (!file) throw new Error("TypeScript gave no syntax tree for the cell");
            const kinds = new Map<string, Kind>();
            const bind = (n: N | undefined, kind: Kind) => {
              if (!n) return;
              if (n.kind === SyntaxKind.Identifier) kinds.set(n.text!, kind);
              else for (const el of n.elements ?? []) bind(el.name, kind);
            };
            let anyAt = -1;
            const walk = (n: N) => { if (anyAt < 0 && n.kind === SyntaxKind.AnyKeyword) anyAt = n.end - 3 - header.length; if (anyAt < 0) n.forEachChild?.(walk); };
            for (const st of (file as unknown as { statements: N[] }).statements) {
              if (st.end <= header.length) continue;
              walk(st);
              // NodeFlags.Const is 2; `let` and `var` are assignable by a later cell.
              if (st.kind === SyntaxKind.VariableStatement) for (const d of st.declarationList!.declarations) bind(d.name, st.declarationList!.flags & 2 ? "const" : "let");
              else if (st.kind === SyntaxKind.TypeAliasDeclaration || st.kind === SyntaxKind.InterfaceDeclaration) bind(st.name, "type");
              else if (st.kind === SyntaxKind.FunctionDeclaration || st.kind === SyntaxKind.ClassDeclaration || st.kind === SyntaxKind.EnumDeclaration) bind(st.name, "const");
              else if (st.kind === SyntaxKind.ImportDeclaration) { bind(st.importClause?.name, "const"); const nb = st.importClause?.namedBindings; bind(nb?.name ?? nb, "const"); }
            }
            return { header, kinds, anyAt, found: found as readonly Diagnostic[] };
          } finally { await snapshot.dispose().catch(() => undefined); }
        };
        let r = await pass(new Set());
        // A cell that declares a name again is checked without the earlier cell's: its own is the name from here on.
        if ([...r.kinds.keys()].some((n) => this.#scope.has(n))) r = await pass(new Set(r.kinds.keys()));
        if (r.anyAt >= 0) {
          return refusal([{ line: lineOf(source, r.anyAt), text: "`any` turns the type check off for everything it touches, which hides the mistake it was written to get past: give the real type (help('name') shows a call's), or `unknown` and narrow it" }], []);
        }
        const errors = r.found
          .filter((d) => !(UNRESOLVED.has(d.code) && !/module '\.{1,2}\//.test(d.text)))
          .map((d) => ({ line: lineOf(source, Math.max(0, d.pos - r.header.length)), text: `${[d.text, ...(d.messageChain ?? []).map((c) => c.text)].join(" ").replace(/\s+/g, " ")} (TS${d.code})` }));
        return errors.length ? refusal(errors, this.globals.known) : r.kinds;
      } catch (error) {
        await this.#stop();
        return `Not run: the type checker failed (${error instanceof Error ? error.message : String(error)}), so the cell could not be checked and nothing of it ran. The next cell starts a new checker.`;
      } finally {
        clearTimeout(timer);
      }
    }
  }

  /** `source` becomes the next cell module and its names the scope's. */
  #commit(source: string, names: readonly string[], kinds: ReadonlyMap<string, Kind>): Admitted {
    // A name the evaluator binds that is not a top-level declaration (a `var` in a block) is assignable.
    const values = names.filter((n) => kinds.get(n) !== "type");
    const types = [...kinds].filter(([, k]) => k === "type").map(([n]) => n);
    if (!values.length && !types.length) return { settle() {} };
    const cell = `c${++this.#count}`;
    const exports = [values.length ? `export { ${values.join(", ")} };` : "", types.length ? `export type { ${types.join(", ")} };` : ""].filter(Boolean).join(" ");
    const file = join(this.dir, `${cell}.ts`);
    writeFileSync(file, `${this.#header(new Set([...values, ...types]))}${source}\n${exports}\n`);
    this.#pending.created.push(file);
    const before = new Map(values.map((n) => [n, this.#scope.get(n)] as const));
    for (const n of values) this.#scope.set(n, { cell, kind: kinds.get(n) === "const" ? "const" : "let" });
    // A type does not hide an earlier cell's value of the same name: the value is what the evaluator still has.
    for (const n of types) if (!this.#scope.has(n) || this.#scope.get(n)!.kind === "type") this.#scope.set(n, { cell, kind: "type" });
    return {
      settle: (bound) => {
        for (const [n, was] of before) {
          if (bound.includes(n) || this.#scope.get(n)?.cell !== cell) continue;
          if (was) this.#scope.set(n, was); else this.#scope.delete(n);
        }
      },
    };
  }

  close(): Promise<void> {
    return this.#run(async () => {
      await this.#stop();
      rmSync(this.dir, { recursive: true, force: true });
    });
  }
}

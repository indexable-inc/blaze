// The cells' type checker against the real globals: what it refuses, how earlier cells' names are typed, and what a
// check costs warm (the bound the design rests on).
import { afterAll, expect, test } from "bun:test";
import { join } from "node:path";
import { prepare } from "./evaluator.ts";
import { docs, functions } from "./functions/index.ts";
import { TypeChecker, preludeText, type Admitted } from "./typecheck.ts";

/** A warm check must stay under this for cells to be checked every time (measured 5 to 15 ms, 2026-11-12). */
const CHECK_WARM_MS = 100;

const globals = { module: join(import.meta.dir, "functions", "index.ts"), typed: Object.keys(functions), untyped: ["Column", "__import"], known: Object.keys(docs) };
const checker = new TypeChecker(globals);
afterAll(() => checker.close());

const names = (code: string) => prepare(new Bun.Transpiler({ loader: "ts" }).transformSync(code)).names;
/** Admit a cell and settle it as having bound everything it declares (it ran to its end). */
const ran = async (code: string) => { const a = await checker.admit(code, names(code)); if ("refusal" in a) throw new Error(a.refusal); a.settle(names(code)); };
const refused = async (code: string) => { const a = await checker.admit(code, names(code)); return "refusal" in a ? a.refusal : null; };

test("the globals are declared with their real types; plumbing is not a name", () => {
  const text = preludeText(globals);
  expect(text).toContain('const glob: Globals["glob"];');
  expect(text).toContain("const Column: unknown;");
  expect(text).not.toContain("__import");
});

test("a wrong call of a kernel function is refused at its line, with where its help is", async () => {
  const r = (await refused("const files = await glob('*.ts');\nfiles.nope;\nawait globb('x')"))!;
  expect(r).toStartWith("Not run: the cell has 2 type errors. Nothing of it ran.");
  expect(r).toContain("line 2: Property 'nope' does not exist on type 'GlobResults'. (TS2339)");
  expect(r).toContain("line 3: Cannot find name 'globb'. Did you mean 'glob'? (TS2552) (help('glob'))");
  expect(await refused("await rg('x', { paht: 'src' })")).toContain("'paht' does not exist");
  expect(await refused("await rg('x', { path: 'src' })")).toBeNull();
});

test("earlier cells' names are typed: a const, an assignable let, a function, a class, a type, an import", async () => {
  await ran("const count = 3; let total = 0; function twice(n: number) { return n * 2 } class Box { v = 1 }\ntype Row = { id: number }; interface Named { name: string }\nimport { join as joinPath } from 'node:path'; const [first, { deep }] = [1, { deep: 'x' }];");
  expect(await refused("total = twice(count) + new Box().v + first; const r: Row & Named = { id: 1, name: joinPath('a', deep) }; r")).toBeNull();
  expect(await refused("const s: string = count")).toContain("line 1: Type 'number' is not assignable to type 'string'");
  expect(await refused("count = 4")).toContain("Cannot assign to 'count' because it is");
  expect(await refused("twice('two')")).toContain("Argument of type 'string' is not assignable to parameter of type 'number'");
  expect(await refused("const r: Row = { id: 'one' }")).toContain("Type 'string' is not assignable to type 'number'");
});

test("a later cell may declare a name again with another type; a name a cell never bound stays the earlier cell's", async () => {
  await ran("const shape = 1");
  await ran("const shape = 'round'");
  expect(await refused("shape.toUpperCase()")).toBeNull();
  // This cell threw before its declaration ran: the evaluator kept the older binding, and so does the checker.
  const a = (await checker.admit("const shape = [1, 2]", ["shape"])) as Admitted;
  expect(await refused("shape.length")).toBeNull();
  a.settle([]);
  expect(await refused("shape.toUpperCase()")).toBeNull();
  expect(await refused("shape.push(3)")).toContain("Property 'push' does not exist on type '\"round\"'");
});

test("what turns the check off is refused: any, @ts-ignore", async () => {
  expect(await refused("const x = JSON.parse('{}') as any;\nx.y")).toContain("line 1: `any` turns the type check off");
  expect(await refused("function f(a: number) {}\n\nconst g = (v: any) => v")).toContain("line 3: `any`");
  expect(await refused("// @ts-ignore\nglob(5)")).toContain("line 1: @ts-ignore turns the type check off");
  expect(await refused("const company = 'acme'; const many = [company]; many")).toBeNull();
});

test("an npm package a cell imports by name is installed when it runs, so its import is not an error; a missing file is", async () => {
  expect(await refused("import { z } from 'zod-not-installed-here'; z")).toBeNull();
  expect(await refused("import { x } from './no-such-file.ts'; x")).toContain("Cannot find module './no-such-file.ts'");
});

test(`a warm check takes under ${CHECK_WARM_MS} ms`, async () => {
  await ran("const warm = 1");
  const took: number[] = [];
  for (let i = 0; i < 12; i++) {
    const at = performance.now();
    await ran(`const w${i} = warm + ${i}; const files${i} = await glob('*.ts'); files${i}.length + w${i}`);
    took.push(performance.now() - at);
  }
  took.sort((a, b) => a - b);
  expect(took[6]!).toBeLessThan(CHECK_WARM_MS);
  console.log(`typecheck warm: median ${took[6]!.toFixed(1)} ms, max ${took.at(-1)!.toFixed(1)} ms over 12 cells, each seeing every earlier cell's names`);
});

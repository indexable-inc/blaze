import { expect, test } from "bun:test";
import { Scope, cellOf, cellStack, prepare } from "./evaluator.ts";

const scope = () => new Scope({ twice: (n: number) => n * 2, __import: (s: string) => import(s) });
let n = 0;
const run = (s: Scope, code: string, id = `t${++n}`) => s.run(prepare(code), id);

test("top-level names of one cell are live bindings in the next; the last expression is the value", async () => {
  const s = scope();
  expect(await run(s, "const a = 2; let b = 3; function f() { return a * b } class C { v = 7 }\nvar v = 1; twice(a)")).toBe(4);
  expect(await run(s, "b = 10; [f(), new C().v, v]")).toEqual([20, 7, 1]);
  // One binding: the first cell's closure sees the assignment the second made.
  expect(await run(s, "f()")).toBe(20);
});

test("cells are strict: a const cannot be assigned, an undeclared name cannot be made by assigning", async () => {
  const s = scope();
  await run(s, "const k = 1");
  expect(run(s, "k = 2")).rejects.toThrow(/k was declared with const by cell t\d+ and cannot be assigned/);
  expect(run(s, "nowhere = 2")).rejects.toThrow(ReferenceError);
  expect(run(s, "twice = 2")).rejects.toThrow(/twice is the kernel's/);
  expect(await run(s, "(function () { return this })()")).toBeUndefined();
});

test("a later cell may declare a name again; a cell that threw before its declaration leaves the older one", async () => {
  const s = scope();
  await run(s, "const x = 1");
  expect(run(s, "const y = (() => { throw new Error('no') })(); const x = 2")).rejects.toThrow("no");
  expect(await run(s, "x")).toBe(1);
  await run(s, "const x = 'new'");
  expect(await run(s, "x")).toBe("new");
});

test("two cells running at once share a binding as soon as it is initialized", async () => {
  const s = scope();
  const gate = Promise.withResolvers<void>();
  s.globals.wait = gate.promise;
  const slow = s.run(prepare("const early = 5; await wait; const late = 6; early + late"), "t90");
  await Promise.resolve();
  expect(await run(s, "early")).toBe(5);
  expect(run(s, "late")).rejects.toThrow(/late is declared by cell t90, which is still running/);
  gate.resolve();
  expect(await slow).toBe(11);
  expect(await run(s, "late")).toBe(6);
});

test("imports: static ones bind names and keep the cell's line numbers, dynamic ones go through __import", async () => {
  const s = scope();
  expect(await run(s, "import { join,\n  basename as base } from 'node:path';\nimport * as os from 'node:os';\nconst m = await import('node:fs');\n[join('a', 'b'), base('/x/y'), typeof os.cpus, typeof m.readFileSync]"))
    .toEqual(["a/b", "y", "function", "function"]);
  expect(prepare("import a from 'x';\nb").js.split("\n").length).toBe(2);
  expect(prepare("import('x')").js).toBe("return (__import('x'));");
});

test("an error's stack names the cell and its own line, with none of the kernel's frames", async () => {
  const s = scope();
  const e = await run(s, "const a = 1;\n\nnull.x", "t41").catch((e) => e);
  expect(cellOf(e)).toBe("t41");
  expect(cellStack(e)).toMatch(/^TypeError: .*\n\s+at .*t41\.cell\.ts:3:/);
  expect(cellStack(e)).not.toContain("evaluator.ts");
  expect(() => prepare("const = 1")).toThrow(SyntaxError);
});

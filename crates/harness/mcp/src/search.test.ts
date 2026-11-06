import { afterAll, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { install } from "./effects.ts";
import { nuon } from "./nuon.ts";
import { du, glob, ls, rg } from "./search.ts";
import { inTask, newTask } from "./tasks.ts";

install();
const dir = realpathSync(mkdtempSync(join(tmpdir(), "harness-search-")));
afterAll(() => rmSync(dir, { recursive: true, force: true }));
// dir: .gitignore (dist), a.ts, big.bin, dist/out.js, sub/c.ts
mkdirSync(join(dir, "sub"));
mkdirSync(join(dir, "dist"));
mkdirSync(join(dir, ".git"));
writeFileSync(join(dir, ".gitignore"), "dist\n");
writeFileSync(join(dir, "a.ts"), "one\nTODO two\nthree\n");
writeFileSync(join(dir, "big.bin"), "x".repeat(1000));
writeFileSync(join(dir, "dist", "out.js"), "TODO\n");
writeFileSync(join(dir, "sub", "c.ts"), "TODO\n");

test("rg finds hits with context, and prints its rows", async () => {
  const hits = await rg("TODO", { path: dir, type: "ts", context: 1 });
  expect(hits.count).toBe(2);
  expect([...hits.files]).toEqual([join(dir, "a.ts"), join(dir, "sub", "c.ts")]);
  expect(hits[0]).toEqual({ path: join(dir, "a.ts"), line: 2, column: 1, text: "TODO two", before: ["one"], after: ["three"], submatches: [{ start: 0, end: 4, text: "TODO" }] });
  expect(hits.truncated).toBe(false);
  expect(nuon(hits)).toContain("TODO two");
  expect(await rg("absent", { path: dir })).toHaveLength(0);
  await expect(rg("(", { path: dir })).rejects.toThrow("regex parse error");
});

test("glob with stat and sort size gives the largest first", async () => {
  const rows = await glob("**/*", { cwd: dir, stat: true, sort: "size", limit: 2 });
  expect(rows.map((r) => [relative(dir, r.path), r.isDir, r.ext])).toEqual([["big.bin", false, "bin"], ["a.ts", false, "ts"]]);
  expect(rows[0]!.size).toBe(1000);
  expect(rows.truncated).toBe(true);
  expect([...(await glob("**/*.ts", { cwd: dir }))]).toEqual([join(dir, "a.ts"), join(dir, "sub", "c.ts")]);
});

test("ls with gitignore leaves out what .gitignore does", async () => {
  const names = (es: { path: string; kind: string }[]) => es.map((e) => relative(dir, e.path) + (e.kind === "dir" ? "/" : ""));
  expect(names(await ls(dir))).toEqual(["dist/", "sub/", "a.ts", "big.bin"]);
  expect(names(await ls(dir, { gitignore: true, recursive: true }))).toEqual(["sub/", "sub/c.ts", "a.ts", "big.bin"]);
});

test("du totals every file, largest first", async () => {
  const use = await du(dir);
  expect(use.total).toBe(1000 + 19 + 5 + 5 + 5);
  expect(use.files).toBe(5);
  expect(relative(dir, use[0]!.path)).toBe("big.bin");
  expect(use.find((e) => e.path === join(dir, "sub"))).toEqual({ path: join(dir, "sub"), size: 5, files: 1, isDir: true });
});

test("an option that does not exist is refused, naming the ones there are", async () => {
  await expect(glob("*", { cwd: dir, hidden: true } as never)).rejects.toThrow('glob has no option "hidden": use dot. Its options: cwd, dot,');
  await expect(rg("x", { cwd: dir } as never)).rejects.toThrow('rg has no option "cwd": use path');
  await expect(ls(dir, { dot: true } as never)).rejects.toThrow('ls has no option "dot": use hidden');
  await expect(du(dir, { human: true } as never)).rejects.toThrow('du has no option "human"');
});

test("in a cell each call is one step of its kind, never an exec", async () => {
  const cases = [
    ["search", `Searched for TODO`, "2 hits in 2 files", () => rg("TODO", { path: dir, type: "ts" })],
    ["list", `Listed ${dir}`, "4 entries", () => ls(dir)],
    ["list", "Listed **/*.ts", "2 files", () => glob("**/*.ts", { cwd: dir })],
    ["list", `Measured ${dir}`, "1.0 KB", () => du(dir)],
  ] as const;
  for (const [kind, done, detail, call] of cases) {
    const t = newTask("x");
    await inTask(t, (): Promise<unknown> => call());
    expect(t.effects.map((e) => [e.kind, e.done, e.detail, e.status])).toEqual([[kind, done, detail, "ok"]]);
    expect(t.effects[0]!.value).not.toBeNull();
  }
});

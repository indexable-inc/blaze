// The docs are held to the real exports, and the tool's description to its budget.
import { expect, test } from "bun:test";
import { AREAS } from "../docs.ts";
import { description } from "../help.ts";
import { Repl } from "../repl.ts";
import { docs, functions, help } from "./index.ts";

/** Bytes of the tool description every conversation pays for. Before help() it was 25,354 (2026-11-12). */
const DESCRIPTION_BUDGET = 2048;

/** What a cell really has in scope: the functions and what the kernel installs beside them (the UI's). */
const scope = new Repl(functions).scope.globals;
const resolve = (name: string): unknown => name.split(".").reduce<unknown>((o, k) => (o == null ? undefined : (o as Record<string, unknown>)[k]), { ...globalThis, Bun, ...scope });
const isNamespace = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && Object.getPrototypeOf(v) === Object.prototype;

test("every function a cell gets has a doc", () => {
  const missing = Object.keys(functions).filter((n) => !(n in docs));
  expect(missing).toEqual([]);
});

test("every doc is of a real export, in a real area, and says what it takes and does", () => {
  for (const [name, d] of Object.entries(docs)) {
    expect(resolve(name), `docs has ${name}, which nothing exports`).toBeDefined();
    expect(Object.keys(AREAS), name).toContain(d.area);
    expect(d.sig.length, name).toBeGreaterThan(0);
    expect(d.about.length, name).toBeGreaterThan(0);
  }
  for (const area of Object.keys(AREAS)) expect(Object.values(docs).some((d) => d.area === area), `area ${area} has no call`).toBe(true);
});

test("every function of a namespace (cargo.test, vfs.write) is in its doc, or has its own", () => {
  const unsaid: string[] = [];
  for (const [ns, v] of Object.entries(functions)) {
    if (!isNamespace(v)) continue;
    const text = `${docs[ns]!.sig} ${docs[ns]!.about}`;
    for (const [member, f] of Object.entries(v)) {
      if (typeof f === "function" && !(`${ns}.${member}` in docs) && !new RegExp(`\\b${member}\\b`).test(text)) unsaid.push(`${ns}.${member}`);
    }
  }
  expect(unsaid).toEqual([]);
});

test("the description fits its budget and names every way to find the rest", () => {
  const d = description();
  expect(Buffer.byteLength(d)).toBeLessThanOrEqual(DESCRIPTION_BUDGET);
  for (const word of ["help()", "help.search(", "expect_ms", "cancel(", "more(", "type-checked"]) expect(d).toContain(word);
  // It lists no call's signature: that is help's.
  expect(d).not.toMatch(/Promise<|\{ [a-z]+\?/);
});

test("help lists the areas, an area's calls, a call in full, and finds by words", () => {
  const top = help();
  for (const area of Object.keys(AREAS)) expect(top).toContain(`\n${area} `);
  expect(help("files")).toMatch(/^files: .*\nBun\.file\s/m);
  expect(help("glob")).toMatch(/^glob\(pattern.*\n[^]*Example: await glob\([^]*Area: files \(help\('files'\)\)$/);
  expect(help("sh")).toContain("Throws: ");
  // A namespace's entry covers its members; a member that does not exist is not help's to invent.
  expect(help("cargo.test")).toContain("cargo.check | clippy | build | test");
  expect(() => help("cargo.tset")).toThrow(/nothing is called "cargo.tset"/);
  expect(() => help("globb")).toThrow(/Nearest: glob/);
  expect(help.search("largest files").split("\n")[0]).toMatch(/^(glob|du)\s/);
  expect(help.search("stop a running cell")).toContain("cancel");
  expect(help.search("zzzz qqqq")).toStartWith("Nothing matches");
});

import { expect, test } from "bun:test";
import { BUDGET, present, presentLogs, select } from "./disclose.ts";

/** A small deterministic generator, so a failure names its seed. */
const rng = (seed: number) => () => (seed = (seed * 48271) % 2147483647) / 2147483647;

function value(r: () => number, depth = 0): unknown {
  const pick = r();
  if (depth > 3 || pick < 0.2) return r() < 0.5 ? Math.round(r() * 1e6) : "s".repeat(Math.floor(r() * (r() < 0.1 ? 50_000 : 40)));
  if (pick < 0.6) return Array.from({ length: Math.floor(r() * (r() < 0.2 ? 3000 : 12)) }, () => value(r, depth + 1));
  return Object.fromEntries(Array.from({ length: Math.floor(r() * 8) }, (_, i) => [`k${i}`, value(r, depth + 1)]));
}

test("whatever the value, what is printed fits the budget, and what was left out is said with the call that gives it", () => {
  for (let seed = 1; seed <= 300; seed++) {
    const v = value(rng(seed));
    const p = present(v, { id: "t7" });
    // The notes that say how to get more are outside the value's budget: three lines at most.
    expect(p.text.length, `seed ${seed}`).toBeLessThanOrEqual(BUDGET + 600);
    if (p.cut) expect(p.text, `seed ${seed}`).toContain("more('t7', {");
    else expect(p.text, `seed ${seed}`).not.toContain("more(");
  }
});

test("a small value prints whole; a long list says its rows; a long string keeps its head and tail", () => {
  expect(present({ a: 1, b: ["x", "y"] })).toEqual({ text: "{a: 1, b: [x, y]}", cut: false });
  expect(present(undefined).text).toBe("undefined");
  expect(present("plain text").text).toBe("plain text");
  const rows = present(Array.from({ length: 250 }, (_, i) => ({ i })), { id: "t3" });
  expect(rows.text).toEndWith("# … 150 more rows (250 total): more('t3', { rows: [100, 200] })");
  const long = present("a".repeat(30_000) + "END", { id: "t3" });
  expect(long.text).toEndWith("END");
  expect(long.text).toContain(`more('t3', { chars: [12288, 28672] })`);
  expect(present({ body: "x".repeat(5000) }, { id: "t3" }).text).toContain("long strings inside it are shortened: more('t3', { path: 'field[0].name' })");
});

test("more's selection: a path, rows, characters, fields; a wrong step says what is there", () => {
  const v = { items: [{ name: "a", size: 1, body: "hello world" }, { name: "b", size: 2, body: "" }], "odd key": new Map([["k", 5]]) };
  expect(select(v, { path: "items[1].name" })).toBe("b");
  expect(select(v, { path: "items", rows: [1, 2], fields: ["name"] })).toEqual([{ name: "b" }]);
  expect(select(v, { path: "items[0].body", chars: [6, 11] })).toBe("world");
  expect(select(v, { path: "['odd key'].k" })).toBe(5);
  expect(() => select(v, { path: "items[5]" })).toThrow("items has no 5: it is a list of 2");
  expect(() => select(v, { path: "itmes" })).toThrow('the value has no "itmes": it is {items, odd key}');
  expect(() => select(v, { path: "items", chars: [0, 1] })).toThrow("chars is for a string; items is a list (use rows)");
  expect(() => select(v, { row: [0, 1] } as never)).toThrow("unknown option row");
});

test("a cell's console lines: all within the budget, else the first and the last with how to read the rest", () => {
  expect(presentLogs(["a", "b"])).toBe("a\nb");
  const many = presentLogs(Array.from({ length: 5000 }, (_, i) => `line ${i}`), { id: "t9", budget: 500 });
  expect(many.length).toBeLessThan(700);
  expect(many).toStartWith("line 0\nline 1\n");
  expect(many).toEndWith("line 4999");
  expect(many).toMatch(/# … \d+ log lines left out \(5000 in all\): task\('t9'\)\.logs\.slice\(5, \d+\)/);
});

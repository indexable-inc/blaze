// Run cells through the kernel's API from a shell: one JSON value, `-` for stdin.
//   bun scripts/cell.ts '{"cells": ["1 + 1", "help()"]}'               a kernel of its own, for these cells
//   bun scripts/cell.ts '{"sock": "/path/kernel.sock", "cells": [...]}'   a running kernel (one started with HARNESS_REPL_SOCK)
// A cell is its code, or a whole request: {"op": "run", "code": "...", "expect_ms": 2000} or {"op": "cancel", "task": "t3"}.
import { parseCall, type Answer, type Call } from "../src/kernel.ts";
import { call } from "../src/serve.ts";

type Input = { sock?: string; cells: (string | Call)[] };

function parse(text: string): Input {
  const v = JSON.parse(text) as Record<string, unknown>;
  const unknown = Object.keys(v).filter((k) => k !== "sock" && k !== "cells");
  if (unknown.length) throw new Error(`unknown field ${unknown.join(", ")}: the input is { sock?, cells }`);
  if (!Array.isArray(v.cells) || !v.cells.length) throw new Error("`cells` is a list of at least one cell (its code, or a request)");
  if (v.sock !== undefined && typeof v.sock !== "string") throw new Error("`sock` is the path of a running kernel's socket");
  return Object.freeze({ sock: v.sock, cells: v.cells }) as Input;
}

const arg = Bun.argv[2];
if (!arg) throw new Error(`usage: bun scripts/cell.ts '{"cells": ["1 + 1"]}'   (or - to read the JSON from stdin)`);
const input = parse(arg === "-" ? await Bun.stdin.text() : arg);
let ask: (c: Call) => Promise<Answer>;
if (input.sock) ask = (c) => call(input.sock!, c);
else {
  const kernel = (await import("../src/start.ts")).start({ journal: null });
  ask = (c) => kernel.handle(c);
}
let failed = false;
for (const cell of input.cells) {
  const request = typeof cell === "string" ? { op: "run" as const, code: cell } : cell;
  const bad = parseCall(request);
  if (typeof bad === "string") throw new Error(bad);
  const a = await ask(request);
  failed ||= a.error;
  console.info(`> ${typeof cell === "string" ? cell : JSON.stringify(cell)}\n${a.text}\n`);
}
process.exit(failed ? 1 : 0);

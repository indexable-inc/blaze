// A cell's first ui tree, as the harness app would be sent it: the cell runs through the same compile and checks as the MCP ts
// tool's (repl.ts), with no harnessd (a socket path that is never there) and no live ~/.harness, and its tree is written as JSON.
// bun scripts/tree.ts <cell.svelte> <out.json>
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const [cell, out] = process.argv.slice(2);
if (!cell || !out) throw new Error("usage: bun scripts/tree.ts <cell.svelte> <out.json>");
const dir = mkdtempSync(`${tmpdir()}/genui-tree-`);
process.env.HARNESS_SOCK = `${dir}/harnessd.sock`; // never created: the cell's posts go nowhere
process.env.HARNESS_HOME = dir;
try {
  const { Repl } = await import("../src/repl.ts");
  const t = await new Repl({}).run(await Bun.file(cell).text()).done;
  if (t.status !== "done") throw new Error(`${cell} failed:\n${t.result}`);
  if (t.ui === undefined) throw new Error(`${cell} drew nothing: its value was ${t.result}`);
  await Bun.write(out, `${JSON.stringify(t.ui, null, 2)}\n`);
  console.log(`${out}: ${t.result}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
process.exit(0);

// The REPL core end to end through Lanes (lanes.ts): real evaluator processes and the real type checker, no harnessd.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Lanes, type Cell } from "./lanes.ts";

const dir = mkdtempSync(join(tmpdir(), "repl-lanes-"));
// No harnessd: the evaluators must not post cells to a real one.
process.env.HARNESS_SOCK = join(dir, "no-daemon.sock");
const opened: Lanes[] = [];
const open = (stateDir = join(dir, "state")) => {
  const lanes = new Lanes({ stateDir, confirm: async () => false });
  opened.push(lanes);
  return lanes;
};
afterAll(async () => {
  for (const lanes of opened) await lanes.close();
  rmSync(dir, { recursive: true, force: true });
});

async function run(lanes: Lanes, code: string, lane = "main"): Promise<Cell | { refusal: string }> {
  return lanes.run(lanes.lane(lane), code, null, []);
}
async function value(lanes: Lanes, code: string, lane = "main"): Promise<string> {
  const c = await run(lanes, code, lane);
  if ("refusal" in c) throw new Error(c.refusal);
  await c.done;
  if (!c.ok) throw new Error(c.text);
  return c.text;
}

let lanes: Lanes;
beforeAll(() => { lanes = open(join(dir, "a")); });

test("a type error is refused naming its line, and so is an escape", async () => {
  await value(lanes, "let n = 5");
  const typed = await run(lanes, "const ok = 1\nconst y: string = n");
  expect("refusal" in typed && typed.refusal).toContain("line 2: Type 'number' is not assignable to type 'string'");
  const any = await run(lanes, "const z = JSON.parse('1') as any");
  expect("refusal" in any && any.refusal).toContain("as any");
  // A cell that was refused did not run.
  expect(await value(lanes, "typeof ok")).toBe("undefined");
}, 60_000);

test("parse<T> checks unknown data, naming every field that does not fit", async () => {
  const c = await run(lanes, `parse<{ a: number; b: string }>(JSON.parse('{"a":"x"}'))`);
  if ("refusal" in c) throw new Error(c.refusal);
  await c.done;
  expect(c.ok).toBe(false);
  expect(c.text).toContain("a: expected number, got string");
  expect(c.text).toContain("b: expected string, got nothing");
  expect(c.text).not.toContain("shape.ts");
  expect(await value(lanes, `parse<{ a: number }>(JSON.parse('{"a":2}')).a + 1`)).toBe("3");
}, 60_000);

test("every global a cell gets is typed by CellGlobals", async () => {
  const names = lanes.names()!;
  expect(names.length).toBeGreaterThan(20);
  const globals = fileURLToPath(new URL("./globals.ts", import.meta.url));
  const c = await run(lanes, `const keys: (keyof import(${JSON.stringify(globals)}).CellGlobals)[] = ${JSON.stringify(names)}; keys.length`);
  expect("refusal" in c ? c.refusal : "").toBe("");
}, 60_000);

test("lanes run in parallel; cancel stops one and clears only its bindings", async () => {
  await value(lanes, "let kept = 1", "slow");
  const started = Date.now();
  const a = await run(lanes, "await new Promise(r => setTimeout(r, 1500)); 'a'", "p1");
  const b = await run(lanes, "await new Promise(r => setTimeout(r, 1500)); 'b'", "p2");
  if ("refusal" in a || "refusal" in b) throw new Error("refused");
  await Promise.all([a.done, b.done]);
  expect(Date.now() - started).toBeLessThan(2900);
  const hang = await run(lanes, "await new Promise(() => {})", "slow");
  if ("refusal" in hang) throw new Error(hang.refusal);
  const stopped = await lanes.kill(lanes.lane("slow"), "cancelled");
  expect(stopped.map((c) => c.id)).toEqual([hang.id]);
  await hang.done;
  expect(hang.ok).toBe(false);
  // The checker forgot the lane's names with its evaluator, so the cell is refused rather than failing at run time.
  const after = await run(lanes, "kept", "slow");
  expect("refusal" in after && after.refusal).toContain("Cannot find name 'kept'");
  expect(await value(lanes, "n")).toBe("5");
}, 60_000);

test("a lane restores what it saved, byte for byte, and names what it dropped", async () => {
  const stateDir = join(dir, "persist");
  const first = open(stateDir);
  await value(first, "const xs = [1, 2, 3]; let m = new Map([[1, { d: new Date(0) }]]); function f() { return 1 }; type Row = { id: number }");
  await value(first, "const rows: Row[] = [{ id: 1 }]; let cyc: { self?: unknown } = {}; cyc.self = cyc");
  const saved = await first.save(first.lane("main"));
  expect(saved?.saved).toEqual(["xs", "m", "rows", "cyc"]);
  expect(saved?.dropped).toEqual(["f"]);
  const fresh = readFileSync(join(stateDir, "main.state"));
  await first.close();

  const second = open(stateDir);
  const lane = second.lane("main");
  await second.start(lane);
  expect(lane.notes.join(" ")).toContain("Restored lane main from before the restart: xs, m, rows, cyc.");
  expect(lane.notes.join(" ")).toContain("Not restored (functions, class instances and handles do not survive a restart; run their cells again): f.");
  // The values and their types are back: a typed use checks, and a wrong one is refused.
  expect(await value(second, "[xs.length, m.get(1)?.d.getTime(), rows[0]?.id, cyc.self === cyc]")).toBe("[3, 0, 1, true]");
  expect("refusal" in (await run(second, "const s: string = rows[0]!.id"))).toBe(true);
  // Restored and saved again with nothing new run: the same bytes as the fresh lane wrote.
  await second.save(lane);
  expect(Buffer.compare(readFileSync(join(stateDir, "main.state")), fresh)).toBe(0);
}, 60_000);

test("a state file that cannot be read is reported and moved aside, never silently empty", async () => {
  const stateDir = join(dir, "corrupt");
  const lanes = open(stateDir);
  await value(lanes, "let a = 1");
  await lanes.save(lanes.lane("main"));
  await lanes.close();
  writeFileSync(join(stateDir, "main.state"), readFileSync(join(stateDir, "main.state")).subarray(0, 20));
  const again = open(stateDir);
  const lane = again.lane("main");
  await again.start(lane);
  expect(lane.notes.join(" ")).toContain("could not be restored");
  expect(existsSync(join(stateDir, "main.state.bad"))).toBe(true);
}, 60_000);

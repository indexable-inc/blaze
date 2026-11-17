// The journal's invariant: replaying what was appended gives back every task as it stood, whatever prefix of the bytes
// survived (a torn tail is dropped and counted, never read); a restore's rewrite is byte for byte what a fresh journal of
// the same tasks is; a file of another layout is refused by name, never read as empty.
import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Appender, KEEP, LAYOUT, LOG_MAX, compact, entryBytes, replay, type TaskRecord } from "./journal.ts";
import { openJournal } from "./journal-file.ts";
import { allTasks, newTask, resetTasks, task, tasks } from "./tasks.ts";

const rng = (seed: number) => () => (seed = (seed * 48271) % 2147483647) / 2147483647;
const K = { pid: 4242, repl: "repl-test", at_ms: 1 };
const join2 = (parts: Uint8Array[]) => { const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0)); let at = 0; for (const p of parts) { out.set(p, at); at += p.length; } return out; };

/** A random history of cells, as the bytes a kernel appends and the records they should give, after each append. */
function history(seed: number) {
  const r = rng(seed);
  const a = new Appender();
  const model: TaskRecord[] = [];
  const parts: Uint8Array[] = [compact([], K)];
  const states: { end: number; tasks: TaskRecord[] }[] = [{ end: parts[0]!.length, tasks: [] }];
  const push = (b: Uint8Array | null) => { if (!b) return; parts.push(b); states.push({ end: states.at(-1)!.end + b.length, tasks: structuredClone(model) }); };
  for (let step = 0; step < 40; step++) {
    const running = model.filter((t) => t.status === "running");
    const pick = r();
    if (pick < 0.3 || !running.length) {
      const t: TaskRecord = { id: `t${model.length + 1}`, status: "running", code: `cell ${step}\n"é ☃"`, call: r() < 0.5 ? `toolu_${step}` : null, started_ms: step * 10, ended_ms: null, expect_ms: r() < 0.5 ? 1000 : null, why: null, logs: [], result: "" };
      model.push(t);
      push(a.start(t));
    } else if (pick < 0.7) {
      const t = running[Math.floor(r() * running.length)]!;
      const line = `line ${step} ${"x".repeat(Math.floor(r() * 30))}`;
      t.logs.push(line);
      push(a.log(t.id, line));
    } else {
      const t = running[Math.floor(r() * running.length)]!;
      Object.assign(t, { status: r() < 0.7 ? "done" : "error", result: `result ${step}`, ended_ms: step * 10 + 5 });
      push(a.end(t));
    }
  }
  return { bytes: join2(parts), states, model };
}

test("replay gives back every task, and any cut of the bytes gives the tasks as they stood at the last whole entry", () => {
  for (let seed = 1; seed <= 40; seed++) {
    const { bytes, states, model } = history(seed);
    expect(replay(bytes)).toEqual({ tasks: model, kernel: { pid: K.pid, repl: K.repl }, torn: 0, refused: null });
    const r = rng(seed + 1000);
    for (let i = 0; i < 25; i++) {
      const cut = states[0]!.end + Math.floor(r() * (bytes.length - states[0]!.end));
      const at = states.findLast((s) => s.end <= cut)!;
      const got = replay(bytes.subarray(0, cut));
      expect(got.refused, `seed ${seed} cut ${cut}`).toBeNull();
      expect(got.tasks, `seed ${seed} cut ${cut}`).toEqual(at.tasks);
      expect(got.torn, `seed ${seed} cut ${cut}`).toBe(cut - at.end);
    }
  }
});

test("a restore's rewrite is byte for byte a fresh journal of the same tasks, and keeps the newest KEEP", () => {
  const { bytes, model } = history(7);
  expect(compact(replay(bytes).tasks, K)).toEqual(compact(model, K));
  const many = Array.from({ length: KEEP + 30 }, (_, i): TaskRecord => ({ id: `t${i + 1}`, status: "done", code: "1", call: null, started_ms: i, ended_ms: i, expect_ms: null, why: null, logs: [], result: "1" }));
  const kept = replay(compact(many, K)).tasks;
  expect(kept).toHaveLength(KEEP);
  expect(kept.at(-1)!.id).toBe(`t${KEEP + 30}`);
});

test("a file of another layout, or that is not a journal, is refused by name; garbage is torn, not a throw", () => {
  const old = join2([entryBytes({ e: "journal", layout: LAYOUT - 1 }), entryBytes({ e: "kernel", ...K })]);
  expect(replay(old).refused).toBe(`it has layout ${LAYOUT - 1}, and this kernel reads layout ${LAYOUT}`);
  expect(replay(entryBytes({ e: "kernel", ...K })).refused).toBe("it does not begin as a task journal");
  expect(replay(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9]))).toMatchObject({ tasks: [], torn: 9 });
  expect(replay(new Uint8Array(0))).toMatchObject({ tasks: [], torn: 0, refused: null });
});

test("a cell's console lines are journaled up to a bound, which the journal says it reached", () => {
  const a = new Appender();
  const parts = [compact([], K), a.start({ id: "t1", status: "running", code: "", call: null, started_ms: 0, ended_ms: null, expect_ms: null, why: null, logs: [], result: "" })];
  for (let i = 0; i < 400; i++) { const b = a.log("t1", "y".repeat(1000)); if (b) parts.push(b); }
  const logs = replay(join2(parts)).tasks[0]!.logs;
  expect(logs.join("").length).toBeLessThan(LOG_MAX + 2000);
  expect(logs.at(-1)).toContain("later lines of t1 are not in it");
});

test("the file: a new kernel has an earlier one's tasks back, a running one as lost; another layout is set aside and said", async () => {
  const dir = mkdtempSync(join(tmpdir(), "journal-"));
  const path = join(dir, "s7.log");
  resetTasks();
  expect(openJournal(path)).toBeNull();
  const a = newTask("1 + 1");
  a.log("a line");
  a.finish("done", 2, "2");
  newTask("await forever()");
  const before = allTasks().map((t) => t.record());
  // The kernel is gone: only the file is left. The next one reads it (the last kernel's pid is this process, so it is not "another live kernel").
  resetTasks();
  const note = openJournal(path)!;
  expect(note).toContain("Its 2 cells are still listed");
  expect(note).toContain("t2 was running when it ended");
  expect(task("t1")).toMatchObject({ status: "done", logs: ["a line"], result: "2", code: before[0]!.code });
  expect(task("t2").status).toBe("lost");
  expect(tasks().length).toBe(2);
  expect(newTask("3").id).toBe("t3");
  // What it rewrote the file to holds exactly those tasks.
  expect(replay(new Uint8Array(readFileSync(path))).tasks.map((t) => [t.id, t.status])).toEqual([["t1", "done"], ["t2", "lost"], ["t3", "running"]]);
  resetTasks();
  writeFileSync(path, join2([entryBytes({ e: "journal", layout: 0 })]));
  expect(openJournal(path)).toMatch(/the task journal was not read \(it has layout 0, and this kernel reads layout 1\).*kept at .*s7\.log\.unread-/);
  expect(tasks().length).toBe(0);
  resetTasks();
});

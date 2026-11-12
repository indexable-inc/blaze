// The kernel over time, in process: a cell cancelled in the middle of each kind of wait stops at once and leaves no
// process behind; the API refuses a bad request by field; an error nobody awaited is given to the right cell.
import { afterAll, expect, test } from "bun:test";
import { functions } from "./functions/index.ts";
import { Kernel, blame, parseCall } from "./kernel.ts";
import { stopText } from "./stop.ts";
import { getTask, newTask } from "./tasks.ts";

const pids: number[] = [];
let aborted = 0;
const server = Bun.serve({ port: 0, fetch: (req) => new Promise<Response>(() => { req.signal.addEventListener("abort", () => aborted++); }) });
const kernel = new Kernel({ ...functions, pids, hang: `http://127.0.0.1:${server.port}/` });
afterAll(() => server.stop(true));

const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };
const group = (pid: number) => Bun.spawnSync(["ps", "-o", "pid=", "-g", String(pid)]).stdout.toString().trim();

/** Run `code`, let it reach its wait, cancel it: the cancel's reply and how long the stop took. */
async function cancelled(code: string) {
  const bg = await kernel.handle({ op: "run", code, yield_ms: 70 });
  expect(bg.status, code).toBe("running");
  const at = performance.now();
  const stop = await kernel.handle({ op: "cancel", task: bg.task! });
  return { id: bg.task!, text: stop.text, ms: performance.now() - at, task: getTask(bg.task!) };
}

const WAITS: [string, string][] = [
  ["a sleep", "await Bun.sleep(60_000)"],
  ["a fetch", "await fetch(hang)"],
  ["a process", "const p = sh('sleep 61'); await p.started_on; pids.push(p.pid); await p"],
  ["a $ command", "await $`sleep 62`"],
  ["a followed command", "await follow(['sh', '-c', 'echo $$ > /dev/null; sleep 63'])"],
  ["a process with children of its own", "const p = Bun.spawn(['sh', '-c', 'sleep 64 & sleep 64']); pids.push(p.pid); await p.exited"],
  ["parallel work", "await pmap([1, 2, 3], () => Bun.sleep(60_000))"],
  ["a question to the person", "await ask('Go on?', { options: ['Yes', 'No'] })"],
  ["a loop that checks its signal", "for (;;) { signal().throwIfAborted(); await new Promise((r) => setTimeout(r, 5)) }"],
];

for (const [what, code] of WAITS) {
  test(`cancelled in ${what}: the cell stops at once and nothing of it is left`, async () => {
    const before = pids.length;
    const r = await cancelled(code);
    expect(r.task.status).toBe("stopped");
    expect(r.text).toContain(`${r.id} stopped: cancelled.`);
    expect(r.text).not.toContain("ignores its signal");
    expect(r.task.owned.size).toBe(0);
    // Nothing waits out a grace period: what a cell waits in ends with its signal.
    expect(r.ms).toBeLessThan(1000);
    for (const pid of pids.slice(before)) {
      expect(alive(pid), `pid ${pid}`).toBe(false);
      expect(group(pid), `the process group of ${pid}`).toBe("");
      expect(r.text).toContain("Killed: ");
    }
  });
}

test("a cancelled fetch is dropped at the server, not just no longer awaited", () => expect(aborted).toBeGreaterThan(0));

test("waiting on a cell is cancelled without stopping the cell waited on; a cell cannot cancel itself", async () => {
  const slow = await kernel.handle({ op: "run", code: "await Bun.sleep(700); 'slow done'", yield_ms: 20 });
  const waiter = await cancelled(`await wait('${slow.task}')`);
  expect(waiter.task.status).toBe("stopped");
  expect(getTask(slow.task!).status).toBe("running");
  expect((await getTask(slow.task!).done).result).toBe("slow done");
  const self = await kernel.handle({ op: "run", code: "const me = tasks().at(-1)!.id; await cancel(me)" });
  expect(self.text).toMatch(/t\d+ cannot cancel itself/);
});

test("a cell that ends by itself leaves its process running; the same cell stopped takes it along", async () => {
  const ok = await kernel.handle({ op: "run", code: "const kept = sh('sleep 65'); await kept.started_on; pids.push(kept.pid); 'started'" });
  expect(ok.status).toBe("done");
  const pid = pids.at(-1)!;
  expect(alive(pid)).toBe(true);
  await kernel.handle({ op: "run", code: "kill(kept); await kept" });
  expect(alive(pid)).toBe(false);
  expect((await kernel.handle({ op: "run", code: "sh('sleep 1', { detach: true })" })).text).toContain("a detached command is held by harnessd");
});

test("a stop's report says what was killed, what would not die, what was left, and code that ignores its signal", () => {
  expect(stopText({ id: "t3", why: "cancelled", killed: ["pid 7 (cargo build)", "r12 (bun test)"], survived: [], left: [], ignoring: false }))
    .toBe("t3 stopped: cancelled.\nKilled: pid 7 (cargo build); r12 (bun test).");
  const all = stopText({ id: "t3", why: "it passed its limit of 2.00s", killed: [], survived: ["pid 9 (zombie)"], left: ["r13 (bun run dev)"], ignoring: true });
  expect(all).toContain("It had no process running.");
  expect(all).toContain("Did not exit after SIGKILL (still running): pid 9 (zombie).");
  expect(all).toContain("Left running, detached on purpose: r13 (bun run dev)");
  expect(all).toContain("it ignores its signal");
});

test("a request is refused by field, in the API, before anything runs", async () => {
  expect(parseCall({ op: "run", code: "1" })).toEqual({ op: "run", code: "1" });
  expect(parseCall({ op: "run" })).toBe("run needs `code`");
  expect(parseCall({ op: "run", code: "1", yeild_ms: 5 })).toBe("run has no field `yeild_ms`; it takes code, yield_ms, timeout_ms, expect_ms, why, call");
  expect(parseCall({ op: "run", code: "1", expect_ms: "soon" })).toBe("run.expect_ms is string, and must be a number");
  expect(parseCall({ op: "launch" })).toContain('unknown op "launch"');
  expect(parseCall(null)).toBe("a request is a record with an `op`");
  expect(await kernel.handle({ op: "cancel", task: "t9999" })).toMatchObject({ error: true, text: expect.stringContaining("no task t9999") });
});

test("an error nobody awaited goes to the cell its stack names, else the cell in whose context it rose, else the cells running", () => {
  const a = newTask("a"), b = newTask("b");
  const fromA = Object.assign(new Error("x"), { stack: `Error: x\n    at <anonymous> (${a.id}.cell.ts:3:1)` });
  expect(blame(fromA, b, [a, b])).toEqual({ tasks: [a], sure: true });
  expect(blame(new Error("y"), b, [a, b])).toEqual({ tasks: [b], sure: true });
  expect(blame(new Error("y"), undefined, [a, b])).toEqual({ tasks: [a, b], sure: false });
  a.finish("done", 1, "1");
  expect(blame(new Error("y"), undefined, [a, b])).toEqual({ tasks: [b], sure: true });
  b.finish("done", 1, "1");
  expect(blame(new Error("y"), undefined, [a, b])).toEqual({ tasks: [b], sure: false });
  expect(blame(new Error("y"), undefined, [])).toEqual({ tasks: [], sure: false });
});

test("a cell that never awaits cannot be stopped from inside: past its limit the watchdog ends the kernel, and the journal says which cell and why", async () => {
  const { mkdtempSync, readFileSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { replay } = await import("./journal.ts");
  const journal = join(mkdtempSync(join(tmpdir(), "watchdog-")), "s1.log");
  const script = `import { start } from ${JSON.stringify(join(import.meta.dir, "start.ts"))};
    const k = start({ journal: ${JSON.stringify(journal)}, watchdog_ms: 300, typecheck: false });
    await k.handle({ op: "run", code: "await Bun.sleep(300); 1 + 1" });
    await k.handle({ op: "run", code: "for (;;) {}", timeout_ms: 100 });`;
  const kernelProcess = Bun.spawn([process.execPath, "-e", script], { stdout: "ignore", stderr: "ignore", env: { ...process.env } });
  const at = performance.now();
  await kernelProcess.exited;
  expect(kernelProcess.signalCode).toBe("SIGKILL");
  expect(performance.now() - at).toBeLessThan(3000);
  const tasks = replay(new Uint8Array(readFileSync(journal))).tasks;
  expect(tasks.map((t) => [t.id, t.status])).toEqual([["t1", "done"], ["t2", "stopped"]]);
  expect(tasks[1]!.result).toStartWith("t2 stopped: it passed its limit while holding the kernel's only thread for 0.");
  expect(tasks[1]!.result).toContain("The kernel was ended to end it");
});

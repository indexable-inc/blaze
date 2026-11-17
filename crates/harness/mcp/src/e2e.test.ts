// The real kernel, end to end, through both of its adapters: an MCP client on stdio (how agents connect) and the
// kernel's own socket (serve.ts). One kernel process for the file; each test is a case an agent meets.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { call as socketCall } from "./serve.ts";

const home = process.env.HARNESS_HOME!;
const sock = join(home, "kernel.sock");
const env = { ...process.env, HARNESS_HOME: home, HARNESS_REPL_SOCK: sock } as Record<string, string>;
let client: Client;
const connect = async () => {
  const c = new Client({ name: "e2e", version: "0" });
  await c.connect(new StdioClientTransport({ command: process.execPath, args: ["run", join(import.meta.dir, "mcp.ts")], env, stderr: "ignore" }));
  return c;
};
type Reply = { text: string; error: boolean };
const ts = async (args: Record<string, unknown>, c = client): Promise<Reply> => {
  const r = (await c.callTool({ name: "ts", arguments: args })) as { isError?: boolean; content: { text: string }[] };
  return { text: r.content[0]!.text, error: r.isError === true };
};

beforeAll(async () => { client = await connect(); });
afterAll(async () => { await client.close(); });

test("the description is short and points at help; help discloses the rest", async () => {
  const { tools } = await client.listTools();
  expect(tools).toHaveLength(1);
  expect(tools[0]!.description!.length).toBeLessThan(2500);
  expect(tools[0]!.description).toContain("help()");
  const areas = await ts({ code: "help()" });
  expect(areas.text).toMatch(/^\[t\d+ done [\d.]+s\]\nhelp\('area'\)/);
  expect(areas.text).toContain("files    ");
  expect((await ts({ code: "help('cargo.test')" })).text).toContain("cargo.check | clippy | build | test");
  expect((await ts({ code: "help.search('largest files')" })).text).toContain("glob");
});

test("names persist across cells, typed; a type error or an unknown name is refused before anything runs", async () => {
  expect((await ts({ code: "const n = 20 + 1; let seen = 0; n * 2" })).text).toEndWith("\n42");
  const wrong = await ts({ code: "seen = 1; const s: string = n" });
  expect(wrong.error).toBe(true);
  expect(wrong.text).toContain("Not run: the cell has a type error");
  expect(wrong.text).toContain("line 1: Type 'number' is not assignable to type 'string'");
  // Nothing of the refused cell ran.
  expect((await ts({ code: "seen" })).text).toEndWith("\n0");
  const unknown = await ts({ code: "await globb('*.ts')" });
  expect(unknown.text).toContain("Did you mean 'glob'");
  expect(unknown.text).toContain("help('glob')");
  expect((await ts({ code: "const x = JSON.parse('1') as any" })).text).toContain("`any` turns the type check off");
});

test("time is told: a refusal of a long run without a reason, the estimate in the header", async () => {
  const refused = await ts({ code: "1", expect_ms: 20 * 60_000 });
  expect(refused.error).toBe(true);
  expect(refused.text).toStartWith("Not run: you expect this cell to take 20m00s");
  expect((await ts({ code: "await Bun.sleep(150); 'slow'", expect_ms: 50 })).text).toMatch(/done 0\.1\ds · expected 0\.05s · 3\.\d× over\]/);
});

test("a cell past its limit is stopped for real: its sleep ends, the process it started is killed, the reply says so", async () => {
  const r = await ts({ code: "const child = Bun.spawn(['sh', '-c', 'sleep 30 & sleep 30']); await Bun.sleep(30_000); 'never'", expect_ms: 100, timeout_ms: 250 });
  expect(r.error).toBe(true);
  expect(r.text).toMatch(/^\[t\d+ stopped 0\.[2-9]\ds/);
  expect(r.text).toContain("it passed its limit of 0.25s");
  expect(r.text).toMatch(/Killed: pid \d+ \(sh -c sleep 30 & sleep 30\)/);
  expect(r.text).toContain("suspiciously slow");
  // The whole process group is gone, the grandchild too.
  const left = await ts({ code: "Bun.spawnSync(['ps', '-o', 'pid=', '-g', String(child.pid)]).stdout.toString().trim()" });
  expect(left.text).toMatch(/done [\d.]+s\]\n$/);
});

test("a backgrounded cell is cancelled by the tool's cancel input, and by cancel() from another cell", async () => {
  const bg = await ts({ code: "await Bun.sleep(60_000)", yield_ms: 100 });
  const id = /^\[(t\d+) running/.exec(bg.text)![1]!;
  expect(bg.text).toContain("backgrounded");
  const stopped = await ts({ cancel: id });
  expect(stopped.text).toStartWith(`[${id} stopped `);
  expect(stopped.text).toContain(`${id} stopped: cancelled.`);
  expect((await ts({ cancel: id })).text).toContain(`${id} is not running (it is stopped)`);
  const second = /^\[(t\d+) running/.exec((await ts({ code: "await fetch('http://10.255.255.1:9/never')", yield_ms: 100 })).text)![1]!;
  expect((await ts({ code: `await cancel('${second}')` })).text).toContain(`${second} stopped: cancelled by t`);
  expect((await ts({ code: `task('${second}').status` })).text).toEndWith("stopped");
});

test("code that ignores its signal is named, and the notice of a backgrounded cell rides on the next reply", async () => {
  const bg = await ts({ code: "await new Promise((r) => setTimeout(r, 1500)); 'late'", yield_ms: 50 });
  const id = /^\[(t\d+) running/.exec(bg.text)![1]!;
  const stopped = await ts({ cancel: id });
  expect(stopped.text).toContain("it ignores its signal");
  const quick = await ts({ code: "await Bun.sleep(150); 'bg done'", yield_ms: 20 });
  const quickId = /^\[(t\d+) running/.exec(quick.text)![1]!;
  await Bun.sleep(250);
  const next = await ts({ code: "1" });
  expect(next.text).toContain("# While you were away:");
  expect(next.text).toContain(`[${quickId} done `);
  expect(next.text).toContain("bg done");
});

test("an unawaited rejection is the cell's that caused it, in its reply; the kernel keeps serving", async () => {
  const r = await ts({ code: "void Promise.reject(new Error('dropped on the floor')); await Bun.sleep(50); 'went on'" });
  const id = /^\[(t\d+) done/.exec(r.text)![1]!;
  expect(r.text).toContain("went on");
  // The engine reports it when it gives up on a handler appearing: in the cell's own reply, or on the next one.
  const said = r.text + (await ts({ code: "0" })).text;
  expect(said).toContain(`[${id}] an unawaited promise rejected: Error: dropped on the floor\n    at <anonymous> (${id}.cell.ts:1:`);
  const timer = await ts({ code: "setTimeout(() => { throw new Error('from a timer') }, 10); await Bun.sleep(80); 'still here'" });
  expect(timer.text).toContain("a callback threw: Error: from a timer");
  expect((await ts({ code: "1 + 1" })).text).toEndWith("\n2");
});

test("a big value is summarized with the call that gives more", async () => {
  const r = await ts({ code: "Array.from({ length: 1000 }, (_, i) => ({ i, name: 'row ' + i, body: 'x'.repeat(2000) }))" });
  const id = /^\[(t\d+) /.exec(r.text)![1]!;
  expect(r.text.length).toBeLessThan(20_000);
  expect(r.text).toContain(`more('${id}', { rows: [`);
  expect((await ts({ code: `more('${id}', { rows: [998, 1000], fields: ['i', 'name'] })` })).text).toContain('[998, "row 998"]');
});

test("the shell refuses a plain read with the typed call; a program runs", async () => {
  const refused = await ts({ code: "await $`cat package.json`" });
  expect(refused.error).toBe(true);
  expect(refused.text).toContain("Not run: use await Bun.file('package.json').text()");
  expect((await ts({ code: "(await $`echo ran`).stdout.trim()" })).text).toEndWith("ran");
});

test("the kernel's socket is the same API: a cell run over it sees the names MCP cells bound", async () => {
  expect(existsSync(sock)).toBe(true);
  const a = await socketCall(sock, { op: "run", code: "n + 1" });
  expect(a).toMatchObject({ error: false, status: "done" });
  expect(a.text).toEndWith("\n22");
  expect((await socketCall(sock, { op: "describe" })).text).toContain("help()");
  expect(socketCall(sock, { op: "nope" } as never)).rejects.toThrow(/unknown op "nope"/);
});

test("the task list, logs and results outlive the kernel; names are made again by rerun", async () => {
  await ts({ code: "console.log('kept line'); const kept = 6 * 7; kept" });
  const before = (await ts({ code: "tasks().length" })).text.split("\n").at(-1)!;
  await client.close();
  client = await connect();
  const first = await ts({ code: "tasks().length" });
  expect(first.text).toStartWith("[kernel] this kernel is new: an earlier one ended.");
  expect(Number(first.text.split("\n").at(-1))).toBe(Number(before) + 1);
  const old = await ts({ code: "tasks().filter((t) => t.code.includes('kept line')).map((t) => t.id)[0]" });
  const id = old.text.split("\n").at(-1)!;
  expect((await ts({ code: `const r = task('${id}'); [r.status, r.logs[0], r.result]` })).text).toContain('[done, "kept line", "42"]');
  expect((await ts({ code: "kept" })).error).toBe(true);
  expect((await ts({ code: `await rerun('${id}')` })).text).toEndWith("\n42");
}, 20_000);

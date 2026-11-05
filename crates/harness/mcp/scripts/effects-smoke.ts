// The cell posts harnessd gets (repl.ts) and the steps in them (effects.ts): a fake harnessd on a private socket takes
// every repl_task, then each cell's last post is checked: its steps, their words and results, and its shown value.
// bun scripts/effects-smoke.ts
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const dir = mkdtempSync(`${tmpdir()}/effects-smoke-`);
process.env.HARNESS_SOCK = `${dir}/harnessd.sock`;
process.env.HARNESS_HOME = dir; // never the live ~/.harness
type Post = { id: string; call: string | null; status: string; effects: { kind: string; doing: string; done: string; status: string; detail: string; value: { t: string } | null }[]; value: { t: string; cols?: { name: string; type: string }[]; total?: number } | null };
const posts: Post[] = [];
Bun.listen({
  unix: process.env.HARNESS_SOCK,
  socket: {
    data(s, chunk) {
      // One small frame per connection: [type u8][len u32 BE][json].
      const len = new DataView(chunk.buffer, chunk.byteOffset).getUint32(1);
      const v = JSON.parse(new TextDecoder().decode(chunk.subarray(5, 5 + len)));
      if (v.op === "repl_task") posts.push(v);
      const out = new TextEncoder().encode('{"ok":true}');
      const f = new Uint8Array(5 + out.length);
      f[0] = 1; new DataView(f.buffer).setUint32(1, out.length); f.set(out, 5);
      s.write(f); s.end();
    },
  },
});

const { functions } = await import("../src/functions/index.ts");
const { Repl } = await import("../src/repl.ts");
const repl = new Repl(functions);
const cells: [string, string | null][] = [
  [`await Bun.write("${dir}/a.txt", "hello"); await Bun.file("${dir}/a.txt").text()`, "toolu_1"],
  [`import { readFileSync, writeFileSync, rmSync } from "node:fs"; writeFileSync("${dir}/b.txt", "xy"); readFileSync("${dir}/b.txt", "utf8"); rmSync("${dir}/b.txt"); 1`, null],
  [`await sh("echo out; echo err >&2; exit 3", { local: true })`, null],
  [`[{ name: "a", n: 1, url: "https://x.y" }, { name: "b", n: 2.5, url: null }]`, null],
  [`({ ok: true, count: 3 })`, null],
  [`throw new Error("boom")`, null],
  [`await fetch("http://127.0.0.1:1/").catch((e) => "refused")`, null],
];
for (const [code, call] of cells) await repl.run(code, undefined, call).done;
await Bun.sleep(300);

const last = (id: string) => posts.findLast((p) => p.id === id)!;
const fails: string[] = [];
const check = (what: string, ok: boolean, got: unknown) => { if (!ok) fails.push(`${what}: got ${JSON.stringify(got)}`); };
const words = (p: Post) => p.effects.map((e) => `${e.done} [${e.status}${e.detail ? ` ${e.detail}` : ""}]`);
for (const [i] of cells.entries()) console.log(`t${i + 1}`, JSON.stringify({ steps: words(last(`t${i + 1}`)), value: last(`t${i + 1}`).value }));

check("t1 call", last("t1").call === "toolu_1", last("t1").call);
check("t1 steps", words(last("t1")).join("|") === `Wrote ${dir}/a.txt [ok 5 B]|Read ${dir}/a.txt [ok 5 B]`, words(last("t1")));
check("t2 steps", last("t2").effects.map((e) => e.kind).join() === "write,read,delete", words(last("t2")));
const sh = last("t3").effects;
check("t3 one exec step (the local Bun.spawn inside sh is quiet)", sh.length === 1 && sh[0]!.kind === "exec" && sh[0]!.status === "error" && sh[0]!.detail === "exit 3", sh);
check("t3 output shown under the step", sh[0]?.value?.t === "text", sh[0]?.value);
check("t3 cell value not repeated", last("t3").value === null, last("t3").value);
const t4 = last("t4").value;
check("t4 table", t4?.t === "table" && t4.total === 2 && t4.cols?.map((c) => c.type).join() === "string,number,url", t4);
check("t5 record", last("t5").value?.t === "record", last("t5").value);
check("t6 error", last("t6").status === "error" && last("t6").value?.t === "error", last("t6"));
check("t7 fetch failed step", last("t7").effects[0]?.kind === "net" && last("t7").effects[0]?.status === "error", last("t7").effects);
check("running posts came first, the end last", posts.filter((p) => p.id === "t1").at(-1)!.status === "done", posts.filter((p) => p.id === "t1").map((p) => p.status));
rmSync(dir, { recursive: true, force: true });
if (fails.length) { console.error(`FAIL\n${fails.join("\n")}`); process.exit(1); }
console.log("ok");
process.exit(0);

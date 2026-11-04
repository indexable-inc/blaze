// End-to-end through one MCP session: backgrounded cells, tasks(), sh() with incremental reads, failures, ptree().
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const client = new Client({ name: "tasks-smoke", version: "0" });
await client.connect(new StdioClientTransport({ command: "bun", args: ["run", `${import.meta.dir}/../src/mcp.ts`] }));
const cells: [string, number?][] = [
  ["const x = 21; x * 2"],
  ["console.log('start'); await Bun.sleep(3000); console.log('end'); 'slept'", 1000],
  ["tasks()"],
  ["const p = sh('for i in 1 2 3; do echo $i; sleep 1; done'); await Bun.sleep(1200); out(p, { new: true })"],
  ["await Bun.sleep(2500); out(p, { new: true })"],
  ["await wait('t2')"],
  ["const f = await sh('echo oops >&2; ls /nope; exit 3'); [f.exit_code, f.stderr]"],
  ["const s = sh(['sh', '-c', 'sleep 5 & sleep 5; echo hi']); await Bun.sleep(300); await ptree()"],
  ["s.kill(); await s; procs()"],
  ["lines('p1', 'stdout', { tail: 2 })"],
  ["throw new Error('boom')"],
];
for (const [code, yield_ms] of cells) {
  const r = await client.callTool({ name: "ts", arguments: { code, ...(yield_ms ? { yield_ms } : {}) } });
  console.log(`> ${code}${r.isError ? "  (isError)" : ""}\n${(r.content as { text: string }[])[0]!.text}\n`);
}
await client.close();

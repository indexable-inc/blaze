// End-to-end: spawn the MCP server over stdio, list tools, run REPL cells including a Google search.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const client = new Client({ name: "smoke", version: "0" });
await client.connect(new StdioClientTransport({ command: "bun", args: ["run", `${import.meta.dir}/../src/mcp.ts`] }));
const { tools } = await client.listTools();
console.log("tools:", tools.map((t) => t.name));
const cells = [
  "const x: number = 21; function double(n: number) { return n * 2 }",
  "console.log('persisted', x); double(x)",
  "import { basename } from 'node:path'; basename('/a/b/c.ts')",
  "const results = await googleSearch('bun sqlite driver', { limit: 5 }); results.map(r => `${r.resolved ? '✓' : '~'} ${r.title} — ${r.url.slice(0, 80)}`)",
  "results.length",
  "throw new Error('boom')",
];
for (const code of cells) {
  const t = Date.now();
  const r = await client.callTool({ name: "ts", arguments: { code } });
  console.log(`\n> ${code}  (${Date.now() - t}ms${r.isError ? ", isError" : ""})\n${(r.content as { text: string }[])[0]!.text.split("\n").slice(0, 12).join("\n")}`);
}
await client.close();

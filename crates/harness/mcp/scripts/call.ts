// Run one or more cells through the MCP `ts` tool: bun scripts/call.ts '<code>' ['<code>' ...]
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const client = new Client({ name: "call", version: "0" });
await client.connect(new StdioClientTransport({ command: "bun", args: ["run", `${import.meta.dir}/../src/mcp.ts`], env: process.env as Record<string, string> }));
for (const code of Bun.argv.slice(2)) {
  const r = await client.callTool({ name: "ts", arguments: { code } });
  console.log(`> ${code}\n${(r.content as { text: string }[])[0]!.text}\n`);
}
await client.close();

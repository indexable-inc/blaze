// Checks the "harness browser not available" path end to end: with the daemon unreachable, googleSearch fails with
// CdpUnavailable naming the harness browser (embedded Chromium) and the daemon's error, and asks the person nothing,
// whether or not the client offers elicitation (there is no other browser to start and nothing to retry).
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const DEAD = `${process.env.TMPDIR ?? "/tmp"}/elicit-smoke-${process.pid}.sock`;

async function run(label: string, elicit: boolean) {
  const client = new Client({ name: "elicit-smoke", version: "0" }, { capabilities: elicit ? { elicitation: { form: {} } } : {} });
  let asked = 0;
  if (elicit) client.setRequestHandler(ElicitRequestSchema, async () => { asked++; return { action: "decline", content: {} }; });
  await client.connect(new StdioClientTransport({ command: "bun", args: ["run", `${import.meta.dir}/../src/mcp.ts`], env: { ...process.env, HARNESS_SOCK: DEAD } as Record<string, string> }));
  const r = await client.callTool({ name: "ts", arguments: { code: "(await googleSearch('mcp elicitation', { limit: 2 })).select('title')" } });
  const text = (r.content as { text: string }[])[0]!.text;
  await client.close();
  const ok = !!r.isError && text.includes("harness browser (embedded Chromium)") && asked === 0;
  console.log(`${label}: ${ok ? "ok" : "FAILED"} (isError=${!!r.isError}, asked=${asked})\n  ${text.split("\n").slice(0, 4).join("\n  ")}\n`);
  return ok;
}

const results = [await run("with elicitation", true), await run("without elicitation", false)];
if (!results.every(Boolean)) process.exit(1);
